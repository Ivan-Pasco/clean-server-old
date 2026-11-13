//! Memory Helpers for WASM Host Functions
//!
//! Provides utilities for reading and writing data to/from WASM linear memory.
//! Clean Language uses length-prefixed strings: [4-byte little-endian length][UTF-8 data]
//!
//! All functions are generic over `WasmStateCore` to work with any runtime.

use super::state::WasmStateCore;
use tracing::{debug, error};
use wasmtime::{Caller, Memory};

/// Clean string format: [4-byte little-endian length][UTF-8 bytes]
pub const STRING_LENGTH_PREFIX_SIZE: usize = 4;

/// WASM page size in bytes (64KB)
const PAGE_SIZE: usize = 65536;

/// Ensure WASM memory is large enough for a write at the given offset + size.
/// Uses 1.5x amortized growth to reduce the number of memory.grow() calls.
/// Returns true on success, false if growth fails.
fn ensure_memory_capacity<T>(store: &mut T, memory: &Memory, offset: usize, size: usize) -> bool
where
    T: wasmtime::AsContextMut,
{
    let required = offset + size;
    let current_size = memory.data_size(&*store);

    if required > current_size {
        let current_pages = memory.size(&*store);

        // 1.5x amortized growth with 4-page floor
        let target = required
            .max(current_size * 3 / 2)
            .max(current_size + 4 * PAGE_SIZE);
        let target_pages = target.div_ceil(PAGE_SIZE) as u64;
        let pages_to_grow = target_pages.saturating_sub(current_pages);

        if pages_to_grow > 0 {
            debug!(
                event = "wasm_memory_grow",
                current_pages = current_pages,
                requested_pages = pages_to_grow,
                new_total_pages = current_pages + pages_to_grow,
                "ensure_memory_capacity: growing memory (required {} bytes)",
                required
            );
            match memory.grow(&mut *store, pages_to_grow) {
                Ok(prev) => {
                    debug!(
                        "ensure_memory_capacity: grew from {} to {} pages",
                        prev,
                        prev + pages_to_grow
                    );
                }
                Err(e) => {
                    error!(
                        event = "wasm_memory_oom",
                        current_pages = current_pages,
                        requested_pages = pages_to_grow,
                        "ensure_memory_capacity: failed to grow memory: {}",
                        e
                    );
                    return false;
                }
            }
        }
    }
    true
}

/// Read the bytes of a Clean Language string from WASM memory
///
/// The string format is: [4-byte little-endian length][bytes]. The bytes are
/// returned unchanged: a Clean string is a byte sequence, so a string holding
/// part of a multi-byte UTF-8 character is still a valid string.
pub fn read_string_bytes_from_caller<S: WasmStateCore>(
    caller: &mut Caller<'_, S>,
    ptr: i32,
) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory").and_then(|e| e.into_memory())?;
    let data = memory.data(&*caller);
    let ptr = ptr as u32 as usize;

    // Check bounds for length prefix
    if ptr + STRING_LENGTH_PREFIX_SIZE > data.len() {
        error!(
            "read_string_bytes_from_caller: ptr {} out of bounds (memory size: {})",
            ptr,
            data.len()
        );
        return None;
    }

    // Read length
    let len_bytes: [u8; 4] = data[ptr..ptr + STRING_LENGTH_PREFIX_SIZE].try_into().ok()?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    // Check bounds for string data
    let data_start = ptr + STRING_LENGTH_PREFIX_SIZE;
    let data_end = data_start + len;

    if data_end > data.len() {
        error!(
            "read_string_bytes_from_caller: string data out of bounds: {}..{} (memory size: {})",
            data_start,
            data_end,
            data.len()
        );
        return None;
    }

    Some(data[data_start..data_end].to_vec())
}

/// Read a Clean Language string from WASM memory as UTF-8 text
///
/// The string format is: [4-byte little-endian length][UTF-8 bytes].
/// Returns `None` when the range is out of bounds or the bytes are not UTF-8.
pub fn read_string_from_caller<S: WasmStateCore>(
    caller: &mut Caller<'_, S>,
    ptr: i32,
) -> Option<String> {
    String::from_utf8(read_string_bytes_from_caller(caller, ptr)?).ok()
}

