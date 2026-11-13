//! Regression test for `string_split` / `string.split` host bridge.
//!
//! Dashboard fingerprints fixed by this test:
//!   - 68db26207477  HOST_BRIDGE_STRING_SPLIT_RETURNS_JSON_STRING_NOT_LIST  (server)
//!   - 2e54b6dd700e  RUNTIME_ITERATE_SPLIT_DERIVED_LIST_WRONG_LENGTH        (server)
//!
//! Background
//! ----------
//! `string.split` is consumed by compiler-emitted `iterate` code that expects a
//! Clean Language `list<string>` layout:
//!
//!   offset  0..4  : length     (u32 LE)
//!   offset  4..8  : capacity   (u32 LE)
//!   offset  8..12 : type_id    (u32 LE)
//!   offset 12..16 : padding
//!   offset 16..   : N pointers to length-prefixed strings
//!
//! Previously the bridge returned a JSON-encoded length-prefixed string
//! (e.g. `["a","b","c","d"]`). Treating the JSON-LP layout as a list put 17
//! (the JSON byte length) in the size slot and made `iterate` walk garbage
//! past the JSON body — either over-iterating to a wrong count or trapping.
//!
//! This test instantiates a tiny WAT module against the real server linker,
//! invokes `string.split("a```b```c```d", "```")`, and asserts the returned
//! pointer's bytes are a valid list<string> of 4 elements containing the
//! expected substrings.

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::WasmState;
use std::sync::Arc;
use wasmtime::{Engine, Module, Store};

// Tiny WAT shim that:
//   - exports a 1-page linear memory and a mutable `__heap_ptr` global
//   - exports a minimal bump-allocator `malloc` (the host bridge calls into
//     this to allocate result strings and the list block)
//   - exports a `write_lp_string` helper so the test can place input strings
//     into memory using the same length-prefixed format the bridge reads
//   - exports `do_split(s_ptr, delim_ptr) -> list_ptr` that simply forwards
//     to the imported `string.split`
//
// The bump allocator starts at address 1024 to leave a small scratch area
// at the very start of memory (the host bridge's defensive paths consult
// __heap_ptr).
const SPLIT_DRIVER_WAT: &str = r#"
(module
  (import "env" "string.split"
    (func $split (param i32 i32) (result i32)))

  (memory (export "memory") 4)
  (global $heap (export "__heap_ptr") (mut i32) (i32.const 1024))

  ;; malloc(size) -> ptr  — bump allocate `size` bytes (8-byte aligned).
  (func (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap
      (i32.and
        (i32.add
          (i32.add (global.get $heap) (local.get $size))
          (i32.const 7))
        (i32.const -8)))
    (local.get $ptr))

  ;; write_lp_string(ptr, byte_len) — caller has already written `byte_len`
  ;; raw UTF-8 bytes starting at `ptr + 4`; this only stamps the 4-byte
  ;; little-endian length prefix at `ptr` and returns `ptr`.
  (func (export "stamp_len") (param $ptr i32) (param $len i32) (result i32)
    (i32.store (local.get $ptr) (local.get $len))
    (local.get $ptr))

  ;; do_split(s_ptr, delim_ptr) -> list_ptr
  (func (export "do_split") (param $s i32) (param $d i32) (result i32)
    (call $split (local.get $s) (local.get $d)))
)
"#;

fn read_u32_le(memory: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(memory[offset..offset + 4].try_into().unwrap())
}

fn read_lp_bytes(memory: &[u8], ptr: usize) -> Vec<u8> {
    let len = read_u32_le(memory, ptr) as usize;
    memory[ptr + 4..ptr + 4 + len].to_vec()
}

