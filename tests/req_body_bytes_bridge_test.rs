//! `_req_body_bytes` bridge — raw request body byte access.
//!
//! Covers the guarantees the errors dashboard's POST
//! /api/v1/reports/tarball-upload endpoint depends on:
//!
//! - Binary payloads (0x00, 0xFF, invalid UTF-8) survive the round-trip
//!   through `RequestContext.body_bytes` verbatim.
//! - Returned LP-buffer length equals the source byte length (which handlers
//!   can compare against `Content-Length`).
//! - Empty bodies return a zero-length buffer with a valid pointer, not 0/null.
//! - When `body_bytes` is `None`, `_req_body_bytes` falls back to the UTF-8
//!   bytes of `body` — keeps text-only handlers working.
//! - `_req_body` still returns the string surface when both are populated
//!   (additive contract, no regression).
//!
//! The tests build a minimal WASM host module (via WAT) that exports `memory`
//! and `malloc`, imports `_req_body_bytes`, and exposes a call helper. Each
//! test seeds `state.request_context.body_bytes`, invokes the helper, then
//! reads back the [4-byte LE length][bytes] LP buffer from the module's
//! linear memory.

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::{RequestContext, WasmState};
use std::sync::Arc;
use wasmtime::{Engine, Instance, Module, Store, TypedFunc};

// ---------------------------------------------------------------------------
// Test module
// ---------------------------------------------------------------------------
//
// A minimal host module that:
//   - imports the `_req_body_bytes` bridge
//   - exports `memory` and `malloc` (a bump allocator) so the bridge can
//     allocate the LP buffer with the same allocation convention it uses
//     against real Clean-compiled modules
//   - exports `call_req_body_bytes` — thin wrapper the test invokes to
//     get the LP-buffer pointer back
//
// The bump allocator starts at offset 1024 and returns the current cursor,
// then advances by `size`. This mirrors the layout convention documented in
// foundation/spec/platform/MEMORY_MODEL.md — the first page is
// reserved for the runtime, allocations grow from 1024 upward.
const WAT: &str = r#"
(module
  (import "env" "_req_body_bytes" (func $req_body_bytes (result i32)))
  (memory (export "memory") 4)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "call_req_body_bytes") (result i32)
    (call $req_body_bytes)))
"#;

struct BridgeHarness {
    store: Store<WasmState>,
    instance: Instance,
    call: TypedFunc<(), i32>,
}

impl BridgeHarness {
    fn new() -> Self {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("failed to create server linker");
        let module = Module::new(&engine, WAT).expect("failed to compile test WAT module");
        let router = Arc::new(Router::new());
        let mut store = Store::new(&engine, WasmState::new(router));
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("failed to instantiate test module");
        let call: TypedFunc<(), i32> = instance
            .get_typed_func(&mut store, "call_req_body_bytes")
            .expect("call_req_body_bytes export missing");
        Self {
            store,
            instance,
            call,
        }
    }

    fn set_body_bytes(&mut self, bytes: Vec<u8>) {
        self.store.data_mut().request_context = Some(RequestContext {
            method: "POST".to_string(),
            path: "/upload".to_string(),
            headers: Vec::new(),
            body: String::new(),
            body_bytes: Some(bytes),
            params: Default::default(),
            query: Default::default(),
        });
    }

    fn set_body_string(&mut self, s: &str) {
        self.store.data_mut().request_context = Some(RequestContext {
            method: "POST".to_string(),
            path: "/upload".to_string(),
            headers: Vec::new(),
            body: s.to_string(),
            body_bytes: None,
            params: Default::default(),
            query: Default::default(),
        });
    }

    fn set_body_bytes_and_string(&mut self, bytes: Vec<u8>, s: &str) {
        self.store.data_mut().request_context = Some(RequestContext {
            method: "POST".to_string(),
            path: "/upload".to_string(),
            headers: Vec::new(),
            body: s.to_string(),
            body_bytes: Some(bytes),
            params: Default::default(),
            query: Default::default(),
        });
    }

    fn clear_context(&mut self) {
        self.store.data_mut().request_context = None;
    }

    /// Invoke `_req_body_bytes` and return the resulting `(pointer, bytes)`
    /// pair extracted from linear memory via the LP layout.
    fn invoke_and_read(&mut self) -> (i32, Vec<u8>) {
        let ptr = self.call.call(&mut self.store, ()).expect("call trapped");
        assert!(
            ptr > 0,
            "_req_body_bytes must return a non-null pointer (got {})",
            ptr
        );
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .expect("memory export missing");
        let data = memory.data(&self.store);
        let start = ptr as usize;
        assert!(
            start + 4 <= data.len(),
            "LP header out of bounds: ptr={} memory_size={}",
            start,
            data.len()
        );
        let len_bytes: [u8; 4] = data[start..start + 4].try_into().unwrap();
        let len = u32::from_le_bytes(len_bytes) as usize;
        let payload_start = start + 4;
        let payload_end = payload_start + len;
        assert!(
            payload_end <= data.len(),
            "LP payload out of bounds: {}..{} memory_size={}",
            payload_start,
            payload_end,
            data.len()
        );
        (ptr, data[payload_start..payload_end].to_vec())
    }
}

// ---------------------------------------------------------------------------
// Binary payload preservation
// ---------------------------------------------------------------------------

#[test]
fn returns_raw_bytes_verbatim_with_null_and_0xff() {
    let mut h = BridgeHarness::new();
    let binary: Vec<u8> = vec![0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff, 0x00, 0xab];
    h.set_body_bytes(binary.clone());
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(out.len(), binary.len());
    assert_eq!(out, binary);
}