/// `text.split(delimiter)` on the bytes of the text.
///
/// An empty delimiter gives one part per character, a character being a byte
/// as `.length()`, `.charAt(i)` and `iterate` count it (stdlib-reference.md
/// §4); an empty text then gives no parts. A non-empty delimiter splits at
/// every non-overlapping occurrence, left to right, keeping empty parts.
pub fn split_bytes<'a>(text: &'a [u8], delimiter: &[u8]) -> Vec<&'a [u8]> {
    if delimiter.is_empty() {
        return text.chunks(1).collect();
    }
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + delimiter.len() <= text.len() {
        if text[i..].starts_with(delimiter) {
            parts.push(&text[start..i]);
            i += delimiter.len();
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Read a raw string from WASM memory (no length prefix, uses explicit length)
pub fn read_raw_string<S: WasmStateCore>(
    caller: &mut Caller<'_, S>,
    ptr: i32,
    len: i32,
) -> Option<String> {
    let memory = caller.get_export("memory").and_then(|e| e.into_memory())?;
    let data = memory.data(&*caller);

    let start = ptr as usize;
    let end = start + len as usize;

    debug!(
        "read_raw_string: ptr={}, len={}, start={}, end={}, memory_size={}",
        ptr,
        len,
        start,
        end,
        data.len()
    );

    if end > data.len() {
        error!(
            "read_raw_string: out of bounds: {}..{} (memory size: {})",
            start,
            end,
            data.len()
        );
        return None;
    }

    match std::str::from_utf8(&data[start..end]) {
        Ok(s) => {
            debug!(
                "read_raw_string: successfully read '{}' ({} bytes)",
                if s.len() > 100 {
                    format!("{}...", &s[..100])
                } else {
                    s.to_string()
                },
                s.len()
            );
            Some(s.to_string())
        }
        Err(e) => {
            error!("read_raw_string: UTF-8 conversion failed: {}", e);
            None
        }
    }
}

/// Read raw bytes from WASM memory (no length prefix, uses explicit length)
pub fn read_raw_bytes<S: WasmStateCore>(
    caller: &mut Caller<'_, S>,
    ptr: i32,
    len: i32,
) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory").and_then(|e| e.into_memory())?;
    let data = memory.data(&*caller);

    let start = ptr as usize;
    let end = start + len as usize;

    if end > data.len() {
        error!(
            "read_raw_bytes: out of bounds: {}..{} (memory size: {})",
            start,
            end,
            data.len()
        );
        return None;
    }

    Some(data[start..end].to_vec())
}

/// Write a Clean Language string to WASM memory
///
/// Returns the pointer to the written string, or 0 on failure.
///
/// This function uses WASM's malloc to allocate memory, ensuring the allocation
/// is properly tracked by WASM's memory management. This is critical because
/// WASM's allocator uses a global variable for the heap pointer, not linear memory.
///
/// IMPORTANT: After allocation, we explicitly update the __heap_ptr global to ensure
/// subsequent WASM allocations don't overlap with this allocation. This is needed
/// because re-entrant malloc calls may not always properly update the global.
pub fn write_string_to_caller<S: WasmStateCore>(caller: &mut Caller<'_, S>, s: &str) -> i32 {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let total_size = STRING_LENGTH_PREFIX_SIZE + len;

    debug!(
        "write_string_to_caller: Writing string ({} bytes, total_size={})",
        len, total_size
    );

    // Read __heap_ptr BEFORE malloc to track heap state
    let heap_ptr_before = if let Some(heap_global) = caller
        .get_export("__heap_ptr")
        .and_then(|e| e.into_global())
    {
        heap_global.get(&mut *caller).i32().unwrap_or(-1)
    } else {
        -1
    };
    debug!(
        "write_string_to_caller: __heap_ptr BEFORE malloc = {}",
        heap_ptr_before
    );

    // MUST use WASM's malloc to allocate - updating memory[0] doesn't work
    // because WASM's allocator uses a global variable, not linear memory.
    let ptr = if let Some(malloc) = caller.get_export("malloc").and_then(|e| e.into_func()) {
        debug!(
            "write_string_to_caller: Found malloc export, calling WASM malloc({})",
            total_size
        );
        match malloc.typed::<i32, i32>(&*caller) {
            Ok(typed_malloc) => match typed_malloc.call(&mut *caller, total_size as i32) {
                Ok(p) if p > 0 => {
                    debug!(
                        "write_string_to_caller: WASM malloc({}) returned ptr={}",
                        total_size, p
                    );
                    p as usize
                }
                Ok(p) => {
                    error!(
                        "write_string_to_caller: WASM malloc returned invalid ptr={}",
                        p
                    );
                    return 0;
                }
                Err(e) => {
                    error!("write_string_to_caller: WASM malloc call failed: {}", e);
                    return 0;
                }
            },
            Err(e) => {
                error!("write_string_to_caller: malloc type mismatch: {}", e);
                return 0;
            }
        }
    } else {
        error!("write_string_to_caller: No malloc export found");
        return 0;
    };

    // Read __heap_ptr AFTER malloc to verify it was updated
    let heap_ptr_after = if let Some(heap_global) = caller
        .get_export("__heap_ptr")
        .and_then(|e| e.into_global())
    {
        heap_global.get(&mut *caller).i32().unwrap_or(-1)
    } else {
        -1
    };
    debug!(
        "write_string_to_caller: __heap_ptr AFTER malloc = {}",
        heap_ptr_after
    );

    // Calculate expected heap pointer after allocation (with 8-byte alignment)
    let expected_heap_ptr = ((ptr + total_size + 7) & !7) as i32;
    if heap_ptr_after >= 0 && heap_ptr_after < expected_heap_ptr {
        error!("write_string_to_caller: HEAP POINTER NOT PROPERLY UPDATED!");
        error!(
            "  malloc returned ptr={}, allocated {} bytes",
            ptr, total_size
        );
        error!(
            "  __heap_ptr is {} but should be at least {}",
            heap_ptr_after, expected_heap_ptr
        );
        error!("  This will cause memory overlap with subsequent allocations!");

        // FIX: Manually update __heap_ptr to prevent overlap
        if let Some(heap_global) = caller
            .get_export("__heap_ptr")
            .and_then(|e| e.into_global())
        {
            if let Err(e) = heap_global.set(&mut *caller, wasmtime::Val::I32(expected_heap_ptr)) {
                error!("write_string_to_caller: Failed to update __heap_ptr: {}", e);
            } else {
                debug!(
                    "write_string_to_caller: Manually updated __heap_ptr to {}",
                    expected_heap_ptr
                );
            }
        }
    }

    // Re-acquire memory reference after malloc call - malloc might have grown memory
    // and we need the updated memory view
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => {
            error!("write_string_to_caller: No memory export found after malloc");
            return 0;
        }
    };

    // Ensure memory is large enough for the write (WASM malloc doesn't grow memory)
    if !ensure_memory_capacity(caller, &memory, ptr, total_size) {
        error!(
            "write_string_to_caller: Failed to ensure memory capacity for {} bytes at ptr={}",
            total_size, ptr
        );
        return 0;
    }

    // Write length prefix (4 bytes, little-endian)
    let len_bytes = (len as u32).to_le_bytes();
    if let Err(e) = memory.write(&mut *caller, ptr, &len_bytes) {
        error!(
            "write_string_to_caller: Failed to write length at ptr={}: {}",
            ptr, e
        );
        return 0;
    }

    // Write string data
    if let Err(e) = memory.write(&mut *caller, ptr + STRING_LENGTH_PREFIX_SIZE, bytes) {
        error!(
            "write_string_to_caller: Failed to write string data at ptr={}: {}",
            ptr + STRING_LENGTH_PREFIX_SIZE,
            e
        );
        return 0;
    }

    debug!(
        "write_string_to_caller: Successfully wrote {} bytes at ptr={}",
        len, ptr
    );
    ptr as i32
}

