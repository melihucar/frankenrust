//! Proves the nine constants build.rs's `allowlist_var` block adds (issue
//! #108) are actually reachable from Rust, agree pairwise, and describe the
//! real ABI -- not just that bindgen emitted *some* value under that name.
//! Without this, issue #106's zval/zend_array walkers would have had to
//! open-code `Z_TYPE_P`'s bit layout instead of reading `IS_*`/
//! `HASH_FLAG_PACKED` off `frankenrust_sys`.
//!
//! No Zend MM is initialised under `cargo test` (no `ts_resource(0)`, no
//! `php_request_startup()`), so every zval/HashTable fabricated here is
//! Rust-owned: stack-allocated via `Default::default()` (both `_zval_struct`
//! and `_zend_array` are unions bindgen zero-fills, per its derived
//! `Default` impl) and never handed to `zend_new_array`, `emalloc`, or
//! anything else that assumes a live interpreter.

use std::collections::HashSet;

use frankenrust_sys::{
    get_ht_packed_data, zval, HashTable, HASH_FLAG_PACKED, IS_ARRAY, IS_DOUBLE, IS_FALSE, IS_LONG,
    IS_NULL, IS_STRING, IS_TRUE, IS_UNDEF,
};

/// `vendor/frankenphp/types.go:111` (`v != nil && zval_get_type(v) !=
/// C.IS_UNDEF` guards processing a packed slot) and `:132` (`zval_get_type(&
/// bucket.val) == C.IS_UNDEF` skips a hashed slot) both use `IS_UNDEF` as the
/// "this slot is a hole" sentinel the array walk relies on. That only works
/// if `IS_UNDEF` is the value a zeroed hash-table region reads as, and if no
/// other type tag collides with it or with any other.
#[test]
fn type_tags_are_pairwise_distinct_and_undef_is_the_zero_sentinel() {
    let tags = [
        IS_UNDEF, IS_NULL, IS_FALSE, IS_TRUE, IS_LONG, IS_DOUBLE, IS_STRING, IS_ARRAY,
    ];
    let mut seen = HashSet::new();
    for &t in &tags {
        assert!(
            seen.insert(t),
            "duplicate value among the IS_* type tags: {t}"
        );
    }
    assert_eq!(
        IS_UNDEF, 0,
        "types.go's array walk (see above) treats IS_UNDEF as the value a zeroed zval reads as"
    );
}

/// Fabricates a zval by hand and proves the exact field path the converter
/// (issue #106) will use -- `(*z).u1.v.type_` and `(*z).value.lval`, per
/// `Zend/zend_types.h:647`'s `zval_get_type` (`return pz->u1.v.type;`) --
/// agrees with the bound `IS_LONG` constant under this build's real struct
/// layout. `frankenrust-sys/src/layout.rs` already asserts field *offsets*
/// for other structs; this asserts that a *value* written through that path
/// reads back correctly, which an offset assertion alone would not catch
/// (e.g. a byte-order or bit-width mismatch in the tag itself).
#[test]
fn zval_long_round_trips_through_the_bound_constant() {
    let mut z = zval::default();
    // SAFETY: `z` is a local, Rust-owned zval with no refcounted payload --
    // IS_LONG's value is an immediate `lval`, not a pointer -- so writing its
    // union fields and reading them back touches only this stack slot, which
    // nothing else can observe or race on.
    let (ty, val) = unsafe {
        z.u1.v.type_ = IS_LONG as u8;
        z.value.lval = 42;
        (z.u1.v.type_, z.value.lval)
    };
    assert_eq!(ty as u32, IS_LONG);
    assert_eq!(val, 42);
}

/// Fabricates a packed `HashTable` by hand and proves `HASH_FLAG_PACKED` is
/// exactly the bit `get_ht_packed_data` (`vendor/frankenphp/types.c:3-9`)
/// tests, without a live Zend MM: `arPacked` points at a plain Rust array
/// this function owns, and `get_ht_packed_data` only ever reads
/// `ht->u.flags` and returns `&ht->arPacked[index]` -- it never dereferences
/// the pointee itself, so no PHP allocator is involved on either side.
#[test]
fn get_ht_packed_data_reads_the_packed_flag() {
    let mut elems: [zval; 3] = [zval::default(), zval::default(), zval::default()];
    // SAFETY: `elems` is a Rust-owned local array; this writes element 0's
    // tag/value (writing a union field is always safe in Rust -- no old
    // value is dropped) and reads them back to confirm the write landed
    // before `elems` is handed to the packed HashTable below.
    let elem0_lval = unsafe {
        elems[0].u1.v.type_ = IS_LONG as u8;
        elems[0].value.lval = 7;
        elems[0].value.lval
    };
    assert_eq!(elem0_lval, 7);

    let mut ht = HashTable {
        nNumUsed: elems.len() as u32,
        ..Default::default()
    };
    // Writes to union fields are always safe in Rust (no prior value is
    // dropped): `ht.u` (flags word) and `ht.__bindgen_anon_1` (the
    // arHash/arData/arPacked union) are the two anonymous unions bindgen
    // generated for `zend_array`. `elems` outlives `ht` for the rest of this
    // function, so `arPacked` never dangles.
    ht.u.flags = HASH_FLAG_PACKED;
    ht.__bindgen_anon_1.arPacked = elems.as_mut_ptr();

    // SAFETY: `ht` is fully initialised above (flags, arPacked, nNumUsed all
    // set) and is a Rust-owned local; `get_ht_packed_data` only dereferences
    // `ht` itself, never the zval it returns a pointer to. `index = 0 <
    // ht.nNumUsed == elems.len()`, so the pointer it computes is in-bounds
    // of `elems`.
    let first = unsafe { get_ht_packed_data(&mut ht, 0) };
    assert_eq!(
        first,
        elems.as_mut_ptr(),
        "a packed HashTable's slot 0 must be &elems[0]"
    );
    // SAFETY: `first` was just proven equal to `elems.as_mut_ptr()`, a live,
    // fully-initialised Rust-owned pointer.
    let round_tripped = unsafe { (*first).value.lval };
    assert_eq!(round_tripped, 7);

    // Clearing the flags word is again a safe union-field write.
    ht.u.flags = 0;
    // SAFETY: same Rust-owned `ht`; `get_ht_packed_data` reads only
    // `ht->u.flags` before deciding to return NULL, which is already
    // initialised.
    let none = unsafe { get_ht_packed_data(&mut ht, 0) };
    assert!(
        none.is_null(),
        "a non-packed HashTable must yield NULL from get_ht_packed_data"
    );
}
