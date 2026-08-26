# :material-cog-outline: Technical Details

This page explains how `rayforce` is put together: the crate layout, the value
handle, the threading model, and how results are materialized.

## :material-layers-outline: A two-crate workspace

The repository is a Cargo workspace with two crates:

| Crate | Purpose |
|---|---|
| `rayforce-sys` | Raw `unsafe` FFI. Runs [`bindgen`](https://github.com/rust-lang/rust-bindgen) over the core's `rayforce.h`, builds the RayforceDB core (`make lib`), and statically links `librayforce.a`. |
| `rayforce` | The safe, ergonomic API you depend on — values, tables, the query DSL, IPC, and serialization. |

You depend only on `rayforce`. The `rayforce-sys` crate exists to isolate the
generated bindings and the link step; the safe layer is a thin wrapper over it.

## :material-tag-multiple-outline: `Value` — a refcounted RAII handle

Everything the engine holds — an integer atom, a float vector, a dictionary, a
whole table — is a [`Value`](../documentation/data-types/values.md). A `Value` is
a thin handle onto memory owned by the engine, with reference-counted lifetime
managed by Rust's RAII:

- **`Clone`** retains: it bumps the engine refcount and hands back another handle
  to the same payload. Cloning never copies data.
- **`Drop`** releases: it decrements the refcount; the engine frees the payload
  when the last handle goes away.

```rust
use rayforce::{Runtime, Value};
let _rt = Runtime::new()?;

let a = Value::vec(&[1i64, 2, 3]);
let b = a.clone();   // same payload, refcount += 1
drop(b);             // refcount -= 1; `a` still valid
assert_eq!(a.as_slice::<i64>()?, &[1, 2, 3]);
# Ok::<(), rayforce::RayError>(())
```

Because lifetime is handled for you, there is no manual `free` and no
use-after-free: `Drop` keeps handles honest.

There is a second reference count you never see. The one above counts handles
to an *object*; the engine also needs to know when its *heap* can be unmapped,
and `ray_runtime_destroy` unmaps it without consulting any object's refcount.
So every `Value` also holds a handle on the heap, which is why a value can
safely outlive the `Runtime` that made it — see below.

## :material-cpu-64-bit: Single runtime, single thread, `!Send`

The RayforceDB core runs on a single thread with a thread-local VM and permits
**one live `Runtime` per process**. To make this safe in Rust:

- `Runtime::new()` returns an RAII guard. Hold it for as long as you do any
  work. Dropping it unmaps the engine heap — but only once nothing points into
  it, so a value that outlives the guard defers the unmap rather than dangling.
- `Value`, `Table`, `TcpClient` and `QConnection` are **`!Send`** and
  **`!Sync`**. They cannot be moved or shared across threads, which statically
  prevents you from touching the engine from a thread other than the one that
  owns the runtime.

```rust
use rayforce::{Runtime, Value};

let _rt = Runtime::new()?;   // start here, in every runtime-dependent program
let v = Value::i64(42);
// `v` stays on this thread — it is !Send by design.
# Ok::<(), rayforce::RayError>(())
```

### What a dropped `Runtime` still allows

One rule: a live guard is required for everything **except reading and dropping
handles you already hold**.

```rust
use rayforce::{Runtime, Value};

let v = {
    let _rt = Runtime::new()?;
    Value::i64(1)
};
assert_eq!(v.as_i64()?, 1);   // fine: the heap is still mapped, `v` pins it
drop(v);                      // last handle out — the heap is unmapped here
# Ok::<(), rayforce::RayError>(())
```

Calling `eval` or a constructor without a guard panics rather than working
against a runtime nobody owns, and `Runtime::new()` returns an error while a
previous heap is still pinned, naming how many handles are outstanding.

!!! note "Why single-thread?"
    The engine's VM state is thread-local. Rather than hide this behind locks,
    the bindings surface it directly: `!Send`/`!Sync` turns a runtime invariant
    into a compile-time guarantee, and the cost of crossing a thread boundary
    is simply never paid.

## :material-lightning-bolt-outline: Lazy result materialization

Some operations — aggregations in particular — produce results lazily inside the
engine. The safe API always materializes them for you before handing back a
`Value`: by the time `execute()` or `eval()` returns, you hold a concrete value.
You never observe an unmaterialized result.

## :material-content-copy: Zero-copy reads

Reading a numeric vector back into Rust is zero-copy. `value.as_slice::<T>()`
returns a `&[T]` that borrows directly from engine-owned memory — no per-element
conversion, no intermediate `Vec`:

```rust
use rayforce::{Runtime, Value};
let _rt = Runtime::new()?;

let prices = Value::vec(&[100.0f64, 200.0, 110.0]);
let slice: &[f64] = prices.as_slice()?;   // borrows engine memory, no copy
assert_eq!(slice, &[100.0, 200.0, 110.0]);
# Ok::<(), rayforce::RayError>(())
```

The same applies to the typed slice readers for temporal and boolean columns
(`date_days_slice`, `time_millis_slice`, `timestamp_nanos_slice`, `bool_slice`).

## :material-arrow-right: Next steps

- [:octicons-database-16: Data Types](../documentation/data-types/overview.md) —
  the full value model.
- [:material-file-document: Documentation overview](../documentation/overview.md).