/// Write bytes to WASM memory with length prefix
/// CRITICAL: Uses WASM malloc to allocate memory, ensuring coordination with WASM's heap.
/// Previously used allocate_at_memory_end which caused memory corruption when overlapping
/// with WASM's heap (State allocator starts at 65536, WASM heap grows from ~1024).
///
/// IMPORTANT: After allocation, we explicitly update the __heap_ptr global to ensure
/// subsequent WASM allocations don't overlap with this allocation.
pub fn write_bytes_to_caller<S: WasmStateCore>(caller: &mut Caller<'_, S>, bytes: &[u8]) -> i32 {
    let len = bytes.len();
    let total_size = STRING_LENGTH_PREFIX_SIZE + len;

    debug!(
        "write_bytes_to_caller: Writing {} bytes (total_size={})",
        len, total_size
    );

    // Read __heap_ptr BEFORE malloc to track heap state
    let heap_ptr_before = if let Some(heap_global) = caller
        .get_export("__heap_ptr")
        .and_then(|e| e.into_global())
    {
        heap_global.get(&mut *caller).i32().unwrap_or(-1)
    } else {
        -1
    };
    debug!(
        "write_bytes_to_caller: __heap_ptr BEFORE malloc = {}",
        heap_ptr_before
    );

    // MUST use WASM's malloc to allocate - using State allocator causes memory overlap
    // because WASM's allocator uses a global variable that grows from ~1024 upward,
    // while State's allocator starts at 65536 and grows upward - they can overlap!
    let ptr = if let Some(malloc) = caller.get_export("malloc").and_then(|e| e.into_func()) {
        debug!(
            "write_bytes_to_caller: Found malloc, calling WASM malloc({})",
            total_size
        );
        match malloc.typed::<i32, i32>(&*caller) {
            Ok(typed_malloc) => match typed_malloc.call(&mut *caller, total_size as i32) {
                Ok(p) if p > 0 => {
                    debug!(
                        "write_bytes_to_caller: WASM malloc({}) returned ptr={}",
                        total_size, p
                    );
                    p as usize
                }
                Ok(p) => {
                    error!(
                        "write_bytes_to_caller: WASM malloc returned invalid ptr={}",
                        p
                    );
                    return 0;
                }
                Err(e) => {
                    error!("write_bytes_to_caller: WASM malloc call failed: {}", e);
                    return 0;
                }
            },
            Err(e) => {
                error!("write_bytes_to_caller: malloc type mismatch: {}", e);
                return 0;
            }
        }
    } else {
        error!("write_bytes_to_caller: No malloc export found");
        return 0;
    };

    // Read __heap_ptr AFTER malloc to verify it was updated
    let heap_ptr_after = if let Some(heap_global) = caller
        .get_export("__heap_ptr")
        .and_then(|e| e.into_global())
    {
        heap_global.get(&mut *caller).i32().unwrap_or(-1)
    } else {
        -1
    };
    debug!(
        "write_bytes_to_caller: __heap_ptr AFTER malloc = {}",
        heap_ptr_after
    );

    // Calculate expected heap pointer after allocation (with 8-byte alignment)
    let expected_heap_ptr = ((ptr + total_size + 7) & !7) as i32;
    if heap_ptr_after >= 0 && heap_ptr_after < expected_heap_ptr {
        error!("write_bytes_to_caller: HEAP POINTER NOT PROPERLY UPDATED!");
        error!(
            "  malloc returned ptr={}, allocated {} bytes",
            ptr, total_size
        );
        error!(
            "  __heap_ptr is {} but should be at least {}",
            heap_ptr_after, expected_heap_ptr
        );
        error!("  This will cause memory overlap with subsequent allocations!");

        // FIX: Manually update __heap_ptr to prevent overlap
        if let Some(heap_global) = caller
            .get_export("__heap_ptr")
            .and_then(|e| e.into_global())
        {
            if let Err(e) = heap_global.set(&mut *caller, wasmtime::Val::I32(expected_heap_ptr)) {
                error!("write_bytes_to_caller: Failed to update __heap_ptr: {}", e);
            } else {
                debug!(
                    "write_bytes_to_caller: Manually updated __heap_ptr to {}",
                    expected_heap_ptr
                );
            }
        }
    }

    // Re-acquire memory reference after malloc call - malloc might have grown memory
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => {
            error!("write_bytes_to_caller: No memory export found after malloc");
            return 0;
        }
    };

    // Ensure memory is large enough for the write (WASM malloc doesn't grow memory)
    if !ensure_memory_capacity(caller, &memory, ptr, total_size) {
        error!(
            "write_bytes_to_caller: Failed to ensure memory capacity for {} bytes at ptr={}",
            total_size, ptr
        );
        return 0;
    }

    // Write length prefix
    let len_bytes = (len as u32).to_le_bytes();
    if let Err(e) = memory.write(&mut *caller, ptr, &len_bytes) {
        error!("write_bytes_to_caller: Failed to write length: {}", e);
        return 0;
    }

    // Write data
    if let Err(e) = memory.write(&mut *caller, ptr + STRING_LENGTH_PREFIX_SIZE, bytes) {
        error!("write_bytes_to_caller: Failed to write data: {}", e);
        return 0;
    }

    debug!(
        "write_bytes_to_caller: Successfully wrote {} bytes at ptr={}",
        len, ptr
    );
    ptr as i32
}

