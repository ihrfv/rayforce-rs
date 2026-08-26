//! Runtime lifecycle and evaluation entry points.
//!
//! The core permits exactly one live runtime per process and pins it to the
//! creating thread (thread-local VM). [`Runtime`] is an RAII guard enforcing the
//! single-live-instance rule; hold one for as long as you use the API. Drop it
//! to tear the runtime down.

use crate::error::{check, materialize, RayError, Result};
use crate::value::Value;
use rayforce_sys as sys;
use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// Is an engine heap mapped? True from `ray_runtime_create` until the pools are
/// unmapped — which, under deferred teardown, can be long after the [`Runtime`]
/// guard is dropped.
static LIVE: AtomicBool = AtomicBool::new(false);

/// The heap outlives its guard, so the runtime pointer has to live somewhere
/// that outlives it too.
static RT: AtomicPtr<sys::ray_runtime_s> = AtomicPtr::new(ptr::null_mut());

/// The heap-level reference count the core does not have.
///
/// `ray_t.rc` counts references to a single *object*. This counts live Rust
/// handles into the *heap* those objects live in — a different thing, and the
/// one nothing else tracks. `ray_runtime_destroy` never consults `rc`: it
/// munmaps every pool outright, so an object with `rc == 5` is unmapped exactly
/// like one with `rc == 1`. A handle surviving that points into unmapped
/// address space, which no check can make safe — only not unmapping can.
///
/// # Safety
///
/// `Relaxed` is sound only because every type that takes a handle is
/// `!Send`/`!Sync`, so all of this traffic happens on the thread that owns the
/// runtime and observes its own writes by program order. The one cross-thread
/// case — a later runtime on another thread — is published by `LIVE`'s `SeqCst`
/// pair, which is why [`teardown`] stores it last. Each handle-carrying type
/// pins its markers with a `compile_fail` doctest.
static HANDLES: AtomicUsize = AtomicUsize::new(0);

/// Set when a [`Runtime`] guard is dropped while handles are still out. The heap
/// stays mapped — those handles remain readable — but there is no guard for it,
/// so nothing new may be evaluated or allocated in it.
static PENDING: AtomicBool = AtomicBool::new(false);

/// Take a handle on the engine heap, keeping it mapped.
#[inline]
pub(crate) fn acquire_handle() {
    HANDLES.fetch_add(1, Ordering::Relaxed);
}

/// Give a handle back. If it was the last one and the guard is already gone,
/// perform the teardown the guard deferred.
#[inline]
pub(crate) fn release_handle() {
    if HANDLES.fetch_sub(1, Ordering::Relaxed) == 1 && PENDING.load(Ordering::Relaxed) {
        teardown();
    }
}

/// Unmap the heap. Reached either from `Runtime::drop` with no handles out, or
/// from the last handle's `Drop` once the guard is gone.
fn teardown() {
    unsafe {
        // The poll belongs to the runtime, and closing a selector releases
        // engine objects held for it — so it has to go down first, while the
        // heap is still there. `ray_runtime_destroy` does not do this itself,
        // so a process that only ever used a `TcpClient` leaked it.
        let poll = sys::ray_runtime_get_poll();
        if !poll.is_null() {
            sys::ray_runtime_set_poll(ptr::null_mut());
            sys::ray_poll_destroy(poll.cast());
        }
        let rt = RT.swap(ptr::null_mut(), Ordering::Relaxed);
        if !rt.is_null() {
            sys::ray_runtime_destroy(rt);
        }
    }
    PENDING.store(false, Ordering::Relaxed);
    // Last, and SeqCst: this is what publishes the teardown to a later runtime
    // on another thread. See `HANDLES`.
    LIVE.store(false, Ordering::SeqCst);
}

/// An owned, live RayforceDB runtime. Only one may exist per process at a time.
///
/// All evaluation and object construction must happen on the thread that created
/// the runtime; `Runtime` is `!Send`/`!Sync` to enforce this.
///
/// # Values that outlive it
///
/// Dropping the runtime unmaps the engine heap — but only once nothing points
/// into it. Every live [`Value`] holds a handle on that heap, so a guard dropped
/// while handles are out defers the unmap and the last handle performs it. Such
/// a value stays fully readable:
///
/// ```
/// # use rayforce::{Runtime, Value};
/// let rt = Runtime::new().unwrap();
/// let v = Value::i64(1);
/// drop(rt);                            // heap stays mapped: `v` points into it
/// assert_eq!(v.as_i64().unwrap(), 1);  // still valid, not a stale read
/// drop(v);                             // last handle out — unmapped here
/// ```
///
/// What you cannot do is *evaluate* without a guard: [`eval`] and the value
/// constructors require a live `Runtime`, and [`Runtime::new`] refuses while a
/// previous heap is still pinned by stragglers.
///
/// # Safety
///
/// `!Send`/`!Sync`, and must stay so: `HANDLES`' `Relaxed` traffic is sound only
/// while every handle is taken and given back on the runtime thread.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<rayforce::Runtime>();
/// ```
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<rayforce::Runtime>();
/// ```
/// Control — `compile_fail` passes on *any* build failure, a rename included:
/// ```
/// fn assert_exists<T>() {}
/// assert_exists::<rayforce::Runtime>();
/// ```
pub struct Runtime {
    _not_send: PhantomData<*mut ()>,
}

