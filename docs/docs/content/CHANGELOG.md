# :material-history: Changelog

All notable changes to `rayforce` are documented here. This project adheres to
[Semantic Versioning](https://semver.org).

## Unreleased

### Added

- **Q subscriptions.** A `QConnection` can now be handed to the event loop with
  [`attach`](documentation/ipc.md), turning it into a `Subscription` that
  receives frames the peer pushes unsolicited — a tickerplant or a dict-form
  publisher. The plain client could not do this: it is blocking
  request/response, so a pushed frame would be read as the answer to the next
  call.

  New `Poll` (the runtime's event loop), `Subscription`
  (`send` / `execute` / `is_alive`), and `env::bind_vary` / `env::bind_unary`
  for binding a Rust handler under the name a publisher calls. A handler is a
  type implementing `env::VaryFn` / `env::UnaryFn`; the generated trampoline
  borrows the arguments and catches panics, so the whole surface is safe.

- **`q::encode`** — the mirror of `q::decode_response`: turn a `Value` into a
  complete Q wire message for a transport you own. Together they let you write
  a Q *publisher*, not just a client.

- **`Value::attrs`** — the attribute byte. Rarely needed, but it is the only
  way to tell a keyed table (a 2-element list carrying `RAY_ATTR_DICT`) from a
  plain list, which no type code distinguishes.

- **CI runs the suite against a debug-flavour engine.** Set
  `RAYFORCE_CORE_DEBUG=1` and `rayforce-sys` builds `librayforce.a` with
  `-DDEBUG`, which compiles in the core's invariant checks and its stale
  retain/release detector; arm it at runtime with `RAY_DFD=1`. This is the only
  tool that sees a use-after-free inside the engine's `mmap`-backed pool
  allocator — AddressSanitizer and Valgrind track `malloc`, which the engine
  never calls, and Miri cannot execute the C library at all. The `test` job now
  runs both flavours; the debug leg reproduces the `Value`-outliving-`Runtime`
  crash below on the commit before its fix.

- **The IPC tests run in CI.** `tests/ipc.rs` drives `TcpClient` against a
  spawned server and was returning early for want of one — which reports as a
  pass, so the gap was invisible. CI now builds the server binary, and
  `RAYFORCE_REQUIRE_SERVER=1` turns a missing one into a failure rather than a
  skip. `tests/q_real.rs` still opts out via `RAYFORCE_Q_ADDR`: it needs a real
  `q` server, which cannot be provisioned on a runner.

### Changed

- `rayforce-sys` now compiles `rayforce-q`'s `q_server.c` alongside `q.c`, and
  its pinned sources move to core **v2.5.15** and rayforce-q **2.1.0**. 2.1.0 is
  a hard floor — the `q_conn_*` API does not exist in 2.0.0.

- **`Runtime::new` refuses while a previous heap is still pinned.** Handles that
  outlive their runtime keep its heap mapped, and the core permits exactly one
  at a time — so a new runtime has to wait for them. The error names how many
  are outstanding. Previously this combination was absorbed silently and left a
  live grenade in the caller's hands.

- **A `Runtime` guard is required for everything except reading and dropping
  handles you already hold.** `eval`, `set_global`, `get_global`, the value
  constructors and the connection constructors all answer to the same
  `is_live()`. So a `Value` whose runtime is gone can still be read and dropped,
  and a `TcpClient` whose runtime is gone can still be closed — but neither can
  be used to do new work.

### Fixed

- **A `Value` outliving its `Runtime` no longer crashes — and stays readable.**
  Two things are shared across the FFI boundary and only one was counted.
  `ray_t.rc` counts references to an *object*, and the crate tracked it
  correctly. Nothing counted references to the *heap* those objects live in,
  which is the one that matters: `ray_runtime_destroy` never consults `rc`, it
  munmaps every pool outright, so a surviving handle pointed into unmapped
  address space rather than at freed bytes. Values now hold a handle on the heap
  itself; a runtime dropped while handles are out defers the unmap, and the last
  handle performs it. Reading such a value is no longer a bug — the mapping is
  still there — and `Value` is back to one pointer wide.

- **The connection types pin the heap their close reaches into.** `TcpClient`
  and `QConnection` had no liveness tracking of any kind, so a client outliving
  its `Runtime` called `ray_ipc_close` / `q_close` against an unmapped heap.
  Both now hold a heap handle for their lifetime, and both take it only on the
  success path — a refused connection must not strand the heap.

- **Building a value requires a live `Runtime`.** `Value::i64(1)` with no runtime
  was safe Rust calling straight into the engine with no check at all. It did not
  crash, which is why it went unnoticed: `ray_alloc` lazily maps a heap when none
  exists, so the value landed in an orphan one. The sharp case was symbols, which
  are runtime-scoped — `Value::sym("hello")` returned an *empty* symbol, dropping
  the string with no error anywhere.

- **The runtime tears down its event loop.** `TcpClient::connect` installs a
  poll on first use and `ray_runtime_destroy` does not touch it, so it leaked.
  `Runtime`'s `Drop` now takes it down first, while the heap it releases
  selector state into is still there.

- **`QConnection` is `!Send`/`!Sync`**, like every other handle in the crate.
  It was a bare file descriptor, so it inferred both, while `execute` interns
  symbols and builds engine objects that belong to the runtime's thread.

- Building with `--no-default-features` (no `chrono`) is now warning-free.


## 1.0.1

### Added

- **Decode Q wire messages from an external transport.** New
  [`q::decode_response`](documentation/ipc.md) turns a complete Q IPC message
  (8-byte wire header + body, compressed or not) into a `Value`. This lets
  socket I/O live in a separate transport thread that owns a plain `TcpStream`
  and just moves bytes, while deserialization into engine objects stays on the
  thread that owns the `Runtime`. Q server-side errors surface as `Err`.


## 1.0.0

Initial release of the Rust bindings for RayforceDB v2.

### Added

- **Value model.** A single reference-counted [`Value`](documentation/data-types/values.md)
  handle (`Clone` = retain, `Drop` = release) covering all atom types — bool,
  `u8`, `i16`/`i32`/`i64`, `f32`/`f64`, symbol, string, date, time, timestamp,
  and GUID — plus typed nulls.
- **Containers.** Vectors with zero-copy `as_slice::<T>()` reads, lists, and
  dictionaries.
- **Tables.** [`Table::new`](documentation/table/overview.md) from typed columns, column/row
  accessors, `head`/`tail`/`take`, and inner/left/asof joins.
- **Query DSL.** A fluent builder over `select` and `update` with `col(..)`
  expressions, arithmetic operator overloads, comparison and aggregation
  methods, filtering, grouping (`by`), and ordering.
- **CSV & splayed I/O.** `read_csv` / `write_csv`, plus `save_splayed`,
  `load_splayed`, and `load_parted` for on-disk columnar data.
- **Serialization.** [`Value::serialize`](documentation/serialization.md) /
  `Value::deserialize` round-trips using RayforceDB's native wire format.
- **Conversions.** `ToValue` / `FromValue` for native Rust types and an optional
  `chrono` feature (default) for temporal interop.
- **IPC client.** [`TcpClient`](documentation/ipc.md) to connect to a running RayforceDB
  server, `execute` queries, and `send` / `send_async` values.

### Notes

- A single live `Runtime` per process; `Value`, `Table`, and `TcpClient` are
  `!Send`/`!Sync`.
- An embedded IPC server, window joins, pivots, and feature-gated
  dataframe/SQL plugins are planned for future releases.
