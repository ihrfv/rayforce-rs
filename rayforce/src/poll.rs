//! The rayforce event loop.
//!
//! [`Poll`] is the core's `ray_poll_t` — the same loop the REPL and native IPC
//! use. On its own it does nothing interesting; it exists so a *subscription*
//! can hand it a connected socket and have every inbound frame read and
//! dispatched. See [`crate::q::Subscription`].
//!
//! There is one poll per runtime, owned by the runtime and torn down with it.
//! [`Poll::install`] returns a handle to it, creating it on first use, so a
//! [`crate::TcpClient`] and a subscription never fight over it.
//!
//! ```no_run
//! use rayforce::{Poll, Runtime};
//! let _rt = Runtime::new().unwrap();
//! let poll = Poll::install().unwrap();
//! poll.run_for(200).unwrap();   // one 200 ms slice
//! ```

use std::ffi::c_void;
use std::marker::PhantomData;

use rayforce_sys as sys;

use crate::error::{RayError, Result};

/// The runtime's event loop.
///
/// `!Send`/`!Sync` like the rest of the crate: every frame this loop reads is
/// decoded into engine objects, so it must run on the thread that owns the
/// [`crate::Runtime`].
///
/// # Ownership and teardown
///
/// The poll belongs to the [`crate::Runtime`], not to this handle: closing a
/// selector releases engine objects held for it, so the loop has to be torn
/// down while the heap is still alive, which is what `Runtime`'s `Drop` does.
/// A `Poll` is therefore just a borrow of it, and any number of them may exist
/// at once — [`crate::TcpClient`] installs one the same way.
///
/// A `Poll` left alive past its runtime is inert rather than dangerous. Unlike
/// a [`crate::Value`] it takes **no handle on the engine heap**, deliberately:
/// it borrows a loop the runtime owns and tears down, so pinning the heap would
/// keep a selector alive that nothing is left to close. Every call therefore
/// checks the runtime is live first and returns an error rather than touching a
/// destroyed loop. The check is safe to trust because it happens before the
/// pointer is dereferenced, and nothing can drop the runtime in between — this
/// is all one thread.
pub struct Poll {
    ptr: *mut sys::ray_poll_t,
    _not_send: PhantomData<*mut ()>,
}

impl Poll {
    /// Return the runtime's poll, creating and installing one if it has none.
    ///
    /// Requires a live [`crate::Runtime`]. Calling it any number of times is
    /// safe: every call after the first returns a handle to the same loop, so
    /// it composes with [`crate::TcpClient`], whose `connect` installs one the
    /// same way. Since the runtime owns the loop, no handle destroys it.
    pub fn install() -> Result<Poll> {
        if !crate::runtime::is_live() {
            return Err(RayError::binding(
                "Poll::install requires a live rayforce runtime",
            ));
        }
        unsafe {
            let existing = sys::ray_runtime_get_poll();
            if !existing.is_null() {
                return Ok(Poll {
                    ptr: existing.cast(),
                    _not_send: PhantomData,
                });
            }
            let ptr = sys::ray_poll_create();
            if ptr.is_null() {
                return Err(RayError::binding("ray_poll_create failed"));
            }
            // The public header types `ray_poll_create` as `ray_poll_t*` while
            // `ray_runtime_set_poll` still takes `void*`, hence the cast.
            sys::ray_runtime_set_poll(ptr.cast());
            Ok(Poll {
                ptr,
                _not_send: PhantomData,
            })
        }
    }

    /// Is this handle still bound to a live runtime?
    ///
    /// False as soon as the [`crate::Runtime`] guard is dropped, which is when
    /// the loop this points at is torn down.
    #[inline]
    pub fn is_current(&self) -> bool {
        crate::runtime::is_live()
    }

    #[inline]
    fn check_current(&self) -> Result<()> {
        if self.is_current() {
            Ok(())
        } else {
            Err(RayError::binding(
                "this Poll outlived the Runtime that installed it",
            ))
        }
    }

    /// Run the loop for at most `timeout_ms`, then return.
    ///
    /// Slicing rather than blocking forever is what makes a disconnect
    /// observable: a peer going away is not an error, it is
    /// [`crate::q::Subscription::is_alive`] turning false, and something has to
    /// come back from the loop to check it.
    ///
    /// A negative timeout blocks until [`Poll::exit`] is called.
    pub fn run_for(&self, timeout_ms: i32) -> Result<i64> {
        self.check_current()?;
        let rc = unsafe { sys::ray_poll_run_for(self.ptr, timeout_ms) };
        if rc < 0 {
            return Err(RayError::binding("ray_poll_run_for failed"));
        }
        Ok(rc)
    }

    /// Stop a blocking [`Poll::run_for`]. A no-op on a retired handle.
    pub fn exit(&self, code: i64) {
        if self.is_current() {
            unsafe { sys::ray_poll_exit(self.ptr, code) };
        }
    }

    /// Raw handle, for the `q_conn_*` calls in [`crate::q`].
    pub(crate) fn as_ptr(&self) -> *mut sys::ray_poll_t {
        debug_assert!(
            self.is_current(),
            "rayforce: this Poll outlived the Runtime that installed it"
        );
        self.ptr
    }

    /// Does selector `id` still resolve, and is it still *the same connection*?
    ///
    /// The rx machine deregisters a selector when its peer closes or its frame
    /// stream goes bad, so this going false *is* the disconnect signal. The
    /// `data` compare is what makes it a disconnect signal rather than a slot
    /// probe: `ray_poll_register` hands out the first free index, so an id is
    /// reused the moment its connection goes away. `q_conn_send` guards itself
    /// the same way (q_server.c: `sel->data != cd`).
    pub(crate) fn holds(&self, id: i64, data: *mut c_void) -> bool {
        if !self.is_current() {
            return false;
        }
        let sel = unsafe { sys::ray_poll_get(self.ptr, id) };
        !sel.is_null() && unsafe { (*sel).data } == data
    }

    /// The `data` pointer the rx machine holds for selector `id`, if it
    /// resolves. This is the per-connection state `q_conn_attach` allocated,
    /// and doubles as that connection's identity.
    pub(crate) fn selector_data(&self, id: i64) -> Option<*mut c_void> {
        if !self.is_current() {
            return None;
        }
        let sel = unsafe { sys::ray_poll_get(self.ptr, id) };
        if sel.is_null() {
            None
        } else {
            Some(unsafe { (*sel).data })
        }
    }
}