/// Write a Clean Language `list<string>` to WASM memory.
///
/// Layout (matches the compiler's iterate / list ops codegen):
///   offset  0..4  : length     (u32 LE)
///   offset  4..8  : capacity   (u32 LE)
///   offset  8..12 : type_id    (u32 LE, 3 = string)
///   offset 12..16 : padding    (u32 LE, 0)
///   offset 16..   : N * 4-byte LE pointers to length-prefixed strings
///
/// Each element is a pointer to a string allocated via `write_bytes_to_caller`
/// (so the strings live in WASM-malloc'd memory just like other host-produced
/// strings). The list header block is allocated via WASM malloc as well.
///
/// Each part is a byte string written unchanged (a part may hold a single
/// byte of a multi-byte UTF-8 character, e.g. from `split("")`).
///
/// Returns the pointer to the list block, or 0 on failure.
///
/// CRITICAL: Prior to this helper, `string_split` returned a JSON-encoded string
/// instead of a list pointer. The compiler emits `iterate part in parts` code
/// that reads the size at offset 0 and element pointers at offset 16 + i*4 —
/// so feeding it a JSON-LP string caused the iterate to walk JSON bytes and
/// either over-run (wrong count) or WASM-trap. See HOST_BRIDGE_STRING_SPLIT.
pub fn write_string_list_to_caller<S: WasmStateCore>(
    caller: &mut Caller<'_, S>,
    parts: &[&[u8]],
) -> i32 {
    const LIST_HEADER_SIZE: usize = 16;
    const ELEM_PTR_SIZE: usize = 4;

    let num_parts = parts.len();

    // Allocate each element string via the existing helper (uses WASM malloc
    // under the hood, so the strings live alongside other host-produced strings
    // and __heap_ptr stays consistent).
    let mut element_ptrs: Vec<i32> = Vec::with_capacity(num_parts);
    for part in parts {
        let p = write_bytes_to_caller(caller, part);
        if p == 0 {
            error!("write_string_list_to_caller: failed to allocate element string");
            return 0;
        }
        element_ptrs.push(p);
    }

    // Allocate the list block (header + N pointer slots) via WASM malloc.
    let list_size = LIST_HEADER_SIZE + num_parts * ELEM_PTR_SIZE;

    let list_ptr = if let Some(malloc) = caller.get_export("malloc").and_then(|e| e.into_func()) {
        match malloc.typed::<i32, i32>(&*caller) {
            Ok(typed_malloc) => match typed_malloc.call(&mut *caller, list_size as i32) {
                Ok(p) if p > 0 => p as usize,
                Ok(p) => {
                    error!(
                        "write_string_list_to_caller: malloc returned invalid ptr={}",
                        p
                    );
                    return 0;
                }
                Err(e) => {
                    error!("write_string_list_to_caller: malloc call failed: {}", e);
                    return 0;
                }
            },
            Err(e) => {
                error!("write_string_list_to_caller: malloc type mismatch: {}", e);
                return 0;
            }
        }
    } else {
        error!("write_string_list_to_caller: No malloc export found");
        return 0;
    };

    // Defensive __heap_ptr fix-up: mirrors write_string_to_caller's pattern so
    // any malloc that fails to bump the global doesn't cause later overlap.
    let expected_heap_ptr = ((list_ptr + list_size + 7) & !7) as i32;
    if let Some(heap_global) = caller
        .get_export("__heap_ptr")
        .and_then(|e| e.into_global())
    {
        let actual = heap_global.get(&mut *caller).i32().unwrap_or(-1);
        if actual >= 0 && actual < expected_heap_ptr {
            if let Err(e) = heap_global.set(&mut *caller, wasmtime::Val::I32(expected_heap_ptr)) {
                error!(
                    "write_string_list_to_caller: failed to update __heap_ptr: {}",
                    e
                );
            }
        }
    }

    // Re-acquire memory after malloc (it may have grown).
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => {
            error!("write_string_list_to_caller: No memory export after malloc");
            return 0;
        }
    };

    if !ensure_memory_capacity(caller, &memory, list_ptr, list_size) {
        error!(
            "write_string_list_to_caller: Failed to ensure memory capacity ({} bytes at ptr={})",
            list_size, list_ptr
        );
        return 0;
    }

    // Build the 16-byte header.
    let mut header = [0u8; LIST_HEADER_SIZE];
    header[0..4].copy_from_slice(&(num_parts as u32).to_le_bytes()); // length
    header[4..8].copy_from_slice(&(num_parts as u32).to_le_bytes()); // capacity
    header[8..12].copy_from_slice(&3u32.to_le_bytes()); // type_id (3 = string)
    header[12..16].copy_from_slice(&0u32.to_le_bytes()); // padding

    if let Err(e) = memory.write(&mut *caller, list_ptr, &header) {
        error!(
            "write_string_list_to_caller: failed to write list header: {}",
            e
        );
        return 0;
    }

    // Write element pointers.
    for (i, &elem_ptr) in element_ptrs.iter().enumerate() {
        let ofs = list_ptr + LIST_HEADER_SIZE + i * ELEM_PTR_SIZE;
        let bytes = (elem_ptr as u32).to_le_bytes();
        if let Err(e) = memory.write(&mut *caller, ofs, &bytes) {
            error!(
                "write_string_list_to_caller: failed to write element ptr {} at ofs {}: {}",
                i, ofs, e
            );
            return 0;
        }
    }

    debug!(
        "write_string_list_to_caller: wrote list of {} string(s) at ptr={}",
        num_parts, list_ptr
    );
    list_ptr as i32
}

