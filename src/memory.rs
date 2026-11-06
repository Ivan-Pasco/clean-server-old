//! WASM Memory Management
//!
//! Handles reading/writing strings and data between Rust and WASM memory.
//! Clean Language uses length-prefixed strings: [4-byte length][UTF-8 data]

use crate::error::{RuntimeError, RuntimeResult};
use tracing::{debug, warn};
use wasmtime::{Caller, Memory, Store};

/// Clean string format: [4-byte little-endian length][UTF-8 bytes]
pub const STRING_LENGTH_PREFIX_SIZE: usize = 4;

/// Minimum allocation alignment (8 bytes)
pub const ALIGNMENT: usize = 8;

/// Memory manager for WASM instance (bump allocator)
pub struct WasmMemory {
    /// Current allocation offset
    offset: usize,
    /// Initial offset (read from __heap_ptr or default 65536)
    initial_offset: usize,
    /// Number of memory.grow() calls during this allocation cycle
    grow_count: u32,
    /// Peak allocation offset seen during this cycle
    peak_offset: usize,
}

impl WasmMemory {
    /// Create a new memory manager with default 64KB initial offset
    pub fn new() -> Self {
        Self::with_initial_offset(65536)
    }

    /// Create a new memory manager with a specific initial offset
    pub fn with_initial_offset(initial_offset: usize) -> Self {
        Self {
            offset: initial_offset,
            initial_offset,
            grow_count: 0,
            peak_offset: initial_offset,
        }
    }

    /// Allocate memory and return the pointer
    pub fn allocate(&mut self, size: usize) -> usize {
        let ptr = self.offset;
        // Align to 8 bytes for safety
        self.offset = (self.offset + size + ALIGNMENT - 1) & !(ALIGNMENT - 1);
        if self.offset > self.peak_offset {
            self.peak_offset = self.offset;
        }
        ptr
    }

    /// Reset allocator (for between requests)
    pub fn reset(&mut self) {
        self.offset = self.initial_offset;
        self.grow_count = 0;
        self.peak_offset = self.initial_offset;
    }

    /// Set the initial offset (e.g., from __heap_ptr)
    pub fn set_offset(&mut self, offset: usize) {
        self.initial_offset = offset;
        self.offset = offset;
        self.peak_offset = offset;
    }

    /// Get current allocation offset
    pub fn current_offset(&self) -> usize {
        self.offset
    }

    /// Record a memory.grow() event
    pub fn record_grow(&mut self) {
        self.grow_count += 1;
    }

    /// Get the number of memory.grow() calls this cycle
    pub fn grow_count(&self) -> u32 {
        self.grow_count
    }

    /// Get the peak allocation offset this cycle
    pub fn peak_offset(&self) -> usize {
        self.peak_offset
    }
}

impl Default for WasmMemory {
    fn default() -> Self {
        Self::new()
    }
}

/// Write a Clean Language string to WASM memory
/// Returns the pointer to the string in WASM memory
pub fn write_string_to_memory<T>(
    store: &mut Store<T>,
    memory: &Memory,
    allocator: &mut WasmMemory,
    s: &str,
) -> RuntimeResult<u32> {
    let bytes = s.as_bytes();
    let total_size = STRING_LENGTH_PREFIX_SIZE + bytes.len();

    // Allocate memory
    let ptr = allocator.allocate(total_size);

    // Ensure memory is large enough
    ensure_memory_size(store, memory, ptr + total_size)?;

    // Write length prefix (little-endian u32)
    let len_bytes = (bytes.len() as u32).to_le_bytes();
    memory
        .write(&mut *store, ptr, &len_bytes)
        .map_err(|e| RuntimeError::memory(format!("Failed to write string length: {}", e)))?;

    // Write string data
    memory
        .write(&mut *store, ptr + STRING_LENGTH_PREFIX_SIZE, bytes)
        .map_err(|e| RuntimeError::memory(format!("Failed to write string data: {}", e)))?;

    Ok(ptr as u32)
}

