//! `_crypto_sha256_bytes` bridge — SHA-256 over a length-prefixed byte handle.
//!
//! Mirrors the harness pattern of `req_body_bytes_bridge_test.rs`. Covers the
//! contract the errors dashboard's tarball-upload flow depends on:
//!
//!   body_bytes(handle) → crypto.sha256_bytes(handle) → header comparison
//!
//! The bridge accepts a pointer to `[4-byte LE length][bytes]` (the same LP
//! layout `_req_body_bytes` returns and `_fs_write_bytes` consumes) and
//! returns a length-prefixed lowercase-hex string pointer (64 chars data +
//! 4-byte length prefix). Binary bytes MUST NOT be UTF-8-decoded on the path,
//! otherwise the digest wouldn't match the source.

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::WasmState;
use std::sync::Arc;
use wasmtime::{Engine, Instance, Module, Store, TypedFunc};

// A minimal host module that:
//   - imports `_crypto_sha256_bytes` and exports a wrapper `call_sha256`
//   - exports `memory` and `malloc` so tests can seed the LP buffer directly
//     and pass its pointer to the bridge.
const WAT: &str = r#"
(module
  (import "env" "_crypto_sha256_bytes" (func $sha256 (param i32) (result i32)))
  (memory (export "memory") 4)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "call_sha256") (param $handle i32) (result i32)
    (call $sha256 (local.get $handle))))
"#;

struct BridgeHarness {
    store: Store<WasmState>,
    instance: Instance,
    call: TypedFunc<i32, i32>,
    malloc: TypedFunc<i32, i32>,
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
        let call: TypedFunc<i32, i32> = instance
            .get_typed_func(&mut store, "call_sha256")
            .expect("call_sha256 export missing");
        let malloc: TypedFunc<i32, i32> = instance
            .get_typed_func(&mut store, "malloc")
            .expect("malloc export missing");
        Self {
            store,
            instance,
            call,
            malloc,
        }
    }

    /// Allocate an LP buffer (`[4-byte LE length][bytes]`) in the module's
    /// linear memory and return its handle.
    fn write_lp_buffer(&mut self, bytes: &[u8]) -> i32 {
        let total = 4 + bytes.len();
        let handle = self
            .malloc
            .call(&mut self.store, total as i32)
            .expect("malloc trapped");
        assert!(handle > 0, "malloc must return non-null pointer");
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .expect("memory export missing");
        let data = memory.data_mut(&mut self.store);
        let base = handle as usize;
        let len_le = (bytes.len() as u32).to_le_bytes();
        data[base..base + 4].copy_from_slice(&len_le);
        data[base + 4..base + 4 + bytes.len()].copy_from_slice(bytes);
        handle
    }

    /// Invoke `_crypto_sha256_bytes(handle)` and read back the lowercase-hex
    /// digest string from the returned length-prefixed pointer.
    fn sha256_hex(&mut self, bytes: &[u8]) -> String {
        let handle = self.write_lp_buffer(bytes);
        let ptr = self
            .call
            .call(&mut self.store, handle)
            .expect("call trapped");
        assert!(
            ptr > 0,
            "_crypto_sha256_bytes must return a non-null pointer (got {})",
            ptr
        );
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .expect("memory export missing");
        let data = memory.data(&self.store);
        let start = ptr as usize;
        let len_bytes: [u8; 4] = data[start..start + 4].try_into().unwrap();
        let len = u32::from_le_bytes(len_bytes) as usize;
        let payload = &data[start + 4..start + 4 + len];
        std::str::from_utf8(payload)
            .expect("digest must be valid UTF-8 hex")
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// Known SHA-256 test vectors
// ---------------------------------------------------------------------------

#[test]
fn sha256_of_empty_input_matches_nist_vector() {
    // NIST vector: SHA-256("") = e3b0c442...b855
    let mut h = BridgeHarness::new();
    let digest = h.sha256_hex(&[]);
    assert_eq!(
        digest,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(digest.len(), 64);
}

#[test]
fn sha256_of_abc_matches_nist_vector() {
    // NIST vector: SHA-256("abc") = ba7816bf...15ad
    let mut h = BridgeHarness::new();
    let digest = h.sha256_hex(b"abc");
    assert_eq!(
        digest,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn sha256_preserves_binary_bytes_including_null_and_0xff() {
    // A UTF-8 decode step on the path would replace 0xC0/0xC1/0xFF etc. with
    // U+FFFD and change the byte count — the digest would drift. Comparing
    // against the reference `sha2` crate's digest of the same input is the
    // direct way to prove the raw bytes reach the hasher untouched.
    use sha2::{Digest, Sha256};
    let bytes: Vec<u8> = vec![0x00, 0x01, 0x7f, 0x80, 0xc0, 0xc1, 0xf5, 0xff, 0x00, 0xab];
    let expected = hex::encode(Sha256::digest(&bytes));

    let mut h = BridgeHarness::new();
    let digest = h.sha256_hex(&bytes);
    assert_eq!(digest, expected);
}

#[test]
fn sha256_of_gzip_shaped_payload() {
    // The tarball-upload use case: hash a gzip stream and compare against the
    // client's X-Tarball-SHA256. Any UTF-8 detour would corrupt the digest.
    use sha2::{Digest, Sha256};
    let mut gz = vec![0u8; 4096];
    gz[0] = 0x1f;
    gz[1] = 0x8b;
    gz[2] = 0x08;
    for (i, byte) in gz.iter_mut().enumerate().skip(3) {
        *byte = ((i * 37) & 0xff) as u8;
    }
    let expected = hex::encode(Sha256::digest(&gz));

    let mut h = BridgeHarness::new();
    let digest = h.sha256_hex(&gz);
    assert_eq!(digest, expected);
}

#[test]
fn sha256_digest_is_always_64_lowercase_hex_chars() {
    let mut h = BridgeHarness::new();
    for input in [&b""[..], b"a", b"hello", &[0xff_u8; 128][..]] {
        let digest = h.sha256_hex(input);
        assert_eq!(digest.len(), 64, "digest length must be 64 chars");
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "digest must be lowercase hex: {}",
            digest
        );
    }
}
