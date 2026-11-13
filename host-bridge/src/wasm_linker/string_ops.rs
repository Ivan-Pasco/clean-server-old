//! String Allocation Host Functions
//!
//! Provides string operations that require memory allocation:
//! - concat, substring, trim, toUpper, toLower, replace, split
//! - Type conversions that produce strings: int_to_string, float_to_string, bool_to_string
//! - Type conversions from strings: string_to_int, string_to_float, string_to_bool
//!
//! All string parameters use length-prefixed pointers (single i32).
//! Return strings are length-prefixed [4-byte LE length][UTF-8 data].
//!
//! All functions are generic over `WasmStateCore` to work with any runtime.

use super::helpers::{
    read_raw_string, read_string_bytes_from_caller, read_string_from_caller, split_bytes,
    write_string_list_to_caller, write_string_to_caller,
};
use super::state::WasmStateCore;
use crate::error::BridgeResult;
use wasmtime::{Caller, Linker};

/// `text.split(delimiter)` for the `string_split` / `string.split` imports:
/// both arguments are length-prefixed strings read as bytes, the parts are
/// written back as a `list<string>`.
fn string_split<S: WasmStateCore>(caller: &mut Caller<'_, S>, ptr: i32, delim_ptr: i32) -> i32 {
    let text = read_string_bytes_from_caller(caller, ptr).unwrap_or_default();
    let delimiter = read_string_bytes_from_caller(caller, delim_ptr).unwrap_or_default();
    let parts = split_bytes(&text, &delimiter);
    write_string_list_to_caller(caller, &parts)
}

/// Validate a string against a compile-time integer pattern ID.
/// IDs: 0=email 1=url 2=uuid 3=phone 4=date 5=integer 6=number 7=alphanumeric
fn string_matches_by_id(s: &str, pattern_id: i32) -> bool {
    match pattern_id {
        0 => {
            // email
            let parts: Vec<&str> = s.splitn(2, '@').collect();
            parts.len() == 2 && !parts[0].is_empty() && parts[1].contains('.')
        }
        1 => s.starts_with("http://") || s.starts_with("https://"),
        2 => {
            // uuid
            let b = s.as_bytes();
            b.len() == 36
                && b[8] == b'-'
                && b[13] == b'-'
                && b[18] == b'-'
                && b[23] == b'-'
                && b.iter().enumerate().all(|(i, &c)| {
                    if i == 8 || i == 13 || i == 18 || i == 23 {
                        c == b'-'
                    } else {
                        c.is_ascii_hexdigit()
                    }
                })
        }
        3 => {
            // phone
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            digits.len() >= 7 && digits.len() <= 15
        }
        4 => {
            // date (YYYY-MM-DD)
            let parts: Vec<&str> = s.splitn(3, '-').collect();
            parts.len() == 3
                && parts[0].len() == 4
                && parts[0].chars().all(|c| c.is_ascii_digit())
                && parts[1].len() == 2
                && parts[1].chars().all(|c| c.is_ascii_digit())
                && parts[2].len() == 2
                && parts[2].chars().all(|c| c.is_ascii_digit())
        }
        5 => !s.is_empty() && s.parse::<i64>().is_ok(),
        6 => !s.is_empty() && s.parse::<f64>().is_ok(),
        7 => !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()),
        _ => false,
    }
}

