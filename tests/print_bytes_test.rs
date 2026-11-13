//! `print` / `printl` / `print_string` write a string's bytes unchanged.
//!
//! Issue #4: a Clean string is a byte sequence (stdlib-reference.md §4), so a
//! one-byte string holding part of a multi-byte UTF-8 character is a valid
//! string and must reach stdout as that byte. The print functions used to
//! decode the bytes as UTF-8 and silently write nothing when that failed, and
//! skip an out-of-range pointer instead of trapping.
//!
//! Each test runs a small module on `canary_runner`, which instantiates it
//! against the real server linker (`clean_server::bridge::create_linker`) and
//! calls its `start` export, then compares the process's stdout byte for byte.

use std::process::{Command, Output};
use tempfile::TempDir;

/// Run the WAT module `wat` on `canary_runner` and return its output.
fn run_module(wat: &str) -> Output {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("module.wat");
    std::fs::write(&path, wat).expect("write module");
    Command::new(env!("CARGO_BIN_EXE_canary_runner"))
        .arg(&path)
        .output()
        .expect("run canary_runner")
}

#[test]
fn printl_writes_each_byte_of_a_multibyte_character() {
    // `iterate ch in "héllo" { print(ch) }`: one printl per byte, including
    // the two bytes of "é" (c3 a9), neither valid UTF-8 on its own.
    let out = run_module(
        r#"(module
          (import "env" "printl" (func $printl (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 16) "h\c3\a9llo")
          (func (export "start")
            (call $printl (i32.const 16) (i32.const 1))
            (call $printl (i32.const 17) (i32.const 1))
            (call $printl (i32.const 18) (i32.const 1))
            (call $printl (i32.const 19) (i32.const 1))
            (call $printl (i32.const 20) (i32.const 1))
            (call $printl (i32.const 21) (i32.const 1))))"#,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout,
        [
            0x68, 0x0a, 0xc3, 0x0a, 0xa9, 0x0a, 0x6c, 0x0a, 0x6c, 0x0a, 0x6f, 0x0a
        ]
    );
}

#[test]
fn print_and_print_string_write_bytes_unchanged() {
    let out = run_module(
        r#"(module
          (import "env" "print" (func $print (param i32 i32)))
          (import "env" "print_string" (func $print_string (param i32 i32)))
          (import "env" "printl" (func $printl (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 16) "a\ff\c3b")
          (func (export "start")
            (call $print (i32.const 16) (i32.const 2))
            (call $print_string (i32.const 18) (i32.const 1))
            (call $print (i32.const 16) (i32.const 0))
            (call $printl (i32.const 19) (i32.const 1))))"#,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, [b'a', 0xff, 0xc3, b'b', b'\n']);
}

#[test]
fn print_out_of_bounds_range_traps() {
    for import in ["print", "printl", "print_string"] {
        let out = run_module(&format!(
            r#"(module
              (import "env" "{import}" (func $p (param i32 i32)))
              (memory (export "memory") 1)
              (func (export "start")
                (call $p (i32.const 65530) (i32.const 10))))"#
        ));
        assert_eq!(
            out.status.code(),
            Some(5),
            "{import} with an out-of-range string must trap; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{import} wrote {:?}", out.stdout);
    }
}
