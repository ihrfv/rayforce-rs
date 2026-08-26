/* bindgen entry point for rayforce-sys.
 *
 * Pulls in the public v2 API, then hand-declares the handful of internal
 * core symbols the safe crate needs (all present in librayforce.a but not in
 * the public header — see PLAN.md). Signatures copied verbatim from the core
 * sources: src/lang/eval.h, src/lang/internal.h, src/store/serde.h. */

#include <rayforce.h>

#ifdef __cplusplus
extern "C" {
#endif

/* src/lang/eval.h — evaluate an already-compiled AST object */
ray_t* ray_eval(ray_t* obj);

/* src/lang/internal.h — query builtins (variadic arg-array form) */
ray_t* ray_update_fn(ray_t** args, int64_t n);
ray_t* ray_insert_fn(ray_t** args, int64_t n);
ray_t* ray_upsert_fn(ray_t** args, int64_t n);

/* src/lang/internal.h — CSV + on-disk table I/O */
ray_t* ray_read_csv_fn(ray_t** args, int64_t n);
ray_t* ray_write_csv_fn(ray_t** args, int64_t n);
ray_t* ray_set_splayed_fn(ray_t** args, int64_t n);
ray_t* ray_get_splayed_fn(ray_t** args, int64_t n);
ray_t* ray_get_parted_fn(ray_t** args, int64_t n);

/* src/ops/ops.h — materialize a lazy DAG result (no-op for non-lazy inputs;
 * consumes the lazy reference on success). RAY_LAZY (104) is the deferred type
 * returned by ray_eval and graph-aware builtins. */
ray_t* ray_lazy_materialize(ray_t* val);

/* src/store/serde.h — serialize / deserialize to a U8 vector with IPC header */
ray_t* ray_ser(ray_t* obj);
ray_t* ray_de(ray_t* bytes);

/* src/core/runtime.c — last per-VM error message (set alongside a RAY_ERROR) */
const char* ray_error_msg(void);

/* ===== Poll-driven subscriptions =====
 *
 * The pieces a subscriber needs that the public header still does not export.
 * Signatures copied verbatim from the core sources, as above. */

/* src/core/poll.h — the selector API, included rather than hand-declared so the
 * `ray_selector` layout comes from the core itself. `ray_poll_get` returns NULL
 * once the rx machine has deregistered a selector, which is how a peer
 * disconnect is detected: there is no error, it simply stops resolving. Its
 * `data` field is the per-connection state, and doubles as the connection's
 * identity — selector ids are reused as soon as a slot frees up. */
#include "core/poll.h"

/* src/lang/eval.h:55,57 — the native function-pointer shapes. `vary` is the
 * one a q push handler wants: a dict-form publisher calls `upd` with one
 * argument and a kdb+ tickerplant with two, so a fixed arity drops one of
 * them. */
typedef ray_t* (*ray_unary_fn)(ray_t*);
typedef ray_t* (*ray_vary_fn)(ray_t**, int64_t);

/* src/lang/env.h:32,34 — wrap a native function as a callable ray_t. */
ray_t* ray_fn_unary(const char* name, uint8_t fn_attrs, ray_unary_fn fn);
ray_t* ray_fn_vary(const char* name, uint8_t fn_attrs, ray_vary_fn fn);

/* src/lang/env.h:57,64 — bind it into the global environment so a pushed frame
 * naming it dispatches there. Both binders are needed: ray_env_bind walks the
 * dotted-segment dicts, while rayforce-q's inbound symbol lookup hits the FLAT
 * binding. rayforce-q's own embed/rayforce_q.c:174-179 calls both. */
ray_err_t ray_env_bind(int64_t sym_id, ray_t* val);
ray_err_t ray_env_bind_flat(int64_t sym_id, ray_t* val);

/* The poll object the IPC client needs (ray_poll_create / ray_runtime_get_poll
 * / ray_runtime_set_poll) is exported by the public header as of core v2.5.8,
 * so it is no longer hand-declared here. ray_poll_create now returns the typed
 * `ray_poll_t*` rather than the old `void*` — see the cast in
 * rayforce/src/ipc.rs::ensure_poll. */

#ifdef __cplusplus
}
#endif