/// Register all string operation functions with the linker
pub fn register_functions<S: WasmStateCore>(linker: &mut Linker<S>) -> BridgeResult<()> {
    // =========================================
    // STRING CONCATENATION
    // =========================================

    // string_concat - Concatenate two strings
    // Signature: (a_ptr: i32, b_ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_concat",
        |mut caller: Caller<'_, S>, ptr1: i32, ptr2: i32| -> i32 {
            let s1 = read_string_from_caller(&mut caller, ptr1).unwrap_or_default();
            let s2 = read_string_from_caller(&mut caller, ptr2).unwrap_or_default();
            let result = format!("{}{}", s1, s2);
            write_string_to_caller(&mut caller, &result)
        },
    )?;

    // string.concat - Dot notation alias
    linker.func_wrap(
        "env",
        "string.concat",
        |mut caller: Caller<'_, S>, ptr1: i32, ptr2: i32| -> i32 {
            let s1 = read_string_from_caller(&mut caller, ptr1).unwrap_or_default();
            let s2 = read_string_from_caller(&mut caller, ptr2).unwrap_or_default();
            let result = format!("{}{}", s1, s2);
            write_string_to_caller(&mut caller, &result)
        },
    )?;

    // =========================================
    // STRING MANIPULATION
    // =========================================

    // string_substring - Extract substring
    // Signature: (ptr: i32, start: i32, end: i32) -> i32
    linker.func_wrap(
        "env",
        "string_substring",
        |mut caller: Caller<'_, S>, ptr: i32, start: i32, end: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            let start = start.max(0) as usize;
            let end = end.max(0) as usize;

            let result = if start < s.len() && end <= s.len() && start <= end {
                &s[start..end]
            } else if start < s.len() {
                &s[start..]
            } else {
                ""
            };

            write_string_to_caller(&mut caller, result)
        },
    )?;

    // string.substring - Dot notation alias
    linker.func_wrap(
        "env",
        "string.substring",
        |mut caller: Caller<'_, S>, ptr: i32, start: i32, end: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            let start = start.max(0) as usize;
            let end = end.max(0) as usize;

            let result = if start < s.len() && end <= s.len() && start <= end {
                &s[start..end]
            } else if start < s.len() {
                &s[start..]
            } else {
                ""
            };

            write_string_to_caller(&mut caller, result)
        },
    )?;

    // string_trim - Trim whitespace from both ends
    // Signature: (ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_trim",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, s.trim())
        },
    )?;

    linker.func_wrap(
        "env",
        "string.trim",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, s.trim())
        },
    )?;

    // string_trim_start - Trim leading whitespace
    linker.func_wrap(
        "env",
        "string_trim_start",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, s.trim_start())
        },
    )?;

    linker.func_wrap(
        "env",
        "string.trimStart",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, s.trim_start())
        },
    )?;

    // string_trim_end - Trim trailing whitespace
    linker.func_wrap(
        "env",
        "string_trim_end",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, s.trim_end())
        },
    )?;

    linker.func_wrap(
        "env",
        "string.trimEnd",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, s.trim_end())
        },
    )?;

    // string_to_upper - Convert to uppercase
    // Signature: (ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_to_upper",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, &s.to_uppercase())
        },
    )?;

    linker.func_wrap(
        "env",
        "string.toUpperCase",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, &s.to_uppercase())
        },
    )?;

    linker.func_wrap(
        "env",
        "string_toUpperCase",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, &s.to_uppercase())
        },
    )?;

    // string_to_lower - Convert to lowercase
    // Signature: (ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_to_lower",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, &s.to_lowercase())
        },
    )?;

    linker.func_wrap(
        "env",
        "string.toLowerCase",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, &s.to_lowercase())
        },
    )?;

    linker.func_wrap(
        "env",
        "string_toLowerCase",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            write_string_to_caller(&mut caller, &s.to_lowercase())
        },
    )?;

    // string_replace - Replace all occurrences
    // Signature: (ptr: i32, find_ptr: i32, replace_ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_replace",
        |mut caller: Caller<'_, S>, ptr: i32, find_ptr: i32, replace_ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            let search = read_string_from_caller(&mut caller, find_ptr).unwrap_or_default();
            let replace = read_string_from_caller(&mut caller, replace_ptr).unwrap_or_default();

            if search.is_empty() {
                return write_string_to_caller(&mut caller, &s);
            }

            let result = s.replace(&search, &replace);
            write_string_to_caller(&mut caller, &result)
        },
    )?;

    linker.func_wrap(
        "env",
        "string.replace",
        |mut caller: Caller<'_, S>, ptr: i32, find_ptr: i32, replace_ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            let search = read_string_from_caller(&mut caller, find_ptr).unwrap_or_default();
            let replace = read_string_from_caller(&mut caller, replace_ptr).unwrap_or_default();

            if search.is_empty() {
                return write_string_to_caller(&mut caller, &s);
            }

            let result = s.replace(&search, &replace);
            write_string_to_caller(&mut caller, &result)
        },
    )?;

    // string_split - Split string by delimiter, returns a Clean Language list<string>
    // pointer (16-byte header + N*4-byte element pointers). See
    // `write_string_list_to_caller` for the layout. Returning a JSON-encoded
    // length-prefixed string here would break `iterate` (HOST_BRIDGE_STRING_SPLIT).
    //
    // The split works on the raw bytes (`split_bytes`): an empty delimiter
    // gives one element per byte, and `"".split("")` gives an empty list.
    // Signature: (ptr: i32, delim_ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_split",
        |mut caller: Caller<'_, S>, ptr: i32, delim_ptr: i32| -> i32 {
            string_split(&mut caller, ptr, delim_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "string.split",
        |mut caller: Caller<'_, S>, ptr: i32, delim_ptr: i32| -> i32 {
            string_split(&mut caller, ptr, delim_ptr)
        },
    )?;

    // string_index_of - Find substring index
    // Signature: (haystack_ptr: i32, needle_ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_index_of",
        |mut caller: Caller<'_, S>, haystack_ptr: i32, needle_ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, haystack_ptr).unwrap_or_default();
            let search = read_string_from_caller(&mut caller, needle_ptr).unwrap_or_default();

            s.find(&search).map(|i| i as i32).unwrap_or(-1)
        },
    )?;

    // string_compare - Compare two strings lexicographically
    // Signature: (a_ptr: i32, b_ptr: i32) -> i32
    // Returns -1 if s1 < s2, 0 if s1 == s2, 1 if s1 > s2
    linker.func_wrap(
        "env",
        "string_compare",
        |mut caller: Caller<'_, S>, a_ptr: i32, b_ptr: i32| -> i32 {
            let s1 = read_string_from_caller(&mut caller, a_ptr).unwrap_or_default();
            let s2 = read_string_from_caller(&mut caller, b_ptr).unwrap_or_default();

            match s1.cmp(&s2) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }
        },
    )?;

    // =========================================
    // TYPE CONVERSIONS (to string)
    // =========================================

    // int_to_string - Convert integer to string
    // Signature: (value: i32) -> i32
    linker.func_wrap(
        "env",
        "int_to_string",
        |mut caller: Caller<'_, S>, value: i32| -> i32 {
            let s = value.to_string();
            write_string_to_caller(&mut caller, &s)
        },
    )?;

    // integer.toString - Method style alias
    linker.func_wrap(
        "env",
        "integer.toString",
        |mut caller: Caller<'_, S>, value: i32| -> i32 {
            let s = value.to_string();
            write_string_to_caller(&mut caller, &s)
        },
    )?;

    // int64_to_string - Convert 64-bit integer to string
    // Signature: (value: i64) -> i32. Distinct from int_to_string because the
    // compiler routes `integer:64`.toString() here so the full signed-64 range
    // round-trips without narrowing through int_to_string's i32 parameter.
    linker.func_wrap(
        "env",
        "int64_to_string",
        |mut caller: Caller<'_, S>, value: i64| -> i32 {
            let s = value.to_string();
            write_string_to_caller(&mut caller, &s)
        },
    )?;

    // float_to_string - Convert float to string
    // Signature: (value: f64) -> i32
    linker.func_wrap(
        "env",
        "float_to_string",
        |mut caller: Caller<'_, S>, value: f64| -> i32 {
            let s = value.to_string();
            write_string_to_caller(&mut caller, &s)
        },
    )?;

    // number.toString - Method style alias
    linker.func_wrap(
        "env",
        "number.toString",
        |mut caller: Caller<'_, S>, value: f64| -> i32 {
            let s = value.to_string();
            write_string_to_caller(&mut caller, &s)
        },
    )?;

    // bool_to_string - Convert boolean to string
    // Signature: (value: i32) -> i32
    linker.func_wrap(
        "env",
        "bool_to_string",
        |mut caller: Caller<'_, S>, value: i32| -> i32 {
            let s = if value != 0 { "true" } else { "false" };
            write_string_to_caller(&mut caller, s)
        },
    )?;

    // boolean.toString - Method style alias
    linker.func_wrap(
        "env",
        "boolean.toString",
        |mut caller: Caller<'_, S>, value: i32| -> i32 {
            let s = if value != 0 { "true" } else { "false" };
            write_string_to_caller(&mut caller, s)
        },
    )?;

    // =========================================
    // TYPE CONVERSIONS (from string)
    // =========================================

    // string_to_int - Convert string to integer
    // Signature: (ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_to_int",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            s.trim().parse::<i32>().unwrap_or(0)
        },
    )?;

    // string.toInteger - Method style alias
    linker.func_wrap(
        "env",
        "string.toInteger",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            s.trim().parse::<i32>().unwrap_or(0)
        },
    )?;

    // string_to_float - Convert string to float
    // Signature: (ptr: i32) -> f64
    linker.func_wrap(
        "env",
        "string_to_float",
        |mut caller: Caller<'_, S>, ptr: i32| -> f64 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            s.trim().parse::<f64>().unwrap_or(0.0)
        },
    )?;

    // string.toNumber - Method style alias
    linker.func_wrap(
        "env",
        "string.toNumber",
        |mut caller: Caller<'_, S>, ptr: i32| -> f64 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            s.trim().parse::<f64>().unwrap_or(0.0)
        },
    )?;

    // string_to_bool - Convert string to boolean
    // Signature: (ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_to_bool",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            let lower = s.trim().to_lowercase();
            if lower == "true" || lower == "1" || lower == "yes" {
                1
            } else {
                0
            }
        },
    )?;

    // string.toBoolean - Method style alias
    linker.func_wrap(
        "env",
        "string.toBoolean",
        |mut caller: Caller<'_, S>, ptr: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, ptr).unwrap_or_default();
            let lower = s.trim().to_lowercase();
            if lower == "true" || lower == "1" || lower == "yes" {
                1
            } else {
                0
            }
        },
    )?;

    // =========================================
    // STRING REPEAT / MATCHES
    // =========================================

    // string_repeat - Repeat a string N times
    // Signature: (str_ptr: i32, str_len: i32, count: i32) -> i32
    // str_ptr is a length-prefixed pointer; str_len is raw length (ignored); count >= 0
    linker.func_wrap(
        "env",
        "string_repeat",
        |mut caller: Caller<'_, S>, str_ptr: i32, _str_len: i32, count: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, str_ptr).unwrap_or_default();
            let result = s.repeat(count.max(0) as usize);
            write_string_to_caller(&mut caller, &result)
        },
    )?;

    // string.repeat - Dot-notation alias
    linker.func_wrap(
        "env",
        "string.repeat",
        |mut caller: Caller<'_, S>, str_ptr: i32, _str_len: i32, count: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, str_ptr).unwrap_or_default();
            let result = s.repeat(count.max(0) as usize);
            write_string_to_caller(&mut caller, &result)
        },
    )?;

    // string_matches - Validate a string against a compile-time integer pattern ID
    // Signature: (str_ptr: i32, str_len: i32, pattern_id: i32) -> i32
    // Pattern IDs: 0=email 1=url 2=uuid 3=phone 4=date 5=integer 6=number 7=alphanumeric
    linker.func_wrap(
        "env",
        "string_matches",
        |mut caller: Caller<'_, S>, str_ptr: i32, _str_len: i32, pattern_id: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, str_ptr).unwrap_or_default();
            let matches = string_matches_by_id(&s, pattern_id);
            if matches {
                1
            } else {
                0
            }
        },
    )?;

    // string.matches - Dot-notation alias
    linker.func_wrap(
        "env",
        "string.matches",
        |mut caller: Caller<'_, S>, str_ptr: i32, _str_len: i32, pattern_id: i32| -> i32 {
            let s = read_string_from_caller(&mut caller, str_ptr).unwrap_or_default();
            let matches = string_matches_by_id(&s, pattern_id);
            if matches {
                1
            } else {
                0
            }
        },
    )?;

    // =========================================
    // HTML Escape / Raw (plugin: frame.ui)
    // =========================================

    // _html_escape - Escape HTML special characters for safe interpolation
    // Signature: (ptr: i32, len: i32) -> i32
    // Used by {var} interpolation in html: blocks
    linker.func_wrap(
        "env",
        "_html_escape",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> i32 {
            let s = match read_raw_string(&mut caller, ptr, len) {
                Some(s) => s,
                None => return write_string_to_caller(&mut caller, ""),
            };
            let escaped = s
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#039;");
            write_string_to_caller(&mut caller, &escaped)
        },
    )?;

    // _html_raw - Pass-through string for raw HTML insertion
    // Signature: (ptr: i32, len: i32) -> i32
    // Used by {!var} interpolation in html: blocks
    linker.func_wrap(
        "env",
        "_html_raw",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> i32 {
            let s = match read_raw_string(&mut caller, ptr, len) {
                Some(s) => s,
                None => return write_string_to_caller(&mut caller, ""),
            };
            write_string_to_caller(&mut caller, &s)
        },
    )?;

    // =========================================
    // STRING EXTRAS — registry "string" convention: (ptr, len) raw pairs.
    // String length matches JS String.length = UTF-16 code units.
    // =========================================

    // string_length(string) -> i32 (UTF-16 code units, matches JS)
    linker.func_wrap(
        "env",
        "string_length",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            s.encode_utf16().count() as i32
        },
    )?;

    // string_char_at(string, i32) -> ptr — 1-char LP string, "" on OOB
    linker.func_wrap(
        "env",
        "string_char_at",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32, idx: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            if idx < 0 {
                return write_string_to_caller(&mut caller, "");
            }
            let c: String = s
                .chars()
                .nth(idx as usize)
                .map(|c| c.to_string())
                .unwrap_or_default();
            write_string_to_caller(&mut caller, &c)
        },
    )?;

    // string_char_code_at(string, i32) -> i32 — UTF-16 unit at index, -1 on OOB
    linker.func_wrap(
        "env",
        "string_char_code_at",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32, idx: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            if idx < 0 {
                return -1;
            }
            s.encode_utf16()
                .nth(idx as usize)
                .map(|u| u as i32)
                .unwrap_or(-1)
        },
    )?;

    // string_from_char_code(i32) -> ptr
    linker.func_wrap(
        "env",
        "string_from_char_code",
        |mut caller: Caller<'_, S>, code: i32| -> i32 {
            let c = char::from_u32(code as u32).unwrap_or('\u{FFFD}');
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            write_string_to_caller(&mut caller, s)
        },
    )?;

    // string_contains(string, string) -> boolean
    linker.func_wrap(
        "env",
        "string_contains",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32, pat_ptr: i32, pat_len: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let pat = read_raw_string(&mut caller, pat_ptr, pat_len).unwrap_or_default();
            if s.contains(&pat) {
                1
            } else {
                0
            }
        },
    )?;

    // string_starts_with(string, string) -> boolean
    linker.func_wrap(
        "env",
        "string_starts_with",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32, pat_ptr: i32, pat_len: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let pat = read_raw_string(&mut caller, pat_ptr, pat_len).unwrap_or_default();
            if s.starts_with(&pat) {
                1
            } else {
                0
            }
        },
    )?;

    // string_ends_with(string, string) -> boolean
    linker.func_wrap(
        "env",
        "string_ends_with",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32, pat_ptr: i32, pat_len: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let pat = read_raw_string(&mut caller, pat_ptr, pat_len).unwrap_or_default();
            if s.ends_with(&pat) {
                1
            } else {
                0
            }
        },
    )?;

    // string_equals(string, string) -> boolean
    linker.func_wrap(
        "env",
        "string_equals",
        |mut caller: Caller<'_, S>, p1: i32, l1: i32, p2: i32, l2: i32| -> i32 {
            let s1 = read_raw_string(&mut caller, p1, l1).unwrap_or_default();
            let s2 = read_raw_string(&mut caller, p2, l2).unwrap_or_default();
            if s1 == s2 {
                1
            } else {
                0
            }
        },
    )?;

    // string_equals_ignore_case(string, string) -> boolean
    linker.func_wrap(
        "env",
        "string_equals_ignore_case",
        |mut caller: Caller<'_, S>, p1: i32, l1: i32, p2: i32, l2: i32| -> i32 {
            let s1 = read_raw_string(&mut caller, p1, l1).unwrap_or_default();
            let s2 = read_raw_string(&mut caller, p2, l2).unwrap_or_default();
            if s1.to_lowercase() == s2.to_lowercase() {
                1
            } else {
                0
            }
        },
    )?;

    // string_last_index_of(string, string) -> i32 (-1 if not found)
    linker.func_wrap(
        "env",
        "string_last_index_of",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32, pat_ptr: i32, pat_len: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let pat = read_raw_string(&mut caller, pat_ptr, pat_len).unwrap_or_default();
            // To match JS String#lastIndexOf, index is in UTF-16 code units.
            // Approximation: byte offset → utf16 offset of the prefix.
            match s.rfind(&pat) {
                Some(byte_idx) => s[..byte_idx].encode_utf16().count() as i32,
                None => -1,
            }
        },
    )?;

    // string_pad_start(string, i32, string) -> ptr
    linker.func_wrap(
        "env",
        "string_pad_start",
        |mut caller: Caller<'_, S>,
         ptr: i32,
         len: i32,
         target: i32,
         pad_ptr: i32,
         pad_len: i32|
         -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let pad = read_raw_string(&mut caller, pad_ptr, pad_len).unwrap_or_default();
            let target = target.max(0) as usize;
            let cur = s.chars().count();
            if cur >= target || pad.is_empty() {
                return write_string_to_caller(&mut caller, &s);
            }
            let mut prefix = String::new();
            while prefix.chars().count() + cur < target {
                prefix.push_str(&pad);
            }
            let need = target - cur;
            let trimmed: String = prefix.chars().take(need).collect();
            write_string_to_caller(&mut caller, &format!("{}{}", trimmed, s))
        },
    )?;

    // string_pad_end(string, i32, string) -> ptr
    linker.func_wrap(
        "env",
        "string_pad_end",
        |mut caller: Caller<'_, S>,
         ptr: i32,
         len: i32,
         target: i32,
         pad_ptr: i32,
         pad_len: i32|
         -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let pad = read_raw_string(&mut caller, pad_ptr, pad_len).unwrap_or_default();
            let target = target.max(0) as usize;
            let cur = s.chars().count();
            if cur >= target || pad.is_empty() {
                return write_string_to_caller(&mut caller, &s);
            }
            let mut suffix = String::new();
            while suffix.chars().count() + cur < target {
                suffix.push_str(&pad);
            }
            let need = target - cur;
            let trimmed: String = suffix.chars().take(need).collect();
            write_string_to_caller(&mut caller, &format!("{}{}", s, trimmed))
        },
    )?;

    // string_join(string, string) -> ptr
    // First string is JSON-encoded array of strings, second is the delimiter.
    linker.func_wrap(
        "env",
        "string_join",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32, d_ptr: i32, d_len: i32| -> i32 {
            let json = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let delim = read_raw_string(&mut caller, d_ptr, d_len).unwrap_or_default();
            let joined: String = match serde_json::from_str::<Vec<String>>(&json) {
                Ok(parts) => parts.join(&delim),
                Err(_) => String::new(),
            };
            write_string_to_caller(&mut caller, &joined)
        },
    )?;

    // string_reverse(string) -> ptr
    linker.func_wrap(
        "env",
        "string_reverse",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let reversed: String = s.chars().rev().collect();
            write_string_to_caller(&mut caller, &reversed)
        },
    )?;

    // string_is_empty(string) -> boolean
    linker.func_wrap(
        "env",
        "string_is_empty",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            if s.is_empty() {
                1
            } else {
                0
            }
        },
    )?;

    // string_is_blank(string) -> boolean
    linker.func_wrap(
        "env",
        "string_is_blank",
        |mut caller: Caller<'_, S>, ptr: i32, len: i32| -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            if s.trim().is_empty() {
                1
            } else {
                0
            }
        },
    )?;

    // string_replace_first(string, string, string) -> ptr
    linker.func_wrap(
        "env",
        "string_replace_first",
        |mut caller: Caller<'_, S>,
         ptr: i32,
         len: i32,
         pat_ptr: i32,
         pat_len: i32,
         rep_ptr: i32,
         rep_len: i32|
         -> i32 {
            let s = read_raw_string(&mut caller, ptr, len).unwrap_or_default();
            let pat = read_raw_string(&mut caller, pat_ptr, pat_len).unwrap_or_default();
            let rep = read_raw_string(&mut caller, rep_ptr, rep_len).unwrap_or_default();
            let result = if pat.is_empty() {
                s
            } else {
                s.replacen(&pat, &rep, 1)
            };
            write_string_to_caller(&mut caller, &result)
        },
    )?;

    // float_to_string_fixed(number, i32) -> ptr — toFixed equivalent
    linker.func_wrap(
        "env",
        "float_to_string_fixed",
        |mut caller: Caller<'_, S>, value: f64, decimals: i32| -> i32 {
            let d = decimals.clamp(0, 100) as usize;
            write_string_to_caller(&mut caller, &format!("{:.*}", d, value))
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_string_operations() {
        assert_eq!("hello world".trim(), "hello world");
        assert_eq!("  hello  ".trim(), "hello");
        assert_eq!("HELLO".to_lowercase(), "hello");
    }
}
