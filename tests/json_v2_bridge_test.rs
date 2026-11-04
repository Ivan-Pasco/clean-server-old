//! `_json_encode_v2` / `_json_encode_pretty_v2` / `_json_decode_v2` bridge tests.
//!
//! Delivery-2 (P3a-reissue-2) — validates that clean-server marshals boxed-Any
//! values across the WASM/host boundary per foundation/spec/platform/BOXED_ANY_ABI.md.
//! Container payloads use the JSON-tree fragment layout (§3.2 Array 4-byte
//! header + count×4 ptr stride; §3.3 Object 4-byte header + count×8 (key,val)
//! stride, insertion-order preserved per D3). Legacy `_json_encode` /
//! `_json_decode` bridges are not tested here — they are held for backward
//! compatibility with frame.server 2.9.5 and dropped in [P5].

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::WasmState;
use std::sync::Arc;
use wasmtime::{Engine, Instance, Module, Store, TypedFunc};

// Minimal host module — imports the three v2 bridges, exports `memory` and
// `malloc`, and thin trampolines so the test drives them without hand-rolling
// wasmtime dispatch each call.
const WAT: &str = r#"
(module
  (import "env" "_json_encode_v2" (func $encode (param i32) (result i32)))
  (import "env" "_json_encode_pretty_v2" (func $encode_pretty (param i32) (result i32)))
  (import "env" "_json_decode_v2" (func $decode (param i32 i32) (result i32)))
  (memory (export "memory") 4)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "call_encode") (param $boxed i32) (result i32)
    (call $encode (local.get $boxed)))
  (func (export "call_encode_pretty") (param $boxed i32) (result i32)
    (call $encode_pretty (local.get $boxed)))
  (func (export "call_decode") (param $ptr i32) (param $len i32) (result i32)
    (call $decode (local.get $ptr) (local.get $len))))
"#;

struct H {
    store: Store<WasmState>,
    instance: Instance,
    encode: TypedFunc<i32, i32>,
    encode_pretty: TypedFunc<i32, i32>,
    decode: TypedFunc<(i32, i32), i32>,
    malloc: TypedFunc<i32, i32>,
}

impl H {
    fn new() -> Self {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("linker");
        let module = Module::new(&engine, WAT).expect("WAT");
        let router = Arc::new(Router::new());
        let mut store = Store::new(&engine, WasmState::new(router));
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let encode = instance
            .get_typed_func(&mut store, "call_encode")
            .expect("call_encode");
        let encode_pretty = instance
            .get_typed_func(&mut store, "call_encode_pretty")
            .expect("call_encode_pretty");
        let decode = instance
            .get_typed_func(&mut store, "call_decode")
            .expect("call_decode");
        let malloc = instance
            .get_typed_func(&mut store, "malloc")
            .expect("malloc");
        Self {
            store,
            instance,
            encode,
            encode_pretty,
            decode,
            malloc,
        }
    }

    fn mem_write(&mut self, off: usize, bytes: &[u8]) {
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .expect("memory");
        let data = memory.data_mut(&mut self.store);
        data[off..off + bytes.len()].copy_from_slice(bytes);
    }

    fn mem_read(&mut self, off: usize, len: usize) -> Vec<u8> {
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .expect("memory");
        let data = memory.data(&mut self.store);
        data[off..off + len].to_vec()
    }

    fn read_i32(&mut self, off: usize) -> i32 {
        let v = self.mem_read(off, 4);
        i32::from_le_bytes(v.try_into().unwrap())
    }

    fn read_lp_string(&mut self, ptr: i32) -> String {
        let len = self.read_i32(ptr as usize) as usize;
        let bytes = self.mem_read(ptr as usize + 4, len);
        String::from_utf8(bytes).expect("valid utf-8")
    }

    // Allocate a raw N-byte block via malloc.
    fn alloc(&mut self, n: i32) -> i32 {
        self.malloc.call(&mut self.store, n).expect("malloc")
    }

    // Allocate an LP string (4-byte length header + bytes).
    fn alloc_lp(&mut self, s: &str) -> i32 {
        let bytes = s.as_bytes();
        let ptr = self.alloc(4 + bytes.len() as i32);
        let mut hdr = [0u8; 4];
        hdr.copy_from_slice(&(bytes.len() as i32).to_le_bytes());
        self.mem_write(ptr as usize, &hdr);
        self.mem_write(ptr as usize + 4, bytes);
        ptr
    }