/// Read a Clean Language string from WASM memory
pub fn read_string_from_memory<T>(
    store: &Store<T>,
    memory: &Memory,
    ptr: u32,
) -> RuntimeResult<String> {
    use tracing::{debug, error};

    let data = memory.data(store);
    let ptr = ptr as usize;

    debug!(
        "read_string_from_memory: ptr={}, memory_size={}",
        ptr,
        data.len()
    );

    // Check bounds for length prefix
    if ptr + STRING_LENGTH_PREFIX_SIZE > data.len() {
        return Err(RuntimeError::memory(format!(
            "String pointer {} out of bounds (memory size: {})",
            ptr,
            data.len()
        )));
    }

    // Read length
    let len_bytes: [u8; 4] = data[ptr..ptr + STRING_LENGTH_PREFIX_SIZE]
        .try_into()
        .map_err(|_| RuntimeError::memory("Failed to read string length"))?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    debug!(
        "read_string_from_memory: length prefix indicates {} bytes",
        len
    );

    // Sanity check: length should not be unreasonably large
    if len > 10_000_000 {
        error!(
            "read_string_from_memory: suspiciously large length {} at ptr={}",
            len, ptr
        );
        // Log the raw bytes around the ptr for debugging
        let start = ptr.saturating_sub(16);
        let end = (ptr + 32).min(data.len());
        error!(
            "read_string_from_memory: bytes around ptr: {:02x?}",
            &data[start..end]
        );
        return Err(RuntimeError::memory(format!(
            "String length {} is unreasonably large (possible memory corruption at ptr={})",
            len, ptr
        )));
    }

    // Check bounds for string data
    let data_start = ptr + STRING_LENGTH_PREFIX_SIZE;
    let data_end = data_start + len;

    if data_end > data.len() {
        return Err(RuntimeError::memory(format!(
            "String data out of bounds: {}..{} (memory size: {})",
            data_start,
            data_end,
            data.len()
        )));
    }

    // Read and convert to string
    match std::str::from_utf8(&data[data_start..data_end]) {
        Ok(s) => {
            debug!(
                "read_string_from_memory: successfully read {} bytes: '{}'",
                len,
                log_preview(s, 100)
            );
            Ok(s.to_string())
        }
        Err(e) => {
            // Log the problematic bytes for debugging
            let error_index = e.valid_up_to();
            let start = data_start + error_index.saturating_sub(8);
            let end = (data_start + error_index + 16).min(data_end);
            error!(
                "read_string_from_memory: UTF-8 error at offset {} (absolute position {})",
                error_index,
                data_start + error_index
            );
            error!(
                "read_string_from_memory: valid prefix: '{}'",
                String::from_utf8_lossy(&data[data_start..data_start + error_index])
            );
            error!(
                "read_string_from_memory: bytes around error: {:02x?}",
                &data[start..end]
            );

            Err(RuntimeError::memory(format!(
                "Invalid UTF-8 in string: {}",
                e
            )))
        }
    }
}

/// Read a Clean Language string from WASM memory (caller version)
pub fn read_string_from_caller<T>(caller: &mut Caller<'_, T>, ptr: u32) -> RuntimeResult<String> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| RuntimeError::memory("No memory export found"))?;

    let data = memory.data(&*caller);
    let ptr = ptr as usize;

    // Check bounds for length prefix
    if ptr + STRING_LENGTH_PREFIX_SIZE > data.len() {
        return Err(RuntimeError::memory(format!(
            "String pointer {} out of bounds (memory size: {})",
            ptr,
            data.len()
        )));
    }

    // Read length
    let len_bytes: [u8; 4] = data[ptr..ptr + STRING_LENGTH_PREFIX_SIZE]
        .try_into()
        .map_err(|_| RuntimeError::memory("Failed to read string length"))?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    // Check bounds for string data
    let data_start = ptr + STRING_LENGTH_PREFIX_SIZE;
    let data_end = data_start + len;

    if data_end > data.len() {
        return Err(RuntimeError::memory(format!(
            "String data out of bounds: {}..{} (memory size: {})",
            data_start,
            data_end,
            data.len()
        )));
    }

    // Read and convert to string
    std::str::from_utf8(&data[data_start..data_end])
        .map(|s| s.to_string())
        .map_err(|e| RuntimeError::memory(format!("Invalid UTF-8 in string: {}", e)))
}

/// Write bytes directly to WASM memory
pub fn write_bytes_to_memory<T>(
    store: &mut Store<T>,
    memory: &Memory,
    allocator: &mut WasmMemory,
    bytes: &[u8],
) -> RuntimeResult<u32> {
    let ptr = allocator.allocate(bytes.len());
    ensure_memory_size(store, memory, ptr + bytes.len())?;

    memory
        .write(store, ptr, bytes)
        .map_err(|e| RuntimeError::memory(format!("Failed to write bytes: {}", e)))?;

    Ok(ptr as u32)
}

/// Read bytes from WASM memory
pub fn read_bytes_from_memory<T>(
    store: &Store<T>,
    memory: &Memory,
    ptr: u32,
    len: u32,
) -> RuntimeResult<Vec<u8>> {
    let data = memory.data(store);
    let start = ptr as usize;
    let end = start + len as usize;

    if end > data.len() {
        return Err(RuntimeError::memory(format!(
            "Bytes out of bounds: {}..{} (memory size: {})",
            start,
            end,
            data.len()
        )));
    }

    Ok(data[start..end].to_vec())
}

/// WASM page size in bytes (64KB)
const PAGE_SIZE: usize = 65536;

/// Ensure WASM memory is at least the specified size.
/// Uses 1.5x amortized growth to reduce the number of memory.grow() calls.
fn ensure_memory_size<T>(
    store: &mut Store<T>,
    memory: &Memory,
    required: usize,
) -> RuntimeResult<()> {
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
                "ensure_memory_size: growing memory (required {} bytes)",
                required
            );

            memory.grow(&mut *store, pages_to_grow).map_err(|e| {
                warn!(
                    event = "wasm_memory_oom",
                    current_pages = current_pages,
                    requested_pages = pages_to_grow,
                    "Failed to grow memory: {}",
                    e
                );
                RuntimeError::memory(format!(
                    "Failed to grow memory by {} pages: {}",
                    pages_to_grow, e
                ))
            })?;
        }
    }

    Ok(())
}

