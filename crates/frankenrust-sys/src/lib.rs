//! Raw FFI surface: bindgen-generated PHP types (from
//! `vendor/frankenphp/frankenphp.h` and `types.h`) plus upstream's
//! `frankenphp.c`/`types.c`, compiled and linked unmodified by `build.rs`.
//!
//! This crate declares no policy and defines none of the 26 `go_*` symbols
//! `frankenphp.c` calls back into -- those are
//! `crates/frankenrust-core/src/callbacks/*` (see `docs/PORTING-NOTES.md`
//! and issue #7). Their C-side declarations live in
//! `include/_cgo_export.h`, hand-written because cgo would normally
//! generate that file at build time and it does not exist in this tree.

mod bindings {
    // bindgen output is machine-generated from PHP's own headers and does not
    // follow this project's lint conventions (e.g. non-`UpperCamelCase` type
    // names taken verbatim from C, no docs on generated items, bitfield
    // accessors built with `transmute` in a way `unnecessary_transmutes`
    // flags on current rustc). Isolated to this one module so the `allow`
    // cannot silently cover hand-written code elsewhere in the crate.
    #![allow(
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        dead_code,
        unnecessary_transmutes,
        clippy::all
    )]

    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub use bindings::*;

mod layout;