impl Runtime {
    /// Create the process runtime.
    ///
    /// Errors if one is already live, or if a previous runtime's heap is still
    /// mapped because handles outlived it — the core permits exactly one heap at
    /// a time, so those handles have to go first.
    pub fn new() -> Result<Runtime> {
        if LIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(if PENDING.load(Ordering::Relaxed) {
                RayError::binding(format!(
                    "the previous runtime's heap is still mapped: {} handle(s) outlive it — \
                     drop them to release it",
                    HANDLES.load(Ordering::Relaxed)
                ))
            } else {
                RayError::binding("a rayforce runtime is already live in this process")
            });
        }
        let rt = unsafe { sys::ray_runtime_create(0, ptr::null_mut()) };
        if rt.is_null() {
            LIVE.store(false, Ordering::SeqCst);
            return Err(RayError::binding("ray_runtime_create failed"));
        }
        RT.store(rt, Ordering::Relaxed);
        Ok(Runtime {
            _not_send: PhantomData,
        })
    }

    /// Evaluate a Rayfall source string against the global environment.
    pub fn eval(&self, source: &str) -> Result<Value> {
        eval(source)
    }

    /// Bind `value` to the global name `name` (the core retains it). The value
    /// then resolves in evaluated expressions and queries.
    pub fn set_global(&self, name: &str, value: &Value) -> Result<()> {
        set_global(name, value)
    }

    /// Look up a global binding by name.
    pub fn get_global(&self, name: &str) -> Result<Value> {
        get_global(name)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if HANDLES.load(Ordering::Relaxed) == 0 {
            teardown();
        } else {
            // Handles still point into the mapping. Unmapping now would leave
            // them dangling into unmapped address space, so the last one out
            // does it instead. See `HANDLES`.
            PENDING.store(true, Ordering::Relaxed);
        }
    }
}

/// Is there a runtime that can be called into?
///
/// False both when none was ever created and when a [`Runtime`] guard was
/// dropped while handles outlived it: the heap stays mapped so those handles
/// remain readable, but without a guard nothing new may be evaluated in it.
pub fn is_live() -> bool {
    LIVE.load(Ordering::SeqCst) && !PENDING.load(Ordering::Relaxed)
}

/// Panic unless a runtime is live.
///
/// The engine has no guard of its own: `ray_eval` and the atom constructors
/// dereference a thread-local VM that is null with no live runtime. This is an
/// unconditional assertion, not a `debug_assert` — the release build has exactly
/// the same hole.
#[inline]
pub(crate) fn assert_live(what: &str) {
    assert!(is_live(), "rayforce: {what} requires a live Runtime");
}

/// Bind `value` to a global name. Requires a live [`Runtime`].
pub fn set_global(name: &str, value: &Value) -> Result<()> {
    assert_live("set_global");
    unsafe {
        let id = sys::ray_sym_intern(name.as_ptr() as *const _, name.len());
        let e = sys::ray_env_set(id, value.as_ptr());
        if e != sys::ray_err_t_RAY_OK {
            return Err(RayError::binding(format!(
                "set_global({name}) failed (err {e})"
            )));
        }
    }
    Ok(())
}

/// Look up a global binding. Requires a live [`Runtime`].
pub fn get_global(name: &str) -> Result<Value> {
    assert_live("get_global");
    unsafe {
        let id = sys::ray_sym_intern(name.as_ptr() as *const _, name.len());
        let v = sys::ray_env_get(id);
        if v.is_null() {
            return Err(RayError::binding(format!("global not found: {name}")));
        }
        if crate::raw::is_err(v) {
            return Err(RayError::from_obj(v));
        }
        Ok(Value::from_borrowed(v))
    }
}

/// Evaluate a Rayfall source string. Requires a live [`Runtime`].
///
/// A void / null result becomes [`Value::null`]; a core error becomes `Err`.
pub fn eval(source: &str) -> Result<Value> {
    assert_live("eval");
    let c = CString::new(source).map_err(|_| RayError::binding("source contains a NUL byte"))?;
    unsafe {
        let r = sys::ray_eval_str(c.as_ptr());
        if r.is_null() {
            return Ok(Value::null());
        }
        Ok(Value::from_owned(materialize(check(r)?)?))
    }
}

/// Evaluate an already-compiled AST [`Value`] (e.g. a query). Requires a live
/// [`Runtime`].
pub fn eval_value(obj: &Value) -> Result<Value> {
    assert_live("eval_value");
    unsafe {
        let r = sys::ray_eval(obj.as_ptr());
        if r.is_null() {
            return Ok(Value::null());
        }
        Ok(Value::from_owned(materialize(check(r)?)?))
    }
}
