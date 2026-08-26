//! Build script for `rayforce-sys`.
//!
//! 1. Locates the RayforceDB v2 core + `rayforce-q` source trees (see
//!    [`core_src_dir`] / [`q_src_dir`] for the resolution order). When neither
//!    an env override nor a local dev checkout is present — the common case for
//!    a crate downloaded from crates.io — the pinned release tags are shallow
//!    cloned from GitHub into `OUT_DIR`.
//! 2. Builds the static library `librayforce.a` via the core's `make lib`
//!    (incremental — a no-op when objects are up to date).
//! 3. Emits the static link directives.
//! 4. Generates Rust bindings from `wrapper.h` with `bindgen`.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned upstream sources cloned when no local checkout is available. Bump
/// these in lockstep with a `rayforce-sys` release. Override at build time with
/// the `RAYFORCE_REPO` / `RAYFORCE_REF` (and `_Q_` variants) env vars.
const RAYFORCE_REPO: &str = "https://github.com/RayforceDB/rayforce.git";
const RAYFORCE_REF: &str = "v2.5.15";
const RAYFORCE_Q_REPO: &str = "https://github.com/RayforceDB/rayforce-q.git";
const RAYFORCE_Q_REF: &str = "2.1.0";

fn main() {
    let core = core_src_dir();
    let include = core.join("include");
    let header = include.join("rayforce.h");

    assert!(
        header.exists(),
        "rayforce core header not found at {}.\n\
         Set RAYFORCE_SRC to your rayforce checkout (default: ~/rayforce).",
        header.display()
    );

    sanitize_libclang_path();
    build_core_lib(&core);

    // --- Q IPC client + server (rayforce-q's q.c / q_server.c) ---
    // Linked BEFORE librayforce so their undefined `ray_*` symbols resolve from
    // the core archive. Both need the core's private `src/` on the include path:
    // `q.c` for `table/sym.h`, `q_server.c` for `core/poll.h` and `lang/env.h`.
    //
    // `q_server.c` is what makes a *subscription* possible. `q.c`'s client is
    // blocking request/response, so a frame the peer pushes unsolicited would be
    // read as the answer to the next `q_send`. `q_conn_attach` hands the fd to a
    // rayforce poll instead, which reads every frame and routes it by message
    // type. Requires rayforce-q >= 2.1.0.
    let q_src = q_src_dir();
    let q_c = q_src.join("q.c");
    let q_server_c = q_src.join("q_server.c");
    assert!(
        q_c.exists(),
        "rayforce-q client not found at {}.\n\
         Set RAYFORCE_Q_SRC to your rayforce-q checkout (default: ~/rayforce-q).",
        q_c.display()
    );
    assert!(
        q_server_c.exists(),
        "rayforce-q server not found at {}.\n\
         It ships with rayforce-q >= 2.1.0; older checkouts have only q.c.",
        q_server_c.display()
    );
    cc::Build::new()
        .file(&q_c)
        .file(&q_server_c)
        .include(&q_src)
        .include(&include)
        .include(core.join("src"))
        .warnings(false)
        .compile("rayforce_q");
    for f in ["q.c", "q.h", "q_server.c", "q_server.h"] {
        println!("cargo:rerun-if-changed={}", q_src.join(f).display());
    }
    println!("cargo:rerun-if-env-changed=RAYFORCE_Q_SRC");

    // --- linking ---
    println!("cargo:rustc-link-search=native={}", core.display());
    println!("cargo:rustc-link-lib=static=rayforce");
    println!("cargo:rustc-link-lib=dylib=m");
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=dylib=pthread");
    }
    // Expose the core dir to downstream crates (e.g. for symfile fixtures).
    println!("cargo:root={}", core.display());

    // --- bindgen ---
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include.display()))
        // wrapper.h includes `core/poll.h` for the real `ray_selector` layout,
        // which lives in the core's private `src/` rather than its public
        // include dir.
        .clang_arg(format!("-I{}", core.join("src").display()))
        // Keep the surface tight and deterministic.
        .allowlist_function("ray_.*")
        .allowlist_type("ray_.*")
        .allowlist_var("RAY_.*")
        .allowlist_var("NULL_.*")
        .allowlist_var("__ray_.*")
        .allowlist_var("ray_type_sizes")
        // ray_t is a union with a flexible array member + nested anon structs;
        // let bindgen represent it faithfully.
        .layout_tests(true)
        .derive_debug(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("failed to generate rayforce bindings");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("failed to write bindings.rs");

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RAYFORCE_SRC");
    println!("cargo:rerun-if-changed={}", header.display());
    // Relink when the core archive changes (e.g. the C core was rebuilt).
    // `make lib` is incremental, so this doesn't cause perpetual rebuilds.
    // For a guaranteed pickup after editing core sources, touch build.rs or
    // `cargo clean -p rayforce-sys`.
    println!(
        "cargo:rerun-if-changed={}",
        core.join("librayforce.a").display()
    );
}

