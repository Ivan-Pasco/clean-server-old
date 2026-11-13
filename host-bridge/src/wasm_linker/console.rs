//! Console I/O Host Functions
//!
//! Provides print, input, and console logging functions for WASM modules.
//! These require host access to stdout/stdin.
//!
//! All functions are generic over `WasmStateCore` to work with any runtime.

use super::helpers::{
    read_raw_bytes_or_trap, read_raw_string, read_string_from_caller, write_string_to_caller,
};
use super::state::WasmStateCore;
use crate::error::BridgeResult;
use tracing::info;
use wasmtime::{Caller, Linker};

/// Write `bytes` to stdout unchanged; a failed write traps.
fn write_stdout(bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    std::io::stdout()
        .lock()
        .write_all(bytes)
        .map_err(|e| anyhow::anyhow!("writing to stdout failed: {e}"))
}

/// Register all console/IO functions with the linker
pub fn register_functions<S: WasmStateCore>(linker: &mut Linker<S>) -> BridgeResult<()> {
    // =========================================
    // PRINT FUNCTIONS
    // =========================================

    // print / print_string (ptr, len): the bytes at ptr..ptr+len written to
    // stdout unchanged; printl adds a newline. A Clean string is a byte
    // sequence, so a string holding one byte of a multi-byte UTF-8 character
    // is written as that byte (no decoding, nothing dropped). A range outside
    // the module's memory traps.
    linker.func_wrap(
        "env",
        "print",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> anyhow::Result<()> {
            write_stdout(&read_raw_bytes_or_trap(&mut caller, ptr, len)?)
        },
    )?;

    linker.func_wrap(
        "env",
        "printl",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> anyhow::Result<()> {
            let mut bytes = read_raw_bytes_or_trap(&mut caller, ptr, len)?;
            bytes.push(b'\n');
            write_stdout(&bytes)
        },
    )?;

    linker.func_wrap(
        "env",
        "print_string",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> anyhow::Result<()> {
            write_stdout(&read_raw_bytes_or_trap(&mut caller, ptr, len)?)
        },
    )?;

    // print_integer - Print integer (i64 per spec)
    linker.func_wrap("env", "print_integer", |_: Caller<'_, S>, value: i64| {
        print!("{}", value);
    })?;

    // print_float - Print float
    linker.func_wrap("env", "print_float", |_: Caller<'_, S>, value: f64| {
        print!("{}", value);
    })?;

    // print_boolean - Print boolean
    linker.func_wrap("env", "print_boolean", |_: Caller<'_, S>, value: i32| {
        print!("{}", if value != 0 { "true" } else { "false" });
    })?;

    // =========================================
    // CONSOLE LOGGING FUNCTIONS
    // =========================================

    // console_log - Log message (raw string)
    linker.func_wrap(
        "env",
        "console_log",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| {
            if let Some(s) = read_raw_string(&mut caller, ptr, len) {
                info!("[LOG] {}", s);
                println!("{}", s);
            }
        },
    )?;

    // console_error - Log error (raw string)
    linker.func_wrap(
        "env",
        "console_error",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| {
            if let Some(s) = read_raw_string(&mut caller, ptr, len) {
                eprintln!("[ERROR] {}", s);
            }
        },
    )?;

    // error - LP-format (single ptr, 4-byte length prefix) per compiler 0.30.212+ signature
    linker.func_wrap(
        "env",
        "error",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            if let Some(s) = read_string_from_caller(&mut caller, ptr) {
                eprintln!("[ERROR] {}", s);
            }
            0
        },
    )?;

    // console_warn - Log warning (raw string)
    linker.func_wrap(
        "env",
        "console_warn",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| {
            if let Some(s) = read_raw_string(&mut caller, ptr, len) {
                eprintln!("[WARN] {}", s);
            }
        },
    )?;

    // print_debug - Stderr with [DEBUG] prefix, gated on CLEAN_VERBOSE
    linker.func_wrap(
        "env",
        "print_debug",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| {
            if std::env::var("CLEAN_VERBOSE").is_ok() {
                if let Some(s) = read_raw_string(&mut caller, ptr, len) {
                    eprintln!("[DEBUG] {}", s);
                }
            }
        },
    )?;

    // print_error - Stderr with trailing newline
    linker.func_wrap(
        "env",
        "print_error",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| {
            if let Some(s) = read_raw_string(&mut caller, ptr, len) {
                eprintln!("{}", s);
            }
        },
    )?;

    // =========================================
    // INPUT FUNCTIONS
    // =========================================

    // input - Read user input (returns length-prefixed string pointer)
    // Signature: (prompt_ptr: i32) -> i32
    // Prompt is a length-prefixed pointer; in server context, returns empty string
    linker.func_wrap(
        "env",
        "input",
        |mut caller: Caller<'_, S>, _prompt_ptr: i32| -> i32 {
            write_string_to_caller(&mut caller, "")
        },
    )?;

    // console_input - Alias for input
    // Signature: (prompt_ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "console_input",
        |mut caller: Caller<'_, S>, _prompt_ptr: i32| -> i32 {
            write_string_to_caller(&mut caller, "")
        },
    )?;

    // input_integer - Read integer from user
    // Signature: (prompt_ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "input_integer",
        |_: Caller<'_, S>, _prompt_ptr: i32| -> i32 { 0 },
    )?;

    // input_float - Read float from user
    // Signature: (prompt_ptr: i32) -> f64
    linker.func_wrap(
        "env",
        "input_float",
        |_: Caller<'_, S>, _prompt_ptr: i32| -> f64 { 0.0 },
    )?;

    // input_yesno - Read yes/no from user
    // Signature: (prompt_ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "input_yesno",
        |_: Caller<'_, S>, _prompt_ptr: i32| -> i32 { 0 },
    )?;

    // input_range - Read integer in range from user
    // Signature: (prompt_ptr: i32, min: i32, max: i32, step: i32) -> i32
    linker.func_wrap(
        "env",
        "input_range",
        |_: Caller<'_, S>, _prompt_ptr: i32, min: i32, _max: i32, _step: i32| -> i32 { min },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    // Tests would require WASM runtime setup
}
