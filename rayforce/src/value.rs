//! [`Value`] — the safe, reference-counted handle to a core `ray_t` object.
//!
//! `Value` owns exactly one reference. `Clone` retains, `Drop` releases (both
//! no-ops for the null singleton and error objects, per the core). It is
//! deliberately `!Send`/`!Sync`: the core is single-threaded with a
//! thread-local VM, so values must never cross threads.

use crate::error::{self, Result};
use crate::raw::{self, Raw};
use crate::runtime::{acquire_handle, release_handle};
use rayforce_sys as sys;
use std::fmt;
use std::marker::PhantomData;

/// Does this handle pin the engine heap?
///
/// Everything a `Value` can wrap lives in the engine's mmap'd pool except one
/// thing: [`Value::null`] wraps `__ray_null` (see `raw::null_obj`), a static in
/// the C library rather than a pool block. It stays mapped for the process
/// lifetime and is safe to read after teardown, so it must not keep the heap
/// alive. The core draws the same line — `ray_retain` and `ray_release` are
/// both no-ops for it.
///
/// Anything unrecognised counts, deliberately. Over-counting keeps the heap
/// mapped slightly longer than necessary; under-counting unmaps it while a
/// handle still points in.
#[inline]
fn pins_heap(ptr: Raw) -> bool {
    !raw::is_null_singleton(ptr)
}

/// A handle to a RayforceDB object (atom, vector, list, dict, table, …).
///
/// # Outliving the runtime
///
/// A `Value` points into the engine heap, and dropping the [`crate::Runtime`]
/// unmaps that heap — `ray_runtime_destroy` munmaps every pool without
/// consulting any object's reference count, so a surviving handle would point
/// at unmapped address space, not at freed bytes.
///
/// So a `Value` holds a handle on the heap itself, the reference count the core
/// does not have. A guard dropped while handles are out defers the unmap, and
/// the last handle performs it. Such a value stays fully readable in the
/// meantime; there is no stale-read case to guard against.
///
/// # Safety
///
/// `!Send`/`!Sync`, and must stay so: the handle count is `Relaxed`, which is
/// sound only while every handle is taken and given back on one thread.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<rayforce::Value>();
/// ```
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<rayforce::Value>();
/// ```
/// Control — `compile_fail` passes on *any* build failure, a rename included:
/// ```
/// fn assert_exists<T>() {}
/// assert_exists::<rayforce::Value>();
/// ```
pub struct Value {
    ptr: Raw,
    /// Makes `Value` `!Send` + `!Sync`.
    _not_send: PhantomData<*mut ()>,
}

impl Value {
    /// Wrap a pointer whose reference this `Value` now owns (the common case for
    /// values returned by core constructors and `eval`).
    ///
    /// # Safety
    /// `ptr` must be a valid, non-error core pointer carrying a reference that
    /// is transferred to the returned `Value`.
    pub(crate) unsafe fn from_owned(ptr: Raw) -> Value {
        debug_assert!(!ptr.is_null(), "from_owned: null pointer");
        // Defensive: value-null must be RAY_NULL_OBJ, never a bare C NULL.
        // Normalize so a stray NULL can never reach `ray_release` on Drop.
        let ptr = if ptr.is_null() { raw::null_obj() } else { ptr };
        if pins_heap(ptr) {
            acquire_handle();
        }
        Value {
            ptr,
            _not_send: PhantomData,
        }
    }

    /// Wrap a borrowed pointer, taking a new reference (`ray_retain`). Use for
    /// pointers the core still owns (e.g. `ray_list_get`, `ray_dict_keys`).
    ///
    /// # Safety
    /// `ptr` must be a valid core pointer.
    pub(crate) unsafe fn from_borrowed(ptr: Raw) -> Value {
        sys::ray_retain(ptr);
        if pins_heap(ptr) {
            acquire_handle();
        }
        Value {
            ptr,
            _not_send: PhantomData,
        }
    }

    /// The value's attribute byte.
    ///
    /// Rarely needed — the typed accessors cover the normal cases. The
    /// exception is telling a *keyed table* apart from a plain list: it decodes
    /// as a 2-element list carrying `RAY_ATTR_DICT`, which no type code
    /// distinguishes.
    pub fn attrs(&self) -> u8 {
        unsafe { raw::attrs(self.as_ptr()) }
    }

    /// The untyped null singleton (`RAY_NULL_OBJ`).
    ///
    /// Takes no heap handle: `__ray_null` is a static in the C library, not a
    /// pool block, so it neither needs the heap kept alive nor is affected by
    /// its teardown. Retain and release are no-ops for it in the core too.
    pub fn null() -> Value {
        Value {
            ptr: raw::null_obj(),
            _not_send: PhantomData,
        }
    }

    /// Borrow the underlying raw pointer (does not transfer ownership).
    ///
    /// Needs no liveness check: holding this `Value` is what keeps the heap it
    /// points into mapped.
    #[inline]
    pub(crate) fn as_ptr(&self) -> Raw {
        self.ptr
    }