/// Resolve the RayforceDB core source tree, in order of precedence:
/// 1. `RAYFORCE_SRC` — an explicit checkout (used by CI and local overrides).
/// 2. `~/rayforce` — a developer's local clone, if it looks like the core.
/// 3. A shallow clone of [`RAYFORCE_REF`] into `OUT_DIR` (crates.io consumers).
fn core_src_dir() -> PathBuf {
    println!("cargo:rerun-if-env-changed=RAYFORCE_SRC");
    if let Ok(p) = env::var("RAYFORCE_SRC") {
        return PathBuf::from(p);
    }
    if let Ok(home) = env::var("HOME") {
        let local = Path::new(&home).join("rayforce");
        if local.join("include/rayforce.h").exists() {
            return local;
        }
    }
    clone_pinned(
        "rayforce",
        "RAYFORCE_REPO",
        RAYFORCE_REPO,
        "RAYFORCE_REF",
        RAYFORCE_REF,
    )
}

/// Resolve the `rayforce-q` source tree; same precedence as [`core_src_dir`],
/// keyed off `RAYFORCE_Q_SRC` / `~/rayforce-q` / a clone of [`RAYFORCE_Q_REF`].
fn q_src_dir() -> PathBuf {
    println!("cargo:rerun-if-env-changed=RAYFORCE_Q_SRC");
    if let Ok(p) = env::var("RAYFORCE_Q_SRC") {
        return PathBuf::from(p);
    }
    if let Ok(home) = env::var("HOME") {
        let local = Path::new(&home).join("rayforce-q");
        if local.join("q.c").exists() {
            return local;
        }
    }
    clone_pinned(
        "rayforce-q",
        "RAYFORCE_Q_REPO",
        RAYFORCE_Q_REPO,
        "RAYFORCE_Q_REF",
        RAYFORCE_Q_REF,
    )
}

/// Shallow-clone `repo` at `git_ref` into `OUT_DIR/<name>` and return the path.
/// Reuses an existing clone (`OUT_DIR` persists across incremental rebuilds) so
/// repeated builds don't re-hit the network. The repo URL and ref can be
/// overridden via the given env vars for testing against unreleased cores.
fn clone_pinned(
    name: &str,
    repo_env: &str,
    repo_default: &str,
    ref_env: &str,
    ref_default: &str,
) -> PathBuf {
    println!("cargo:rerun-if-env-changed={repo_env}");
    println!("cargo:rerun-if-env-changed={ref_env}");
    let repo = env::var(repo_env).unwrap_or_else(|_| repo_default.to_string());
    let git_ref = env::var(ref_env).unwrap_or_else(|_| ref_default.to_string());

    let dst = PathBuf::from(env::var("OUT_DIR").unwrap()).join(name);
    if dst.join(".git").exists() {
        return dst;
    }

    eprintln!(
        "rayforce-sys: cloning {name} {git_ref} from {repo} into {}",
        dst.display()
    );
    let status = Command::new("git")
        .args(["clone", "--depth", "1", "--branch", &git_ref, &repo])
        .arg(&dst)
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke `git clone` for {name}: {e}"));
    assert!(
        status.success(),
        "`git clone --branch {git_ref} {repo}` failed (exit {:?}).\n\
         Provide a local checkout via {}_SRC to build offline.",
        status.code(),
        if name == "rayforce-q" {
            "RAYFORCE_Q"
        } else {
            "RAYFORCE"
        },
    );
    dst
}

fn sanitize_libclang_path() {
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    let Ok(p) = env::var("LIBCLANG_PATH") else {
        return;
    };
    let dir = Path::new(&p);
    let has_libclang = std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("libclang")
                && (name.contains(".so") || name.contains(".dylib") || name.contains(".dll"))
        })
    });
    if !has_libclang {
        println!(
            "cargo:warning=LIBCLANG_PATH ({p}) contains no libclang; ignoring it \
             so bindgen can auto-detect the system libclang."
        );
        // Safe: single-threaded build script, before bindgen runs.
        env::remove_var("LIBCLANG_PATH");
    }
}