/// Run `string.split(text, delim)` against the real server linker and return
/// the list's elements as byte strings, after checking the list header.
fn split_via_server(text: &[u8], delim: &[u8]) -> Vec<Vec<u8>> {
    let engine = Engine::default();
    let module = Module::new(&engine, SPLIT_DRIVER_WAT).unwrap();
    let linker = create_linker(&engine).unwrap();
    let mut store = {
        let router = Arc::new(Router::new());
        Store::new(&engine, WasmState::new(router))
    };
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let memory = instance.get_memory(&mut store, "memory").unwrap();

    let (s_ptr, delim_ptr) = {
        let buf = memory.data_mut(&mut store);
        let mut p = 16;
        let s_ptr = p as i32;
        p = write_lp_bytes(buf, p, text);
        let delim_ptr = p as i32;
        let _ = write_lp_bytes(buf, p, delim);
        (s_ptr, delim_ptr)
    };

    let do_split = instance
        .get_typed_func::<(i32, i32), i32>(&mut store, "do_split")
        .unwrap();
    let list_ptr = do_split.call(&mut store, (s_ptr, delim_ptr)).unwrap();
    assert!(list_ptr > 0, "string.split returned {}", list_ptr);

    let buf = memory.data(&store);
    let off = list_ptr as usize;
    let length = read_u32_le(buf, off) as usize;
    assert_eq!(read_u32_le(buf, off + 4) as usize, length, "capacity");
    assert_eq!(read_u32_le(buf, off + 8), 3, "type_id (3 = string)");
    (0..length)
        .map(|i| read_lp_bytes(buf, read_u32_le(buf, off + 16 + i * 4) as usize))
        .collect()
}

fn read_lp_string(memory: &[u8], ptr: usize) -> String {
    let len = read_u32_le(memory, ptr) as usize;
    String::from_utf8(memory[ptr + 4..ptr + 4 + len].to_vec())
        .expect("LP string body is not valid UTF-8")
}

/// Write a length-prefixed string at offset `ptr`. Returns the new heap
/// frontier (8-byte aligned) so the caller can place the next string after it.
fn write_lp_string(memory: &mut [u8], ptr: usize, s: &str) -> usize {
    write_lp_bytes(memory, ptr, s.as_bytes())
}