/// Read a length-prefixed byte array from WASM memory (from raw data slice)
pub fn read_length_prefixed_bytes(data: &[u8], ptr: usize) -> Vec<u8> {
    if ptr + STRING_LENGTH_PREFIX_SIZE > data.len() {
        return Vec::new();
    }

    let len_bytes: [u8; 4] = match data[ptr..ptr + STRING_LENGTH_PREFIX_SIZE].try_into() {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let len = u32::from_le_bytes(len_bytes) as usize;

    let data_start = ptr + STRING_LENGTH_PREFIX_SIZE;
    let data_end = data_start + len;

    if data_end > data.len() {
        return Vec::new();
    }

    data[data_start..data_end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_length_prefixed_bytes() {
        // Create test data: length prefix (5) + "hello"
        let mut data = vec![5, 0, 0, 0]; // length = 5 (little endian)
        data.extend_from_slice(b"hello");

        let result = read_length_prefixed_bytes(&data, 0);
        assert_eq!(result, b"hello");
    }

    #[test]
    fn test_read_length_prefixed_bytes_empty() {
        // Empty string
        let data = vec![0, 0, 0, 0]; // length = 0
        let result = read_length_prefixed_bytes(&data, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn test_read_length_prefixed_bytes_out_of_bounds() {
        let data = vec![10, 0, 0, 0]; // length = 10, but no data
        let result = read_length_prefixed_bytes(&data, 0);
        assert!(result.is_empty()); // Should return empty on out of bounds
    }

    #[test]
    fn split_bytes_empty_delimiter_gives_one_part_per_byte() {
        let parts = split_bytes(b"abc", b"");
        assert_eq!(parts, vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
    }

    #[test]
    fn split_bytes_empty_text_and_empty_delimiter_gives_no_parts() {
        assert!(split_bytes(b"", b"").is_empty());
    }

    #[test]
    fn split_bytes_empty_delimiter_splits_multibyte_characters_into_bytes() {
        // "h\u{e9}" is 3 bytes: 'h', 0xC3, 0xA9
        let parts = split_bytes("h\u{e9}".as_bytes(), b"");
        assert_eq!(parts, vec![&b"h"[..], &[0xC3u8][..], &[0xA9u8][..]]);
    }

    #[test]
    fn split_bytes_non_empty_delimiter_keeps_empty_parts() {
        assert_eq!(
            split_bytes(b",a,,b,", b","),
            vec![&b""[..], &b"a"[..], &b""[..], &b"b"[..], &b""[..]]
        );
        assert_eq!(
            split_bytes(b"a```b```c", b"```"),
            vec![&b"a"[..], &b"b"[..], &b"c"[..]]
        );
        assert_eq!(split_bytes(b"", b","), vec![&b""[..]]);
        assert_eq!(split_bytes(b"abc", b"x"), vec![&b"abc"[..]]);
    }
}