#[test]
fn preserves_invalid_utf8_sequences() {
    let mut h = BridgeHarness::new();
    // 0xC0 and 0xC1 are never valid UTF-8 lead bytes; 0xF5..=0xFF cannot start
    // a valid sequence either. Any UTF-8 decode step between wire and bridge
    // would replace them with U+FFFD (3 bytes each) and change the byte count,
    // so SHA-256 over the result would no longer match the source.
    let invalid_utf8: Vec<u8> = vec![0xc0, 0xc1, 0xf5, 0xf6, 0xf7, 0xff];
    h.set_body_bytes(invalid_utf8.clone());
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(out, invalid_utf8);
}

#[test]
fn preserves_gzip_magic_and_tar_gz_shaped_payload() {
    let mut h = BridgeHarness::new();
    // 0x1F 0x8B = gzip magic; 0x08 = deflate method. The tarball-upload path
    // hashes the body to enforce the contract's integrity check — these first
    // three bytes must survive intact.
    let mut gz = vec![0u8; 64];
    gz[0] = 0x1f;
    gz[1] = 0x8b;
    gz[2] = 0x08;
    for (i, byte) in gz.iter_mut().enumerate().skip(3) {
        *byte = ((i * 37) & 0xff) as u8;
    }
    h.set_body_bytes(gz.clone());
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(out.len(), 64);
    assert_eq!(&out[..3], &[0x1f, 0x8b, 0x08]);
    assert_eq!(out, gz);
}

#[test]
fn preserves_all_byte_values_0_through_ff() {
    // Every possible byte value round-trips. A UTF-8 detour would expand
    // 0x80..=0xFF into multi-byte replacement chars.
    let mut h = BridgeHarness::new();
    let all_bytes: Vec<u8> = (0u8..=255).collect();
    h.set_body_bytes(all_bytes.clone());
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(out.len(), 256);
    assert_eq!(out, all_bytes);
}

// ---------------------------------------------------------------------------
// Length semantics
// ---------------------------------------------------------------------------

#[test]
fn zero_length_body_returns_valid_pointer_with_length_zero() {
    let mut h = BridgeHarness::new();
    h.set_body_bytes(Vec::new());
    let (ptr, out) = h.invoke_and_read();
    // Non-null pointer (allocation still happened for the 4-byte header).
    assert!(ptr > 0);
    assert!(out.is_empty());
}

#[test]
fn four_kb_payload_integrity_and_length_prefix() {
    let mut h = BridgeHarness::new();
    let payload: Vec<u8> = (0..4096).map(|i| (i & 0xff) as u8).collect();
    h.set_body_bytes(payload.clone());
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(out.len(), 4096);
    // Spot-check the interior — a UTF-8 detour would have expanded high bytes.
    assert_eq!(out[128], 128);
    assert_eq!(out[255], 255);
    assert_eq!(out[4095], (4095u32 & 0xff) as u8);
    assert_eq!(out, payload);
}

#[test]
fn returned_length_equals_content_length_for_binary_payload() {
    // The registry description promises: "When Content-Length is set on the
    // request, the returned length equals it." Content-Length equals the byte
    // count on the wire, which is exactly what body_bytes holds.
    let mut h = BridgeHarness::new();
    let payload: Vec<u8> = (0..1234).map(|i| (i as u8) ^ 0xa5).collect();
    let content_length = payload.len();
    h.set_body_bytes(payload);
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(out.len(), content_length);
}

// ---------------------------------------------------------------------------
// Fallback to `body` UTF-8 when `body_bytes` is None
// ---------------------------------------------------------------------------

#[test]
fn falls_back_to_utf8_of_body_when_body_bytes_is_none() {
    let mut h = BridgeHarness::new();
    h.set_body_string("hello world");
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(std::str::from_utf8(&out).unwrap(), "hello world");
    assert_eq!(out.len(), 11);
}

#[test]
fn fallback_handles_multibyte_utf8_correctly() {
    let mut h = BridgeHarness::new();
    // 'é' is 2 UTF-8 bytes (0xC3 0xA9); '文' is 3 bytes (0xE6 0x96 0x87).
    h.set_body_string("café文");
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(out.len(), 3 + 2 + 3);
    assert_eq!(std::str::from_utf8(&out).unwrap(), "café文");
}

// ---------------------------------------------------------------------------
// Additive to `_req_body` — no regression, both surfaces coexist
// ---------------------------------------------------------------------------

#[test]
fn body_bytes_and_body_string_coexist_on_same_request() {
    // The wire body was binary; the handler happens to also carry a
    // legacy string view (empty or lossy). `_req_body_bytes` must return
    // the raw bytes, not the string surface's UTF-8 encoding.
    let mut h = BridgeHarness::new();
    let raw = vec![0x00u8, 0xff, 0x1f, 0x8b];
    h.set_body_bytes_and_string(raw.clone(), "legacy-string-view");
    let (_ptr, out) = h.invoke_and_read();
    assert_eq!(out, raw);
    assert_ne!(out, b"legacy-string-view");
}

#[test]
fn no_request_context_returns_empty_lp_buffer_not_null() {
    // The Rust host tolerates a missing context by returning an empty LP
    // buffer — matches the contract that empty bodies still allocate a
    // valid header rather than 0.
    let mut h = BridgeHarness::new();
    h.clear_context();
    let (ptr, out) = h.invoke_and_read();
    assert!(ptr > 0);
    assert!(out.is_empty());
}
