use log::{error, trace, warn};
use std::convert::TryFrom;
use std::ffi::c_void;
use std::fmt;
use std::mem;
use std::rc::Rc;

use crate::convert::{FromMrb, TryFromMrb};
use crate::exception::{LastError, MrbExceptionHandler};
use crate::gc::MrbGarbageCollection;
use crate::interpreter::Mrb;
use crate::sys;
use crate::MrbError;

pub mod types;

/// Max argument count for function calls including initialize.
///
/// Defined in `vm.c`.
pub const MRB_FUNCALL_ARGC_MAX: usize = 16;

struct ProtectArgs {
    slf: sys::mrb_value,
    func: String,
    args: Vec<sys::mrb_value>,
}

struct ProtectArgsWithBlock {
    slf: sys::mrb_value,
    func: String,
    args: Vec<sys::mrb_value>,
    block: sys::mrb_value,
}

impl ProtectArgs {
    fn new(slf: sys::mrb_value, func: &str, args: Vec<sys::mrb_value>) -> Self {
        Self {
            slf,
            func: func.to_owned(),
            args,
        }
    }

    fn with_block(self, block: sys::mrb_value) -> ProtectArgsWithBlock {
        ProtectArgsWithBlock {
            slf: self.slf,
            func: self.func,
            args: self.args,
            block: block,
        }
    }
}

