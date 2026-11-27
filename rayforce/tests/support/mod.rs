//! Shared between the integration tests that drive a server. Every file under `tests/` is its
//! own crate, so anything two of them need lives here and is pulled in with
//! `mod support;`. Each test crate uses a different subset, hence the allow.
#![allow(dead_code)]

pub mod ipc;
pub mod wire;

use rayforce::{Poll, Runtime};

/// A hook that takes every request it is handed as data and inserts it, which
/// a restricted request may not do on its own.
pub const RECORDING_HOOK: &str = "(fn [r] (insert 'seen (list (first r) (count r))))";

/// The statement that creates `seen`, the table `RECORDING_HOOK` inserts
/// into, with one placeholder row.
pub const CREATE_SEEN: &str = "(set seen (table [head n] (list ['none] [0])))";

/// Install `RECORDING_HOOK` as the `.q.on` sync hook, answering 42 once it
/// has recorded the request, so a test can tell the hook's reply from any
/// other.
pub fn install_recording_sync_hook(rt: &Runtime) -> rayforce::Result<()> {
    rt.eval(&format!(
        "(.q.on 'sync (fn [r] (do ({RECORDING_HOOK} r) 42)))"
    ))
    .map(drop)
}

/// Pump the loop until `f` yields or the budget runs out.
pub fn pump_until<T>(poll: &Poll, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..100 {
        if let Some(v) = f() {
            return Some(v);
        }
        poll.run_for(20).unwrap();
    }
    f()
}