    // Allocate a 12-byte boxed-Any block and populate it. `val0` is the low 4
    // bytes of the value slot; `val1` the high 4 bytes.
    fn alloc_boxed(&mut self, tag: i32, val0: i32, val1: i32) -> i32 {
        let ptr = self.alloc(12);
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&tag.to_le_bytes());
        buf[4..8].copy_from_slice(&val0.to_le_bytes());
        buf[8..12].copy_from_slice(&val1.to_le_bytes());
        self.mem_write(ptr as usize, &buf);
        ptr
    }

    fn alloc_boxed_i64(&mut self, tag: i32, whole: i64) -> i32 {
        let ptr = self.alloc(12);
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&tag.to_le_bytes());
        buf[4..12].copy_from_slice(&whole.to_le_bytes());
        self.mem_write(ptr as usize, &buf);
        ptr
    }

    fn alloc_boxed_f64(&mut self, f: f64) -> i32 {
        let ptr = self.alloc(12);
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&3i32.to_le_bytes());
        buf[4..12].copy_from_slice(&f.to_le_bytes());
        self.mem_write(ptr as usize, &buf);
        ptr
    }

    fn encode(&mut self, boxed: i32) -> String {
        let out = self.encode.call(&mut self.store, boxed).expect("encode");
        assert!(out > 0, "encode returned non-positive ptr {}", out);
        self.read_lp_string(out)
    }

    fn encode_pretty(&mut self, boxed: i32) -> String {
        let out = self
            .encode_pretty
            .call(&mut self.store, boxed)
            .expect("encode_pretty");
        assert!(out > 0, "encode_pretty returned non-positive ptr {}", out);
        self.read_lp_string(out)
    }

    fn decode(&mut self, json: &str) -> i32 {
        let json_bytes = json.as_bytes();
        let ptr = self.alloc(4 + json_bytes.len() as i32);
        let mut hdr = [0u8; 4];
        hdr.copy_from_slice(&(json_bytes.len() as i32).to_le_bytes());
        self.mem_write(ptr as usize, &hdr);
        self.mem_write(ptr as usize + 4, json_bytes);
        // _json_decode_v2 uses (ptr, len) expanded from a "string" param.
        // Length excludes the 4-byte header — the LP header is what
        // read_raw_string reads via ptr; but the bridge uses the raw ptr+len
        // convention (`params = ["string"]` expands to (ptr, len) with the
        // content starting at ptr). Pass the content start and its length.
        self.decode
            .call(&mut self.store, (ptr + 4, json_bytes.len() as i32))
            .expect("decode")
    }
}

// ============================================================================
// _json_encode_v2 — one test per primitive plus a couple of composite cases.
// ============================================================================

#[test]
fn test_json_encode_v2_null() {
    let mut h = H::new();
    let boxed = h.alloc_boxed(0, 0, 0);
    assert_eq!(h.encode(boxed), "null");
}

#[test]
fn test_json_encode_v2_integer() {
    let mut h = H::new();
    let boxed = h.alloc_boxed_i64(1, 42);
    assert_eq!(h.encode(boxed), "42");

    let neg = h.alloc_boxed_i64(1, -12345);
    assert_eq!(h.encode(neg), "-12345");
}

#[test]
fn test_json_encode_v2_number() {
    let mut h = H::new();
    let boxed = h.alloc_boxed_f64(2.75);
    assert_eq!(h.encode(boxed), "2.75");
}

#[test]
fn test_json_encode_v2_boolean() {
    let mut h = H::new();
    let t = h.alloc_boxed(2, 1, 0);
    assert_eq!(h.encode(t), "true");
    let f = h.alloc_boxed(2, 0, 0);
    assert_eq!(h.encode(f), "false");
}

#[test]
fn test_json_encode_v2_string() {
    let mut h = H::new();
    let str_ptr = h.alloc_lp("hello world");
    let boxed = h.alloc_boxed(4, str_ptr, 0);
    assert_eq!(h.encode(boxed), "\"hello world\"");
}