#[allow(clippy::module_name_repetitions)]
pub trait ValueLike
where
    Self: Sized,
{
    fn inner(&self) -> sys::mrb_value;

    fn interp(&self) -> &Mrb;

    fn funcall<T, M, A>(&self, func: M, args: A) -> Result<T, MrbError>
    where
        T: TryFromMrb<Value, From = types::Ruby, To = types::Rust>,
        M: AsRef<str>,
        A: AsRef<[Value]>,
    {
        unsafe extern "C" fn run_protected(
            mrb: *mut sys::mrb_state,
            data: sys::mrb_value,
        ) -> sys::mrb_value {
            let ptr = sys::mrb_sys_cptr_ptr(data);
            let args = mem::transmute::<*const c_void, *const ProtectArgs>(ptr);
            let args = Rc::from_raw(args);

            let sym = sys::mrb_intern(mrb, args.func.as_ptr() as *const i8, args.func.len());
            let value = sys::mrb_funcall_argv(
                mrb,
                args.slf,
                sym,
                // This will always unwrap because we've already checked that we
                // have fewer than `MRB_FUNCALL_ARGC_MAX` args, which is less
                // than i64 max value.
                i64::try_from(args.args.len()).unwrap_or_default(),
                args.args.as_ptr(),
            );
            sys::mrb_sys_raise_current_exception(mrb);
            value
        }
        // Ensure the borrow is out of scope by the time we eval code since
        // Rust-backed files and types may need to mutably borrow the `Mrb` to
        // get access to the underlying `MrbState`.
        let (mrb, _ctx) = {
            let borrow = self.interp().borrow();
            (borrow.mrb, borrow.ctx)
        };

        let _arena = self.interp().create_arena_savepoint();

        let args = args.as_ref().iter().map(Value::inner).collect::<Vec<_>>();
        if args.len() > MRB_FUNCALL_ARGC_MAX {
            warn!(
                "Too many args supplied to funcall: given {}, max {}.",
                args.len(),
                MRB_FUNCALL_ARGC_MAX
            );
            return Err(MrbError::TooManyArgs {
                given: args.len(),
                max: MRB_FUNCALL_ARGC_MAX,
            });
        }
        trace!(
            "Calling {}#{} with {} args",
            types::Ruby::from(self.inner()),
            func.as_ref(),
            args.len()
        );
        let args = Rc::new(ProtectArgs::new(self.inner(), func.as_ref(), args));
        let value = unsafe {
            let data = sys::mrb_sys_cptr_value(mrb, Rc::into_raw(args) as *mut c_void);
            let mut state = mem::uninitialized::<u8>();

            let value = sys::mrb_protect(mrb, Some(run_protected), data, &mut state as *mut u8);
            if state != 0 {
                (*mrb).exc = sys::mrb_sys_obj_ptr(value);
            }
            value
        };
        let value = Value::new(self.interp(), value);

        match self.interp().last_error() {
            LastError::Some(exception) => {
                warn!("runtime error with exception backtrace: {}", exception);
                Err(MrbError::Exec(exception.to_string()))
            }
            LastError::UnableToExtract(err) => {
                error!("failed to extract exception after runtime error: {}", err);
                Err(err)
            }
            LastError::None if value.is_unreachable() => {
                // Unreachable values are internal to the mruby interpreter and
                // interacting with them via the C API is unspecified and may
                // result in a segfault.
                //
                // See: https://github.com/mruby/mruby/issues/4460
                Err(MrbError::UnreachableValue(value.inner().tt))
            }
            LastError::None => unsafe {
                T::try_from_mrb(self.interp(), value).map_err(MrbError::ConvertToRust)
            },
        }
    }

    fn funcall_with_block<T, M, A>(&self, func: M, args: A, block: Value) -> Result<T, MrbError>
    where
        T: TryFromMrb<Value, From = types::Ruby, To = types::Rust>,
        M: AsRef<str>,
        A: AsRef<[Value]>,
    {
        unsafe extern "C" fn run_protected(
            mrb: *mut sys::mrb_state,
            data: sys::mrb_value,
        ) -> sys::mrb_value {
            let ptr = sys::mrb_sys_cptr_ptr(data);
            let args = mem::transmute::<*const c_void, *const ProtectArgsWithBlock>(ptr);
            let args = Rc::from_raw(args);

            let sym = sys::mrb_intern(mrb, args.func.as_ptr() as *const i8, args.func.len());
            let value = sys::mrb_funcall_with_block(
                mrb,
                args.slf,
                sym,
                // This will always unwrap because we've already checked that we
                // have fewer than `MRB_FUNCALL_ARGC_MAX` args, which is less
                // than i64 max value.
                i64::try_from(args.args.len()).unwrap_or_default(),
                args.args.as_ptr(),
                args.block,
            );
            sys::mrb_sys_raise_current_exception(mrb);
            value
        }
        // Ensure the borrow is out of scope by the time we eval code since
        // Rust-backed files and types may need to mutably borrow the `Mrb` to
        // get access to the underlying `MrbState`.
        let (mrb, _ctx) = {
            let borrow = self.interp().borrow();
            (borrow.mrb, borrow.ctx)
        };

        let _arena = self.interp().create_arena_savepoint();

        let args = args.as_ref().iter().map(Value::inner).collect::<Vec<_>>();
        if args.len() > MRB_FUNCALL_ARGC_MAX {
            warn!(
                "Too many args supplied to funcall_with_block: given {}, max {}.",
                args.len(),
                MRB_FUNCALL_ARGC_MAX
            );
            return Err(MrbError::TooManyArgs {
                given: args.len(),
                max: MRB_FUNCALL_ARGC_MAX,
            });
        }
        trace!(
            "Calling {}#{} with {} args and block",
            types::Ruby::from(self.inner()),
            func.as_ref(),
            args.len()
        );
        let args =
            Rc::new(ProtectArgs::new(self.inner(), func.as_ref(), args).with_block(block.inner()));
        let value = unsafe {
            let data = sys::mrb_sys_cptr_value(mrb, Rc::into_raw(args) as *mut c_void);
            let mut state = mem::uninitialized::<u8>();

            let value = sys::mrb_protect(mrb, Some(run_protected), data, &mut state as *mut u8);
            if state != 0 {
                (*mrb).exc = sys::mrb_sys_obj_ptr(value);
            }
            value
        };
        let value = Value::new(self.interp(), value);

        match self.interp().last_error() {
            LastError::Some(exception) => {
                warn!("runtime error with exception backtrace: {}", exception);
                Err(MrbError::Exec(exception.to_string()))
            }
            LastError::UnableToExtract(err) => {
                error!("failed to extract exception after runtime error: {}", err);
                Err(err)
            }
            LastError::None if value.is_unreachable() => {
                // Unreachable values are internal to the mruby interpreter and
                // interacting with them via the C API is unspecified and may
                // result in a segfault.
                //
                // See: https://github.com/mruby/mruby/issues/4460
                Err(MrbError::UnreachableValue(value.inner().tt))
            }
            LastError::None => unsafe {
                T::try_from_mrb(self.interp(), value).map_err(MrbError::ConvertToRust)
            },
        }
    }

    fn respond_to<T: AsRef<str>>(&self, method: T) -> Result<bool, MrbError> {
        let sym = Value::from_mrb(self.interp(), method.as_ref())
            .funcall::<Value, _, _>("to_sym", &[])?;
        self.funcall::<bool, _, _>("respond_to?", &[sym])
    }
}

