//! A `Value` may outlive the `Runtime` that created it — and stay readable.
//!
//! Dropping the runtime unmaps the engine heap: `ray_runtime_destroy` munmaps
//! every pool without consulting any object's reference count. A handle that
//! survived that pointed at unmapped address space, and its `Drop` wrote a
//! refcount into it — a segfault reachable from safe code, usually surfacing at
//! process exit far from its cause.
//!
//! So a `Value` holds a handle on the heap itself, the reference count the core
//! does not have. A guard dropped while handles are out defers the unmap; the
//! last handle performs it.

use rayforce::{Runtime, TcpClient, Value};

#[test]
fn a_value_outliving_its_runtime_stays_readable() {
    let rt = Runtime::new().unwrap();
    let v = Value::i64(1);
    let cloned = v.clone();
    drop(rt);
    // No `unsafe` anywhere in this test. The heap is still mapped, so this is
    // an ordinary read, not a stale one.
    assert_eq!(v.as_i64().unwrap(), 1);
    assert_eq!(cloned.as_i64().unwrap(), 1);
    drop((v, cloned));
}

#[test]
fn cloning_a_value_after_its_guard_is_gone_still_retains() {
    // Under the old generation check, `Clone` had to skip `ray_retain` once the
    // runtime was gone. Now there is nothing to skip: the object is alive
    // because the heap is, so the refcount must move normally.
    let v = {
        let _rt = Runtime::new().unwrap();
        Value::i64(3)
    };
    let before = v.ref_count();
    let a = v.clone();
    assert_eq!(v.ref_count(), before + 1);
    let b = a.clone();
    assert_eq!(v.ref_count(), before + 2);
    assert_eq!(b.as_i64().unwrap(), 3);
    drop((v, a, b));
}

#[test]
fn containers_outliving_their_runtime_stay_readable() {
    let rt = Runtime::new().unwrap();
    let vals: Vec<Value> = (0..64).map(Value::i64).collect();
    let list = Value::list(&vals);
    let vec = Value::vec(&[1i64, 2, 3]);
    drop(rt);
    assert_eq!(list.len(), 64);
    assert_eq!(vec.as_slice::<i64>().unwrap(), &[1, 2, 3]);
    drop((vals, list, vec));
}

#[test]
fn a_straggler_pins_the_heap_and_the_next_runtime_waits() {
    // The case a bare is-a-runtime-live flag gets wrong: the stale handle would
    // release into the *next* runtime's heap, corrupting a live object. Now the
    // next runtime simply cannot start until the straggler is gone.
    let straggler = {
        let _rt = Runtime::new().unwrap();
        Value::i64(7)
    };
    let err = match Runtime::new() {
        Ok(_) => panic!("a second runtime must not start while a handle pins the heap"),
        Err(e) => e,
    };
    assert!(
        err.message.contains("still mapped"),
        "expected a pinned-heap error, got: {}",
        err.message
    );
    assert_eq!(straggler.as_i64().unwrap(), 7);

    // Last handle out: the heap is released and a new runtime can start.
    drop(straggler);
    let _rt2 = Runtime::new().unwrap();
    assert_eq!(Value::i64(9).as_i64().unwrap(), 9);
}

#[test]
fn a_null_value_does_not_pin_the_heap() {
    // `__ray_null` is a static in the C library, not a pool block — it survives
    // teardown on its own and must not keep the heap mapped for everyone else.
    let n = {
        let _rt = Runtime::new().unwrap();
        Value::null()
    };
    let rt2 = Runtime::new();
    assert!(
        rt2.is_ok(),
        "the null singleton must not pin the heap: {:?}",
        rt2.err()
    );
    assert!(n.is_null());
}

#[test]
fn value_is_one_pointer_wide() {
    // The generation tag is gone. With teardown deferred, only one heap is ever
    // mapped at a time, so every live handle provably belongs to it and there
    // is nothing left to disambiguate.
    assert_eq!(
        std::mem::size_of::<Value>(),
        std::mem::size_of::<*mut ()>(),
        "Value grew a field — did a liveness tag creep back?"
    );
}

#[test]
fn a_failed_connection_does_not_pin_the_heap() {
    {
        let _rt = Runtime::new().unwrap();
        // Nothing is listening on port 1; the connect must fail *without*
        // taking a heap handle, or a routine connection error would strand the
        // heap for the rest of the process.
        assert!(TcpClient::connect("127.0.0.1", 1, "", "").is_err());
    }
    let rt2 = Runtime::new();
    assert!(
        rt2.is_ok(),
        "a failed connect leaked a heap handle: {:?}",
        rt2.err()
    );
}