#[test]
fn test_json_encode_v2_array() {
    let mut h = H::new();
    // Build [1, 2, "three"] using the compiler's JSON-tree Array layout:
    //   header (i32 count) at offset 0, then count × 4-byte boxed-Any pointers.
    //   Total = 4 + 3 * 4 = 16 bytes.
    let e0 = h.alloc_boxed_i64(1, 1);
    let e1 = h.alloc_boxed_i64(1, 2);
    let three_str = h.alloc_lp("three");
    let e2 = h.alloc_boxed(4, three_str, 0);
    let arr_ptr = h.alloc(16);
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&3i32.to_le_bytes());
    buf[4..8].copy_from_slice(&e0.to_le_bytes());
    buf[8..12].copy_from_slice(&e1.to_le_bytes());
    buf[12..16].copy_from_slice(&e2.to_le_bytes());
    h.mem_write(arr_ptr as usize, &buf);
    let boxed = h.alloc_boxed(5, arr_ptr, 0);
    assert_eq!(h.encode(boxed), "[1,2,\"three\"]");
}

#[test]
fn test_json_encode_v2_object_preserves_insertion_order() {
    // D3 — insertion order MUST survive the roundtrip. Build {"z": 1, "a": 2, "m": 3}
    // and confirm serialized output matches source order (not alphabetical).
    let mut h = H::new();
    let v0 = h.alloc_boxed_i64(1, 1);
    let v1 = h.alloc_boxed_i64(1, 2);
    let v2 = h.alloc_boxed_i64(1, 3);
    let k0 = h.alloc_lp("z");
    let k1 = h.alloc_lp("a");
    let k2 = h.alloc_lp("m");
    // JSON-tree Object: 4-byte header + 3 × 8-byte entries = 28 bytes.
    let obj_ptr = h.alloc(28);
    let mut buf = [0u8; 28];
    buf[0..4].copy_from_slice(&3i32.to_le_bytes());
    buf[4..8].copy_from_slice(&k0.to_le_bytes());
    buf[8..12].copy_from_slice(&v0.to_le_bytes());
    buf[12..16].copy_from_slice(&k1.to_le_bytes());
    buf[16..20].copy_from_slice(&v1.to_le_bytes());
    buf[20..24].copy_from_slice(&k2.to_le_bytes());
    buf[24..28].copy_from_slice(&v2.to_le_bytes());
    h.mem_write(obj_ptr as usize, &buf);
    let boxed = h.alloc_boxed(6, obj_ptr, 0);
    assert_eq!(h.encode(boxed), r#"{"z":1,"a":2,"m":3}"#);
}

#[test]
fn test_json_encode_v2_nested() {
    // { "items": [1, 2] }
    let mut h = H::new();
    let e0 = h.alloc_boxed_i64(1, 1);
    let e1 = h.alloc_boxed_i64(1, 2);
    let arr_ptr = h.alloc(12);
    let mut arr = [0u8; 12];
    arr[0..4].copy_from_slice(&2i32.to_le_bytes());
    arr[4..8].copy_from_slice(&e0.to_le_bytes());
    arr[8..12].copy_from_slice(&e1.to_le_bytes());
    h.mem_write(arr_ptr as usize, &arr);
    let arr_boxed = h.alloc_boxed(5, arr_ptr, 0);
    let key = h.alloc_lp("items");
    let obj_ptr = h.alloc(12);
    let mut obj = [0u8; 12];
    obj[0..4].copy_from_slice(&1i32.to_le_bytes());
    obj[4..8].copy_from_slice(&key.to_le_bytes());
    obj[8..12].copy_from_slice(&arr_boxed.to_le_bytes());
    h.mem_write(obj_ptr as usize, &obj);
    let boxed = h.alloc_boxed(6, obj_ptr, 0);
    assert_eq!(h.encode(boxed), r#"{"items":[1,2]}"#);
}

#[test]
fn test_json_encode_pretty_v2_two_space_indent() {
    let mut h = H::new();
    // { "n": 1 } pretty is: {\n  "n": 1\n}
    let v = h.alloc_boxed_i64(1, 1);
    let k = h.alloc_lp("n");
    let obj_ptr = h.alloc(12);
    let mut obj = [0u8; 12];
    obj[0..4].copy_from_slice(&1i32.to_le_bytes());
    obj[4..8].copy_from_slice(&k.to_le_bytes());
    obj[8..12].copy_from_slice(&v.to_le_bytes());
    h.mem_write(obj_ptr as usize, &obj);
    let boxed = h.alloc_boxed(6, obj_ptr, 0);
    let pretty = h.encode_pretty(boxed);
    assert_eq!(pretty, "{\n  \"n\": 1\n}");
    assert!(pretty.contains("  \"n\""), "expected two-space indent");
}

// ============================================================================
// _json_decode_v2 — verify tag + layout offsets of the produced boxed-Any tree.
// ============================================================================

#[test]
fn test_json_decode_v2_null() {
    let mut h = H::new();
    let ptr = h.decode("null");
    assert!(ptr > 0);
    let tag = h.read_i32(ptr as usize);
    assert_eq!(tag, 0);
}

#[test]
fn test_json_decode_v2_integer() {
    let mut h = H::new();
    let ptr = h.decode("42");
    assert_eq!(h.read_i32(ptr as usize), 1);
    let lo = h.read_i32(ptr as usize + 4);
    let hi = h.read_i32(ptr as usize + 8);
    let whole = ((hi as i64) << 32) | (lo as i64 & 0xFFFF_FFFF);
    assert_eq!(whole, 42);
}

#[test]
fn test_json_decode_v2_number() {
    let mut h = H::new();
    let ptr = h.decode("2.75");
    assert_eq!(h.read_i32(ptr as usize), 3);
    let bytes = h.mem_read(ptr as usize + 4, 8);
    let f = f64::from_le_bytes(bytes.try_into().unwrap());
    assert!((f - 2.75).abs() < 1e-9);
}

#[test]
fn test_json_decode_v2_boolean() {
    let mut h = H::new();
    let t_ptr = h.decode("true");
    assert_eq!(h.read_i32(t_ptr as usize), 2);
    assert_eq!(h.read_i32(t_ptr as usize + 4), 1);
    let f_ptr = h.decode("false");
    assert_eq!(h.read_i32(f_ptr as usize), 2);
    assert_eq!(h.read_i32(f_ptr as usize + 4), 0);
}

#[test]
fn test_json_decode_v2_string() {
    let mut h = H::new();
    let ptr = h.decode(r#""hello""#);
    assert_eq!(h.read_i32(ptr as usize), 4);
    let str_ptr = h.read_i32(ptr as usize + 4);
    assert_eq!(h.read_lp_string(str_ptr), "hello");
}

#[test]
fn test_json_decode_v2_array_produces_4byte_header() {
    // §3.2: JSON-tree Array = 4-byte count + count × 4-byte ptrs.
    // For [1, 2, 3] the container payload must be exactly 4 + 3*4 = 16 bytes
    // — we assert the shape via count read + child tag reads (no sentinel
    // padding, no type_id).
    let mut h = H::new();
    let ptr = h.decode("[1,2,3]");
    assert_eq!(h.read_i32(ptr as usize), 5); // tag Array
    let arr_ptr = h.read_i32(ptr as usize + 4);
    let count = h.read_i32(arr_ptr as usize);
    assert_eq!(count, 3);
    for i in 0..3 {
        let elem_ptr = h.read_i32(arr_ptr as usize + 4 + (i as usize) * 4);
        // Every element is a boxed-Any Integer holding its 1-indexed value.
        assert_eq!(h.read_i32(elem_ptr as usize), 1);
        let lo = h.read_i32(elem_ptr as usize + 4);
        assert_eq!(lo, i + 1);
    }
}

#[test]
fn test_json_decode_v2_object_produces_4byte_header() {
    // §3.3: JSON-tree Object = 4-byte count + count × 8-byte (key_ptr, val_ptr).
    // For {"a": 1, "b": 2} the payload must be exactly 4 + 2*8 = 20 bytes.
    let mut h = H::new();
    let ptr = h.decode(r#"{"a":1,"b":2}"#);
    assert_eq!(h.read_i32(ptr as usize), 6);
    let obj_ptr = h.read_i32(ptr as usize + 4);
    let count = h.read_i32(obj_ptr as usize);
    assert_eq!(count, 2);
    let k0 = h.read_i32(obj_ptr as usize + 4);
    let v0 = h.read_i32(obj_ptr as usize + 8);
    let k1 = h.read_i32(obj_ptr as usize + 12);
    let v1 = h.read_i32(obj_ptr as usize + 16);
    assert_eq!(h.read_lp_string(k0), "a");
    assert_eq!(h.read_i32(v0 as usize), 1);
    assert_eq!(h.read_lp_string(k1), "b");
    assert_eq!(h.read_i32(v1 as usize), 1);
}

#[test]
fn test_json_decode_v2_invalid_returns_zero() {
    let mut h = H::new();
    // Malformed JSON — must return the 0 sentinel, not trap and not a valid
    // boxed-Any pointer (D5, BOXED_ANY_ABI §6).
    let ptr = h.decode("{not: json");
    assert_eq!(ptr, 0, "decode of invalid JSON must return 0 sentinel");
}

#[test]
fn test_json_decode_v2_roundtrip() {
    // Decode → encode should produce equivalent JSON (structure-preserving,
    // insertion-order preserving under D3 + preserve_order feature).
    let mut h = H::new();
    let src = r#"{"z":1,"a":[true,null,"x"],"m":{"k":2}}"#;
    let decoded_ptr = h.decode(src);
    assert!(decoded_ptr > 0);
    let re_encoded = h.encode(decoded_ptr);
    assert_eq!(re_encoded, src);
}

// ============================================================================
// Cross-layer parity — the boxed-Any our tests hand to _json_encode_v2 uses
// the exact same JSON-tree fragment layout the compiler's
// __json_from_cln_list helper produces. This is the byte-for-byte check that
// caught the earlier spec/impl divergence.
// ============================================================================

#[test]
fn test_cross_layer_encode_bit_for_bit_matches_serde_json() {
    // Construct a boxed-Any Array whose JSON-tree layout exactly mirrors what
    // the compiler's __json_from_cln_list helper emits at json_class.rs:4095
    // (malloc(4 + count*4), i32 count at offset 0, count × i32 element
    // pointers at offset 4 with stride 4, each element a fresh 12-byte boxed
    // Any). Encode via bridge; compare to reference serde_json::to_string.
    let mut h = H::new();
    let e0 = h.alloc_boxed_i64(1, 10);
    let e1 = h.alloc_boxed_i64(1, 20);
    let e2 = h.alloc_boxed_i64(1, 30);
    let arr_ptr = h.alloc(16); // 4 + 3*4 — the exact size __json_from_cln_list would malloc
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&3i32.to_le_bytes());
    buf[4..8].copy_from_slice(&e0.to_le_bytes());
    buf[8..12].copy_from_slice(&e1.to_le_bytes());
    buf[12..16].copy_from_slice(&e2.to_le_bytes());
    h.mem_write(arr_ptr as usize, &buf);
    let boxed = h.alloc_boxed(5, arr_ptr, 0);

    let bridge_output = h.encode(boxed);
    let reference =
        serde_json::to_string(&serde_json::json!([10, 20, 30])).expect("reference encode");
    assert_eq!(bridge_output, reference);
}

// ============================================================================
// JSONTestSuite spot-check surrogate (full harness deferred to [P4a])
//
// Standard corpus files are not vendored in this repo. Instead we hand-pick
// 15 shapes that mirror JSONTestSuite y_* (valid → round-trip) and n_* (invalid
// → 0-sentinel) categories to prove the code paths exercised in the real
// harness behave correctly. Replace with a full harness in [P4a].
// ============================================================================

#[test]
fn test_json_v2_jsontestsuite_spot_check() {
    let mut h = H::new();

    // y_* valid — must decode and re-encode identically (or acceptably).
    let valid: &[&str] = &[
        r#"{}"#,
        r#"[]"#,
        r#""hello""#,
        r#"42"#,
        r#"true"#,
        r#"null"#,
        r#"[1,2,3]"#,
        r#"{"a":1}"#,
        r#"[{"k":"v"}]"#,
        r#"{"nested":{"deep":{"leaf":true}}}"#,
    ];
    for src in valid {
        let ptr = h.decode(src);
        assert!(ptr > 0, "valid input {:?} produced 0-sentinel", src);
        let re = h.encode(ptr);
        // Compare against serde_json's own re-serialization (which under
        // preserve_order gives insertion order matching the source key order).
        let via_serde =
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(src).unwrap())
                .unwrap();
        assert_eq!(re, via_serde, "roundtrip mismatch for {}", src);
    }

    // n_* invalid — must produce the 0-sentinel per D5.
    let invalid: &[&str] = &[
        r#"{"#,
        r#"[1,]"#,
        r#"{a:1}"#,
        r#"'single-quoted'"#,
        r#"undefined"#,
    ];
    for src in invalid {
        let ptr = h.decode(src);
        assert_eq!(
            ptr, 0,
            "invalid input {:?} should return 0, got {}",
            src, ptr
        );
    }
}