pub struct Value {
    interp: Mrb,
    value: sys::mrb_value,
}

impl Value {
    pub fn new(interp: &Mrb, value: sys::mrb_value) -> Self {
        Self {
            interp: Rc::clone(interp),
            value,
        }
    }

    pub fn inner(&self) -> sys::mrb_value {
        self.value
    }

    pub fn ruby_type(&self) -> types::Ruby {
        types::Ruby::from(self.value)
    }

    /// Some type tags like [`MRB_TT_UNDEF`](sys::mrb_vtype::MRB_TT_UNDEF) are
    /// internal to the mruby VM and manipulating them with the [`sys`] API is
    /// unspecified and may result in a segfault.
    ///
    /// After extracting a [`sys::mrb_value`] from the interpreter, check to see
    /// if the value is [unreachable](types::Ruby::Unreachable) and propagate an
    /// [`MrbError::UnreachableValue`](crate::MrbError::UnreachableValue) error.
    ///
    /// See: <https://github.com/mruby/mruby/issues/4460>
    pub fn is_unreachable(&self) -> bool {
        self.ruby_type() == types::Ruby::Unreachable
    }

    pub fn is_dead(&self) -> bool {
        unsafe { sys::mrb_sys_value_is_dead(self.interp.borrow().mrb, self.value) }
    }

    pub fn to_s(&self) -> String {
        self.funcall::<String, _, _>("to_s", &[])
            .unwrap_or_else(|_| "<unknown>".to_owned())
    }

    pub fn to_s_debug(&self) -> String {
        format!("{}<{}>", self.ruby_type().class_name(), self.inspect())
    }

    pub fn inspect(&self) -> String {
        self.funcall::<String, _, _>("inspect", &[])
            .unwrap_or_else(|_| "<unknown>".to_owned())
    }

    /// Consume `self` and try to convert `self` to type `T`.
    ///
    /// If you do not want to consume this [`Value`], use [`Value::itself`].
    pub fn try_into<T>(self) -> Result<T, MrbError>
    where
        T: TryFromMrb<Self, From = types::Ruby, To = types::Rust>,
    {
        let interp = Rc::clone(&self.interp);
        unsafe { T::try_from_mrb(&interp, self) }.map_err(MrbError::ConvertToRust)
    }

    /// Call `#itself` on this [`Value`] and try to convert the result to type
    /// `T`.
    ///
    /// If you want to consume this [`Value`], use [`Value::try_into`].
    pub fn itself<T>(self) -> Result<T, MrbError>
    where
        T: TryFromMrb<Self, From = types::Ruby, To = types::Rust>,
    {
        self.funcall::<T, _, _>("itself", &[])
    }
}

impl ValueLike for Value {
    fn inner(&self) -> sys::mrb_value {
        self.value
    }

    fn interp(&self) -> &Mrb {
        &self.interp
    }
}

impl FromMrb<Value> for Value {
    type From = types::Ruby;
    type To = types::Rust;

    fn from_mrb(_interp: &Mrb, value: Self) -> Self {
        value
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.to_s())
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.to_s_debug())
    }
}

#[cfg(test)]
mod tests {
    use crate::eval::MrbEval;
    use crate::gc::MrbGarbageCollection;
    use crate::interpreter::{Interpreter, MrbApi};
    use crate::value::ValueLike;
    use crate::MrbError;