/// Write an i32 to WASM memory at the given pointer
pub fn write_i32<T>(
    store: &mut Store<T>,
    memory: &Memory,
    ptr: u32,
    value: i32,
) -> RuntimeResult<()> {
    let bytes = value.to_le_bytes();
    memory
        .write(store, ptr as usize, &bytes)
        .map_err(|e| RuntimeError::memory(format!("Failed to write i32: {}", e)))
}

/// Read an i32 from WASM memory
pub fn read_i32<T>(store: &Store<T>, memory: &Memory, ptr: u32) -> RuntimeResult<i32> {
    let data = memory.data(store);
    let start = ptr as usize;

    if start + 4 > data.len() {
        return Err(RuntimeError::memory("i32 read out of bounds"));
    }

    let bytes: [u8; 4] = data[start..start + 4]
        .try_into()
        .map_err(|_| RuntimeError::memory("Failed to read i32"))?;

    Ok(i32::from_le_bytes(bytes))
}

/// Write a JSON value as a string to WASM memory
pub fn write_json_to_memory<T>(
    store: &mut Store<T>,
    memory: &Memory,
    allocator: &mut WasmMemory,
    value: &serde_json::Value,
) -> RuntimeResult<u32> {
    let json_str = serde_json::to_string(value)
        .map_err(|e| RuntimeError::memory(format!("Failed to serialize JSON: {}", e)))?;
    write_string_to_memory(store, memory, allocator, &json_str)
}

/// Read a JSON value from WASM memory
pub fn read_json_from_memory<T>(
    store: &Store<T>,
    memory: &Memory,
    ptr: u32,
) -> RuntimeResult<serde_json::Value> {
    let json_str = read_string_from_memory(store, memory, ptr)?;
    serde_json::from_str(&json_str)
        .map_err(|e| RuntimeError::memory(format!("Failed to parse JSON: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocator() {
        let mut mem = WasmMemory::new();

        let ptr1 = mem.allocate(100);
        assert_eq!(ptr1, 65536);

        let ptr2 = mem.allocate(200);
        // 65536 + 100 = 65636, aligned to 8 = 65640
        assert_eq!(ptr2, 65640);

        mem.reset();
        let ptr3 = mem.allocate(50);
        assert_eq!(ptr3, 65536);
    }

    #[test]
    fn test_alignment() {
        let mut mem = WasmMemory::new();

        // Allocate 1 byte
        let ptr1 = mem.allocate(1);
        assert_eq!(ptr1, 65536);

        // Next allocation should be aligned
        let ptr2 = mem.allocate(1);
        assert_eq!(ptr2, 65544); // 65536 + 8 (aligned)
    }

    #[test]
    fn test_with_initial_offset() {
        let mut mem = WasmMemory::with_initial_offset(131072);
        let ptr1 = mem.allocate(100);
        assert_eq!(ptr1, 131072);

        mem.reset();
        assert_eq!(mem.current_offset(), 131072);
    }

    #[test]
    fn test_metrics() {
        let mut mem = WasmMemory::new();
        assert_eq!(mem.grow_count(), 0);
        assert_eq!(mem.peak_offset(), 65536);

        mem.allocate(1000);
        assert!(mem.peak_offset() > 65536);

        mem.record_grow();
        assert_eq!(mem.grow_count(), 1);

        mem.reset();
        assert_eq!(mem.grow_count(), 0);
    }
}

/// Shortens `s` to at most `max_bytes` bytes for a log line, adding "..."
/// when cut. Cuts only on a character boundary: slicing a `&str` inside a
/// multi-byte character (an em dash, an accented letter) panics.
pub fn log_preview(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod log_preview_tests {
    use super::log_preview;

    #[test]
    fn short_text_is_kept_whole() {
        assert_eq!(log_preview("hola", 200), "hola");
    }

    #[test]
    fn ascii_text_is_cut_at_the_limit() {
        assert_eq!(log_preview("abcdef", 3), "abc...");
    }

    #[test]
    fn cut_inside_a_multibyte_character_moves_back_to_its_start() {
        // "—" is 3 bytes; byte 200 falls inside it, as in the Spanish
        // tutorials index that crashed `_json_get`'s log line.
        let text = format!("{}—resto", "a".repeat(199));
        assert_eq!(log_preview(&text, 200), format!("{}...", "a".repeat(199)));
    }

    #[test]
    fn accented_text_never_panics_at_any_limit() {
        let text = "Programación con IA — información útil";
        for limit in 0..text.len() {
            let out = log_preview(text, limit);
            assert!(out.len() <= limit + 3, "limit {limit} gave {out:?}");
        }
    }
}