fn write_lp_bytes(memory: &mut [u8], ptr: usize, bytes: &[u8]) -> usize {
    memory[ptr..ptr + 4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    memory[ptr + 4..ptr + 4 + bytes.len()].copy_from_slice(bytes);
    (ptr + 4 + bytes.len() + 7) & !7
}

#[test]
fn string_split_returns_clean_list_layout() {
    let engine = Engine::default();
    let module =
        Module::new(&engine, SPLIT_DRIVER_WAT).expect("failed to compile split driver WAT module");
    let linker = create_linker(&engine).expect("failed to create server linker");
    let mut store = {
        let router = Arc::new(Router::new());
        Store::new(&engine, WasmState::new(router))
    };

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("failed to instantiate split driver against server linker");

    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("driver missing memory export");

    // Place input strings into a reserved low region [16..1024). The bump
    // allocator starts at 1024, so this won't be clobbered by malloc.
    let (s_ptr, delim_ptr) = {
        let buf = memory.data_mut(&mut store);
        let mut p = 16;
        let s_ptr = p as i32;
        p = write_lp_string(buf, p, "a```b```c```d");
        let delim_ptr = p as i32;
        let _ = write_lp_string(buf, p, "```");
        (s_ptr, delim_ptr)
    };

    let do_split = instance
        .get_typed_func::<(i32, i32), i32>(&mut store, "do_split")
        .expect("driver missing do_split export");

    let list_ptr = do_split
        .call(&mut store, (s_ptr, delim_ptr))
        .expect("string.split host call trapped");

    assert!(
        list_ptr > 0,
        "string.split returned null/error pointer: {}",
        list_ptr
    );

    // Inspect the returned list block.
    let buf = memory.data(&store);
    let list_off = list_ptr as usize;

    let length = read_u32_le(buf, list_off);
    let capacity = read_u32_le(buf, list_off + 4);
    let type_id = read_u32_le(buf, list_off + 8);
    let padding = read_u32_le(buf, list_off + 12);

    assert_eq!(
        length, 4,
        "list length field at offset 0 is {} — `iterate part in parts` would \
         run that many times, this is the dashboard bug",
        length
    );
    assert_eq!(capacity, 4, "list capacity field at offset 4");
    assert_eq!(
        type_id, 3,
        "list type_id field at offset 8 (3 = string, matches compiler convention)"
    );
    assert_eq!(padding, 0, "list padding field at offset 12 must be zero");

    // Element pointers at offset 16, 20, 24, 28 must point to valid LP strings
    // matching the original delimiter-split parts in order.
    let expected = ["a", "b", "c", "d"];
    for (i, want) in expected.iter().enumerate() {
        let slot = list_off + 16 + i * 4;
        let elem_ptr = read_u32_le(buf, slot) as usize;
        assert!(
            elem_ptr > 0 && elem_ptr + 4 < buf.len(),
            "element {} pointer at offset {} is out of range: {}",
            i,
            slot,
            elem_ptr
        );
        let got = read_lp_string(buf, elem_ptr);
        assert_eq!(
            &got, want,
            "element {} content mismatch (ptr={})",
            i, elem_ptr
        );
    }
}

#[test]
fn string_split_no_match_returns_single_element_list() {
    // When the delimiter is not present in the input, Rust's str::split (and
    // therefore the bridge) yields a one-element iterator with the whole
    // input. Verify the list layout still reads as length=1.
    let engine = Engine::default();
    let module = Module::new(&engine, SPLIT_DRIVER_WAT).unwrap();
    let linker = create_linker(&engine).unwrap();
    let mut store = {
        let router = Arc::new(Router::new());
        Store::new(&engine, WasmState::new(router))
    };
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let memory = instance.get_memory(&mut store, "memory").unwrap();

    let (s_ptr, delim_ptr) = {
        let buf = memory.data_mut(&mut store);
        let mut p = 16;
        let s_ptr = p as i32;
        p = write_lp_string(buf, p, "no-delim-here");
        let delim_ptr = p as i32;
        let _ = write_lp_string(buf, p, ",");
        (s_ptr, delim_ptr)
    };

    let do_split = instance
        .get_typed_func::<(i32, i32), i32>(&mut store, "do_split")
        .unwrap();
    let list_ptr = do_split.call(&mut store, (s_ptr, delim_ptr)).unwrap();
    assert!(list_ptr > 0);

    let buf = memory.data(&store);
    let off = list_ptr as usize;
    assert_eq!(read_u32_le(buf, off), 1);
    let elem_ptr = read_u32_le(buf, off + 16) as usize;
    assert_eq!(read_lp_string(buf, elem_ptr), "no-delim-here");
}

#[test]
fn string_split_empty_delimiter_gives_one_element_per_byte() {
    // `"abc".split("")` -> ["a", "b", "c"] (issue #3).
    let parts = split_via_server(b"abc", b"");
    assert_eq!(parts, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
}

#[test]
fn string_split_empty_text_and_empty_delimiter_gives_empty_list() {
    // `"".split("")` -> [] (issue #3).
    assert!(split_via_server(b"", b"").is_empty());
}

#[test]
fn string_split_empty_delimiter_splits_multibyte_character_into_bytes() {
    // A character is a byte: "h\u{e9}" (68 c3 a9) splits into three one-byte
    // strings, the last two each holding part of the UTF-8 encoding of e-acute.
    let parts = split_via_server("h\u{e9}".as_bytes(), b"");
    assert_eq!(parts, vec![vec![0x68], vec![0xC3], vec![0xA9]]);
}

#[test]
fn string_split_non_utf8_text_splits_on_bytes() {
    // A string holding a lone byte of a multi-byte character is a valid
    // Clean string; splitting it must not drop it.
    let parts = split_via_server(&[b'a', 0xC3, b',', b'b'], b",");
    assert_eq!(parts, vec![vec![b'a', 0xC3], vec![b'b']]);
}