    #[test]
    fn to_s_true() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.bool(true);
        let string = value.to_s();
        assert_eq!(string, "true");
    }

    #[test]
    fn debug_true() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.bool(true);
        let debug = value.to_s_debug();
        assert_eq!(debug, "Boolean<true>");
    }

    #[test]
    fn inspect_true() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.bool(true);
        let debug = value.inspect();
        assert_eq!(debug, "true");
    }

    #[test]
    fn to_s_false() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.bool(false);
        let string = value.to_s();
        assert_eq!(string, "false");
    }

    #[test]
    fn debug_false() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.bool(false);
        let debug = value.to_s_debug();
        assert_eq!(debug, "Boolean<false>");
    }

    #[test]
    fn inspect_false() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.bool(false);
        let debug = value.inspect();
        assert_eq!(debug, "false");
    }

    #[test]
    fn to_s_nil() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.nil();
        let string = value.to_s();
        assert_eq!(string, "");
    }

    #[test]
    fn debug_nil() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.nil();
        let debug = value.to_s_debug();
        assert_eq!(debug, "NilClass<nil>");
    }

    #[test]
    fn inspect_nil() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.nil();
        let debug = value.inspect();
        assert_eq!(debug, "nil");
    }

    #[test]
    fn to_s_fixnum() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.fixnum(255);
        let string = value.to_s();
        assert_eq!(string, "255");
    }

    #[test]
    fn debug_fixnum() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.fixnum(255);
        let debug = value.to_s_debug();
        assert_eq!(debug, "Fixnum<255>");
    }

    #[test]
    fn inspect_fixnum() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.fixnum(255);
        let debug = value.inspect();
        assert_eq!(debug, "255");
    }

    #[test]
    fn to_s_string() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.string("interstate");
        let string = value.to_s();
        assert_eq!(string, "interstate");
    }

    #[test]
    fn debug_string() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.string("interstate");
        let debug = value.to_s_debug();
        assert_eq!(debug, r#"String<"interstate">"#);
    }

    #[test]
    fn inspect_string() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.string("interstate");
        let debug = value.inspect();
        assert_eq!(debug, r#""interstate""#);
    }

    #[test]
    fn to_s_empty_string() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.string("");
        let string = value.to_s();
        assert_eq!(string, "");
    }

    #[test]
    fn debug_empty_string() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.string("");
        let debug = value.to_s_debug();
        assert_eq!(debug, r#"String<"">"#);
    }

    #[test]
    fn inspect_empty_string() {
        let interp = Interpreter::create().expect("mrb init");

        let value = interp.string("");
        let debug = value.inspect();
        assert_eq!(debug, r#""""#);
    }

    #[test]
    fn is_dead() {
        let interp = Interpreter::create().expect("mrb init");
        let arena = interp.create_arena_savepoint();
        let live = interp.eval("'dead'").expect("value");
        assert!(!live.is_dead());
        let dead = live;
        let live = interp.eval("'live'").expect("value");
        arena.restore();
        interp.full_gc();
        // unreachable objects are dead after a full garbage collection
        assert!(dead.is_dead());
        // the result of the most recent eval is always live even after a full
        // garbage collection
        assert!(!live.is_dead());
    }

    #[test]
    fn immediate_is_dead() {
        let interp = Interpreter::create().expect("mrb init");
        let arena = interp.create_arena_savepoint();
        let live = interp.eval("27").expect("value");
        assert!(!live.is_dead());
        let immediate = live;
        let live = interp.eval("64").expect("value");
        arena.restore();
        interp.full_gc();
        // immediate objects are never dead
        assert!(!immediate.is_dead());
        // the result of the most recent eval is always live even after a full
        // garbage collection
        assert!(!live.is_dead());
        // Fixnums are immediate even if they are created directly without an
        // interpreter.
        let fixnum = interp.fixnum(99);
        assert!(!fixnum.is_dead());
    }

    #[test]
    fn funcall() {
        let interp = Interpreter::create().expect("mrb init");
        let nil = interp.nil();
        assert!(nil.funcall::<bool, _, _>("nil?", &[]).expect("nil?"));
        let s = interp.string("foo");
        assert!(!s.funcall::<bool, _, _>("nil?", &[]).expect("nil?"));
        let delim = interp.string("");
        let split = s
            .funcall::<Vec<String>, _, _>("split", &[delim])
            .expect("split");
        assert_eq!(split, vec!["f".to_owned(), "o".to_owned(), "o".to_owned()])
    }

    #[test]
    fn funcall_different_types() {
        let interp = Interpreter::create().expect("mrb init");
        let nil = interp.nil();
        let s = interp.string("foo");
        let eql = nil.funcall::<bool, _, _>("==", &[s]);
        assert_eq!(eql, Ok(false));
    }

    #[test]
    fn funcall_type_error() {
        let interp = Interpreter::create().expect("mrb init");
        let nil = interp.nil();
        let s = interp.string("foo");
        let result = s.funcall::<String, _, _>("+", &[nil]);
        assert_eq!(
            result,
            Err(MrbError::Exec("TypeError: expected String".to_owned()))
        );
    }

    #[test]
    fn funcall_method_not_exists() {
        let interp = Interpreter::create().expect("mrb init");
        let nil = interp.nil();
        let s = interp.string("foo");
        let result = nil.funcall::<bool, _, _>("garbage_method_name", &[s]);
        assert_eq!(
            result,
            Err(MrbError::Exec(
                "NoMethodError: undefined method 'garbage_method_name'".to_owned()
            ))
        );
    }
}
