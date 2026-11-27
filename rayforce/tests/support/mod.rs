//! Shared between the integration tests that drive a server. Every file under `tests/` is its
//! own crate, so anything two of them need lives here and is pulled in with
//! `mod support;`. Each test crate uses a different subset, hence the allow.
#![allow(dead_code)]

pub mod ipc;
pub mod wire;

/// A hook that takes every request it is handed as data and inserts it, which
/// a restricted request may not do on its own.
pub const RECORDING_HOOK: &str = "(fn [r] (insert 'seen (list (first r) (count r))))";

/// The table `RECORDING_HOOK` inserts into, with one placeholder row.
pub const SEEN: &str = "(set seen (table [head n] (list ['none] [0])))";