    /// Consume `self`, returning the raw pointer and its reference (no release).
    #[inline]
    pub(crate) fn into_raw(self) -> Raw {
        let p = self.ptr;
        let pinned = pins_heap(p);
        std::mem::forget(self);
        if pinned {
            // The reference moves to the engine, which keeps the object inside
            // the heap — our handle on the mapping goes with it. This can never
            // be the last handle out: every caller is a constructor, and those
            // assert a live runtime, which means a guard is still held.
            release_handle();
        }
        p
    }

    /// Replace the held pointer after a C call that already **consumed** our old
    /// reference (copy-on-write move semantics) and returned `new` as an owned
    /// reference. The old pointer is not released here.
    #[inline]
    pub(crate) unsafe fn replace_ptr_consumed(&mut self, new: Raw) {
        self.ptr = new;
    }

    /// The signed type tag (negative = atom, positive = vector, 0 = list, …).
    #[inline]
    pub fn type_code(&self) -> i8 {
        unsafe { raw::type_code(self.as_ptr()) }
    }

    /// `|type|` — the canonical (unsigned) type id.
    #[inline]
    pub fn abs_type(&self) -> i8 {
        unsafe { raw::abs_type(self.as_ptr()) }
    }

    /// True for atoms (scalars and function objects).
    #[inline]
    pub fn is_atom(&self) -> bool {
        unsafe { raw::is_atom(self.as_ptr()) }
    }

    /// True for homogeneous vectors (bool…str).
    #[inline]
    pub fn is_vec(&self) -> bool {
        unsafe { raw::is_vec(self.as_ptr()) }
    }

    /// True if this is the null singleton.
    #[inline]
    pub fn is_null(&self) -> bool {
        raw::is_null_singleton(self.as_ptr())
    }

    /// Element / pair count for vectors, lists, and dicts; for other objects the
    /// raw `len` field (not meaningful for atoms).
    #[inline]
    pub fn len_raw(&self) -> i64 {
        unsafe { raw::len(self.as_ptr()) }
    }

    /// Current core reference count (diagnostic).
    #[inline]
    pub fn ref_count(&self) -> u32 {
        unsafe { raw::rc(self.as_ptr()) }
    }

    /// Pretty-print via the core formatter (`ray_fmt`).
    pub fn format(&self) -> String {
        unsafe {
            let s = sys::ray_fmt(self.as_ptr(), 1);
            if s.is_null() {
                return String::new();
            }
            // ray_fmt can return an error object; don't read it as a string
            // (and ray_release is a no-op on errors, so free explicitly).
            if raw::is_err(s) {
                sys::ray_error_free(s);
                return String::new();
            }
            let p = sys::ray_str_ptr(s).cast::<u8>();
            let n = sys::ray_str_len(s);
            let out = if p.is_null() || n == 0 {
                String::new()
            } else {
                String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned()
            };
            sys::ray_release(s);
            out
        }
    }

    /// Serialize to a byte vector (core wire format with IPC header).
    pub fn serialize(&self) -> Result<Vec<u8>> {
        unsafe {
            let ser = error::check(sys::ray_ser(self.as_ptr()))?;
            if ser.is_null() {
                return Err(crate::error::RayError::binding("serialize returned null"));
            }
            let v = Value::from_owned(ser);
            let p = raw::data(v.ptr) as *const u8;
            let n = raw::len(v.ptr) as usize;
            Ok(std::slice::from_raw_parts(p, n).to_vec())
        }
    }

    /// Deserialize a value from bytes previously produced by [`Value::serialize`]
    /// (or another Rayforce wire-format encoder).
    pub fn deserialize(bytes: &[u8]) -> Result<Value> {
        // Wrap the bytes in a U8 vector for ray_de.
        let buf = Value::vec(bytes);
        unsafe {
            let de = error::check(sys::ray_de(buf.ptr))?;
            if de.is_null() {
                return Err(crate::error::RayError::binding("deserialize returned null"));
            }
            Ok(Value::from_owned(error::materialize(de)?))
        }
    }
}

impl Clone for Value {
    fn clone(&self) -> Value {
        unsafe { sys::ray_retain(self.ptr) };
        // A second handle on the same object is a second handle on the heap.
        if pins_heap(self.ptr) {
            acquire_handle();
        }
        Value {
            ptr: self.ptr,
            _not_send: PhantomData,
        }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        // Decide this *before* the release: if that drops the object's last
        // reference the block is freed, and the pointer stops being readable.
        let pinned = pins_heap(self.ptr);
        unsafe { sys::ray_release(self.ptr) }
        if pinned {
            // May unmap the heap, when this is the last handle and the guard is
            // already gone. Nothing touches `self.ptr` past this point.
            release_handle();
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format())
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Value(type={}, {})", self.type_code(), self.format())
    }
}