/// Which flavour of `librayforce.a` to link.
///
/// `Release` is the default and what every published build uses. `Debug` adds
/// `-DDEBUG`, which compiles in the core's invariant checks and its stale
/// retain/release detector (`ray_dfd_check_live` in `src/mem/cow.c`) — the only
/// tool that can see a use-after-free inside the engine's `mmap`-backed pool
/// allocator, which ASan and Valgrind are structurally blind to. Opt in with
/// `RAYFORCE_CORE_DEBUG=1`, then run with `RAY_DFD=1` to arm the detector.
///
/// Both flavours compile to the same object names, so switching forces a full
/// rebuild of the core (see the stamp handling in [`build_core_lib`]). CI never
/// pays that — each matrix leg is a fresh checkout building one flavour. Locally
/// it bites whenever you alternate, because `cargo clippy` and a debug
/// `cargo test` share one `RAYFORCE_SRC` tree; point the debug work at a second
/// checkout via `RAYFORCE_SRC` if the rebuilds get tiresome.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Flavour {
    Release,
    Debug,
}

impl Flavour {
    /// The stamp recorded next to the archive; also the `make clean` trigger.
    fn tag(self) -> &'static str {
        match self {
            Flavour::Release => "release",
            Flavour::Debug => "debug",
        }
    }
}

/// Read the flavour from `RAYFORCE_CORE_DEBUG`, using the same truthiness rule
/// as the core's own `dfd_enabled()` (`src/mem/heap.c`): set, non-empty, not
/// `"0"`. The empty-string case is not hypothetical — a GitHub Actions
/// conditional expression yields `''` for its false branch.
fn core_flavour() -> Flavour {
    println!("cargo:rerun-if-env-changed=RAYFORCE_CORE_DEBUG");
    match env::var("RAYFORCE_CORE_DEBUG") {
        Ok(v) if !v.is_empty() && v != "0" => Flavour::Debug,
        _ => Flavour::Release,
    }
}

fn build_core_lib(core: &Path) {
    let flavour = core_flavour();

    // Both flavours compile to the same `.rel.o` object names, so make would
    // consider objects from the *other* flavour up to date and archive a
    // mixed-flavour library. Clean when the recorded flavour differs.
    let stamp = core.join(".rayforce-rs-flavour");
    // An unstamped tree is assumed release: that is what every build predating
    // this stamp produced, so there is nothing to clean.
    let previous = std::fs::read_to_string(&stamp)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| Flavour::Release.tag().to_string());
    if previous != flavour.tag() {
        println!(
            "cargo:warning=core flavour changed {previous} -> {}; running `make clean`",
            flavour.tag()
        );
        let _ = Command::new("make").arg("clean").current_dir(core).status();
    }

    let mut cmd = Command::new("make");
    cmd.arg("lib").current_dir(core);
    if flavour == Flavour::Debug {
        // `DEBUG_CFLAGS` from the core's Makefile minus `-fsanitize=address,
        // undefined`: the sanitizers cannot see into the pool allocator (the
        // engine says so itself, `src/mem/heap.c`) and linking their runtime
        // into every Rust test binary buys nothing for the cost. `$(WARNS)`,
        // `$(STD)` and `$(RAY_MARCH)` are expanded by make from its own
        // definitions, so only the flavour delta is restated here.
        cmd.arg(
            "RELEASE_CFLAGS=-fPIC $(WARNS) -std=$(STD) -g -O0 \
             -march=$(RAY_MARCH) -DDEBUG -fno-omit-frame-pointer",
        );
    }

    let status = cmd
        .status()
        .expect("failed to invoke `make` to build librayforce.a");
    assert!(
        status.success(),
        "`make lib` failed in {} (exit {:?})",
        core.display(),
        status.code()
    );
    assert!(
        core.join("librayforce.a").exists(),
        "make lib succeeded but librayforce.a is missing in {}",
        core.display()
    );

    // Only stamp a build that got all the way through, so a failed switch
    // re-cleans on the next attempt instead of trusting a half-built tree.
    let _ = std::fs::write(&stamp, flavour.tag());
}
