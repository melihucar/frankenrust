//! Compiles upstream's `frankenphp.c` / `types.c` unmodified and generates
//! Rust bindings for the PHP types they expose, mirroring the flag set
//! `vendor/frankenphp/cgo.go:3-10` and `vendor/frankenphp/go.sh:4-8` define
//! for the real (Go) build. See docs/PORTING-NOTES.md and issue #7.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs `php-config <args>` and splits stdout on whitespace, mirroring
/// `vendor/frankenphp/go.sh:4-8`, which does the same via shell `$(...)`.
/// Panics on failure: a silent fallback here would build against the wrong
/// PHP (or none) and only surface as a much harder to diagnose link error.
fn php_config(php_config_bin: &str, args: &[&str]) -> Vec<String> {
    let output = Command::new(php_config_bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{php_config_bin} {}`: {e}", args.join(" ")));
    if !output.status.success() {
        panic!(
            "`{php_config_bin} {}` exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| {
            panic!(
                "`{php_config_bin} {}` produced non-UTF-8 output: {e}",
                args.join(" ")
            )
        })
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Emits the right `cargo:rustc-link-*` instruction for one token of
/// `php-config --ldflags`/`--libs` output (e.g. `-lssl`, `-L/usr/lib`,
/// `-Wl,-O1`). `-l`/`-L` propagate to every downstream crate that links this
/// one, which is what makes `-lphp` reach frankenrust-core/-server later;
/// bare linker flags via `rustc-link-arg` only apply to targets built by
/// *this* package (see the comment where PIE/rpath flags are emitted below).
fn emit_link_flag(flag: &str) {
    if let Some(lib) = flag.strip_prefix("-l") {
        println!("cargo:rustc-link-lib={lib}");
    } else if let Some(path) = flag.strip_prefix("-L") {
        println!("cargo:rustc-link-search=native={path}");
    } else if !flag.is_empty() {
        println!("cargo:rustc-link-arg={flag}");
    }
}

/// Probes whether `FRANKENPHP_HAS_KILL_SIGNAL` is defined for this build
/// (`vendor/frankenphp/frankenphp.h:56-59`: true iff `!PHP_WIN32 &&
/// defined(SIGRTMIN)`), by asking the real preprocessor rather than guessing
/// from `target_os`. `force_kill_slot`'s layout depends on it directly (an
/// extra `pthread_t tid` field), and the acceptance test needs to assert
/// against the header's own condition, not a hardcoded word count.
fn probe_has_kill_signal(
    vendor_dir: &Path,
    sys_include_dir: &Path,
    include_paths: &[PathBuf],
    target_os: &str,
    out_dir: &Path,
) -> bool {
    const MARKER: &str = "FRANKENRUST_HAS_KILL_SIGNAL_PROBE_RESULT";
    let probe_path = out_dir.join("probe_kill_signal.c");
    fs::write(
        &probe_path,
        format!(
            "#include \"frankenphp.h\"\n\
             #ifdef FRANKENPHP_HAS_KILL_SIGNAL\n\
             {MARKER} yes\n\
             #else\n\
             {MARKER} no\n\
             #endif\n"
        ),
    )
    .expect("failed to write probe_kill_signal.c");

    let mut probe = cc::Build::new();
    probe
        .file(&probe_path)
        .include(sys_include_dir)
        .include(vendor_dir);
    for path in include_paths {
        probe.include(path);
    }
    if target_os == "linux" {
        probe.define("_GNU_SOURCE", None);
    }
    let expanded = String::from_utf8_lossy(&probe.expand()).into_owned();

    if expanded.contains(&format!("{MARKER} yes")) {
        true
    } else if expanded.contains(&format!("{MARKER} no")) {
        false
    } else {
        panic!(
            "could not determine FRANKENPHP_HAS_KILL_SIGNAL from preprocessed output:\n{expanded}"
        );
    }
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("../../vendor/frankenphp");
    let sys_include_dir = manifest_dir.join("include");
    let wrapper_h = manifest_dir.join("wrapper.h");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");

    for path in [
        vendor_dir.join("frankenphp.c"),
        vendor_dir.join("types.c"),
        vendor_dir.join("frankenphp.h"),
        vendor_dir.join("types.h"),
        vendor_dir.join("frankenphp_arginfo.h"),
        sys_include_dir.join("_cgo_export.h"),
        sys_include_dir.join("frankenrust_shim.h"),
        manifest_dir.join("shim.c"),
        wrapper_h.clone(),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");

    let php_config_bin = env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".to_string());

    // vendor/frankenphp/go.sh:4-8 / cgo.go:3-10 -- the flag set we mirror. Deliberately
    // not `php-config --libs` -- see the comment at its would-be call site below.
    let includes = php_config(&php_config_bin, &["--includes"]);
    let ldflags = php_config(&php_config_bin, &["--ldflags"]);

    let include_paths: Vec<PathBuf> =
        includes
            .iter()
            .map(|flag| {
                PathBuf::from(flag.strip_prefix("-I").unwrap_or_else(|| {
                    panic!("unexpected token in `php-config --includes`: {flag}")
                }))
            })
            .collect();

    // --- compile frankenphp.c + types.c, unmodified ----------------------------------
    let mut build = cc::Build::new();
    build
        .file(vendor_dir.join("frankenphp.c"))
        .file(vendor_dir.join("types.c"))
        // Ours, not upstream's: the C-side definition of
        // go_register_server_variables, which keeps a zend_bailout()'s longjmp
        // from ever crossing a Rust frame (issue #11). Compiled with exactly
        // the same flags and include path as the vendored sources because it
        // calls into them and reads EG()/SG() the same way.
        .file(manifest_dir.join("shim.c"))
        // for frankenphp.c:47 `#include "_cgo_export.h"`
        .include(&sys_include_dir)
        // for _cgo_export.h's own `#include "frankenphp.h"`, and types.c/types.h's
        // quote-includes of files that live next to them in vendor/frankenphp
        .include(&vendor_dir)
        // cc's own opinionated warning defaults are not upstream's; set exactly
        // upstream's unix CFLAGS (cgo.go:4) ourselves instead.
        .warnings(false)
        .flag("-Wall")
        .flag("-Werror");

    for path in &include_paths {
        build.include(path);
    }

    match target_os.as_str() {
        "linux" => {
            // cgo.go:5 `#cgo linux CFLAGS: -D_GNU_SOURCE`
            build.define("_GNU_SOURCE", None);
        }
        "macos" => {
            // cgo.go:3 `#cgo darwin pkg-config: libxml-2.0`
            let xml2 = pkg_config::probe_library("libxml-2.0").unwrap_or_else(|e| {
                panic!(
                    "pkg-config libxml-2.0 not found (required on macOS, see \
                     vendor/frankenphp/cgo.go:3): {e}"
                )
            });
            for path in xml2.include_paths {
                build.include(path);
            }
        }
        "windows" => {
            panic!(
                "frankenrust-sys does not support windows: frankenphp.c's Windows branches \
                 (HANDLE-based force-kill, WIN32 SAPI startup) are untouched by this build, and \
                 the only toolchain this repo builds against (docker/frankenrust-dev.Dockerfile, \
                 issue #5) is linux/arm64. See docs/PORTING-NOTES.md."
            );
        }
        other => {
            // frankenphp.c has some other-BSD code paths (frankenphp.c:34-39), but
            // nothing here has ever been run against them; fail loudly rather than
            // silently build with untested flags on an unverified platform.
            panic!(
                "frankenrust-sys has not been verified on target_os = \"{other}\"; the only \
                 supported/tested platforms are linux and macos (see docs/PORTING-NOTES.md)."
            );
        }
    }

    build.compile("frankenphp_c");

    let has_kill_signal = probe_has_kill_signal(
        &vendor_dir,
        &sys_include_dir,
        &include_paths,
        &target_os,
        &out_dir,
    );
    println!("cargo:rustc-check-cfg=cfg(frankenphp_has_kill_signal)");
    if has_kill_signal {
        println!("cargo:rustc-cfg=frankenphp_has_kill_signal");
    }

    // --- link --------------------------------------------------------------------------
    // php-config's own `--ldflags` (linker tuning, e.g. `-Wl,-O1 -pie`; no `-l`/`-L`
    // tokens on any platform this has been run on)...
    for flag in &ldflags {
        emit_link_flag(flag);
    }
    // ...but deliberately NOT `php-config --libs` (e.g. `-lreadline -lncurses -lcurl
    // -lonig -lsqlite3 -largon2 -lxml2 -lz -lssl -lcrypto ...`): those are libphp.so's
    // OWN transitive shared-library dependencies (`ldd /usr/local/lib/libphp.so` lists
    // every one of them), not symbols frankenphp.c/types.c call directly, and the
    // dynamic linker resolves a .so's DT_NEEDED entries on its own at load time.
    // Upstream's cgo build re-lists them because cgo's C compiler invocation plays the
    // role of the final linker for the whole Go binary and historically assumes they
    // may be needed explicitly; re-emitting them here does nothing but demand an
    // unversioned `libNAME.so` symlink for each one, which only ships with that
    // library's `-dev` package. docker/frankenrust-dev.Dockerfile's PHP base is a
    // runtime image (it has e.g. `libreadline.so.8` but not `libreadline.so`), so doing
    // what upstream's flags literally say breaks the link here for a library we never
    // call into. `-lphp` below is the one entry from that set frankenphp.c actually
    // needs, and it works because /usr/local/lib carries `libphp.so` unversioned.
    //
    // ...then cgo.go's hardcoded additions on top, mirrored exactly. `-lphp`/`-lm`/
    // `-lutil`/`-ldl`/`-lresolv` use rustc-link-lib, which Cargo propagates to every
    // downstream binary that links this crate transitively (frankenrust-core,
    // frankenrust-server). `-Wl,-rpath,...` and similar raw flags below only apply to
    // *this* package's own binaries/tests (frankenrust-sys/tests/version.rs) --
    // Cargo does not propagate bare `rustc-link-arg` to dependents. That is a
    // non-functional (loader-tuning) gap for the eventual frankenrust-server binary;
    // revisit if #14 needs it.
    for lib in ["php", "m", "util"] {
        println!("cargo:rustc-link-lib={lib}");
    }
    match target_os.as_str() {
        "linux" => {
            for lib in ["dl", "resolv"] {
                println!("cargo:rustc-link-lib={lib}");
            }
        }
        "macos" => {
            for lib in ["iconv", "dl"] {
                println!("cargo:rustc-link-lib={lib}");
            }
            println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib");
        }
        _ => unreachable!("handled above"),
    }

    // --- bindgen: types from frankenphp.h and types.h -----------------------------------
    let bindings = bindgen::Builder::default()
        .header(
            wrapper_h
                .to_str()
                .expect("wrapper.h path must be valid UTF-8"),
        )
        .clang_arg(format!("-I{}", sys_include_dir.display()))
        .clang_arg(format!("-I{}", vendor_dir.display()))
        .clang_args(include_paths.iter().map(|p| format!("-I{}", p.display())))
        .clang_args(match target_os.as_str() {
            "linux" => vec!["-D_GNU_SOURCE".to_string()],
            _ => vec![],
        })
        // --- types crossed by value across the FFI boundary, or referenced directly
        // by the go_* callback signatures in _cgo_export.h. Curated by name rather
        // than by wildcard/regex on purpose (docs/PORTING-NOTES.md's bindgen hazard
        // note, and ext-php-rs's allowed_bindings.rs, which takes the same
        // explicit-allowlist approach against the same headers): a broad regex over
        // frankenphp.h would recursively pull in every type transitively reachable
        // from it, including PHP internals this crate has no reason to touch yet.
        .allowlist_type("frankenphp_version")
        .allowlist_type("frankenphp_config")
        .allowlist_type("force_kill_slot")
        .allowlist_type("frankenphp_server_vars")
        .allowlist_type("go_string")
        .allowlist_type("go_apache_request_headers_return")
        .allowlist_type("go_frankenphp_worker_handle_request_start_return")
        .allowlist_type("go_ub_write_return")
        .allowlist_type("go_mercure_publish_return")
        .allowlist_type("sapi_request_info")
        .allowlist_type("zend_string")
        .allowlist_type("zend_llist")
        .allowlist_type("zend_array")
        .allowlist_type("HashTable")
        .allowlist_type("zval")
        .allowlist_type("zend_long")
        // shim.c's wire format (frankenrust_shim.h) -- see the `.file()` above.
        // Taken from the header rather than hand-written in frankenrust-core so
        // the two sides of the boundary cannot drift.
        .allowlist_type("frankenrust_header_var")
        .allowlist_type("frankenrust_server_vars_batch")
        // frankenphp.h functions this issue's acceptance test and the near-term
        // callback bodies need. frankenrust-sys/build.rs is not part of issue #7's
        // frozen module layout (docs/ARCHITECTURE.md) -- later issues extend this
        // list as they need more of frankenphp.h's / types.h's surface.
        .allowlist_function("frankenphp_get_version")
        .allowlist_function("frankenphp_get_config")
        // Interning is the one PHP call issue #11 makes from Rust (see
        // servervars.rs's `intern`) -- it is a plain persistent malloc with no
        // TSRM dependency and cannot bail out. `frankenphp_register_server_vars`,
        // `frankenphp_register_known_variable` and `frankenphp_register_variable_safe`
        // are deliberately NOT allowlisted here: under issue #11's design they are
        // called only from shim.c, which already has the real header, and exposing
        // them to Rust would invite the mistake that design exists to prevent.
        .allowlist_function("frankenphp_init_persistent_string")
        // The two real thread entry points (frankenphp.h:190-191): not called by
        // this issue's abort-stubs, but tests/version.rs takes their address (never
        // calls them) so the linker's --gc-sections can see a live reference into
        // frankenphp.c's actual call graph -- otherwise nothing reaches the go_*
        // symbols at all (frankenphp_get_version() alone doesn't call anything) and
        // gc-sections silently prunes both the references and frankenrust-core's
        // matching definitions before the nm check ever gets to see them.
        .allowlist_function("frankenphp_new_main_thread")
        .allowlist_function("frankenphp_new_php_thread")
        .allowlist_function("frankenphp_init_persistent_string")
        .allowlist_function("frankenphp_init_thread_metrics")
        .allowlist_function("frankenphp_destroy_thread_metrics")
        .allowlist_function("frankenphp_force_kill_thread")
        .allowlist_function("frankenphp_release_thread_for_kill")
        // `max_threads=auto` resolution (issue #103, `frankenphp.h:210`):
        // reads `PG(memory_limit)`, truncated to C `int` by the function
        // itself (`frankenphp.c:1916`). Only ever called from the main PHP
        // pthread inside `go_frankenphp_main_thread_is_ready`, after PHP
        // module startup has parsed php.ini.
        .allowlist_function("frankenphp_get_current_memory_limit")
        // PHP includes stdlib.h unconditionally. These declarations keep
        // cross-boundary allocations paired with C's allocator without adding
        // a second hand-written FFI surface in frankenrust-core.
        .allowlist_function("malloc")
        .allowlist_function("free")
        // types.h's helpers: thin wrappers this port uses instead of bindgen-ing
        // macro-only Zend APIs directly (ZVAL_* etc. are macros, invisible to
        // bindgen). Not called yet (out of scope: "any callback body beyond the
        // abort-stub"), but their argument/return types are already covered above,
        // so exposing them now costs nothing and saves #10-#14 a build.rs edit.
        .allowlist_function("get_ht_packed_data")
        .allowlist_function("get_ht_bucket_data")
        .allowlist_function("__emalloc__")
        .allowlist_function("__efree__")
        .allowlist_function("__zend_hash_init__")
        .allowlist_function("__hash_update_string__")
        .allowlist_function("__zend_is_callable__")
        .allowlist_function("__call_user_function__")
        .allowlist_function("__zval_null__")
        .allowlist_function("__zval_bool__")
        .allowlist_function("__zval_long__")
        .allowlist_function("__zval_double__")
        .allowlist_function("__zval_string__")
        .allowlist_function("__zval_empty_string__")
        .allowlist_function("__zval_arr__")
        .allowlist_function("__zend_new_array__")
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: false,
        })
        .derive_default(true)
        .generate()
        .expect("bindgen failed to generate bindings for frankenphp.h/types.h");

    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindgen output to OUT_DIR");
}
