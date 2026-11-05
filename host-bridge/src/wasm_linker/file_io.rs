//! File I/O Host Functions
//!
//! Provides file system operations for WASM modules:
//! - file_read: Read file contents
//! - file_write: Write to a file
//! - file_exists: Check if file exists
//! - file_delete: Delete a file
//! - file_append: Append to a file
//!
//! String parameters use raw (ptr, len) pairs via expand_strings convention.
//! All functions are generic over `WasmStateCore` to work with any runtime.

use super::helpers::{read_raw_string, write_bytes_to_caller, write_string_to_caller};
use super::state::WasmStateCore;
use crate::error::BridgeResult;
use base64::Engine as _;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tracing::{debug, error};
use wasmtime::{Caller, Linker};

/// `_fs_write_bytes` return codes (see foundation/spec/platform/HOST_BRIDGE.md).
///
/// The registry declares `returns = "i32"`; values live in 0..=5.
const FS_WRITE_OK: i32 = 0;
const FS_WRITE_ERR_PERMISSION: i32 = 1;
const FS_WRITE_ERR_DISK_FULL: i32 = 2;
const FS_WRITE_ERR_INVALID_PATH: i32 = 3;
const FS_WRITE_ERR_PARENT_NOT_DIR: i32 = 4;
const FS_WRITE_ERR_IO: i32 = 5;

/// Hard-blocked path prefixes — refused regardless of the storage root.
const FS_WRITE_BLOCKED_PREFIXES: &[&str] = &["/proc", "/sys", "/dev"];

/// Classify a `std::io::Error` into an `_fs_write_bytes` error code.
fn classify_io_error(e: &std::io::Error) -> i32 {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::PermissionDenied => FS_WRITE_ERR_PERMISSION,
        // ErrorKind::StorageFull is unstable; fall back to raw errno on unix.
        _ => {
            #[cfg(unix)]
            {
                if let Some(code) = e.raw_os_error() {
                    // ENOSPC = 28 on linux/macos.
                    if code == 28 {
                        return FS_WRITE_ERR_DISK_FULL;
                    }
                }
            }
            FS_WRITE_ERR_IO
        }
    }
}

/// Reject a path containing `..`, null bytes, or that resolves to a blocked
/// system directory. Enforced BEFORE canonicalization so a symlink can't sneak
/// a `..` past us.
fn path_has_forbidden_syntax(path: &str) -> bool {
    if path.contains('\0') {
        return true;
    }
    Path::new(path)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
}

fn under_blocked_system_prefix(path: &Path) -> bool {
    let s = path.to_string_lossy();
    FS_WRITE_BLOCKED_PREFIXES
        .iter()
        .any(|p| s == *p || s.starts_with(&format!("{}/", p)))
}

/// Environment variable naming the storage root (frame-storage STORAGE-H001).
const STORAGE_ROOT_ENV: &str = "FRAME_STORAGE_ROOT";
/// Earlier name of the storage root variable, still honored when
/// `FRAME_STORAGE_ROOT` is not set.
const LEGACY_STORAGE_ROOT_ENV: &str = "CLEAN_FS_WRITE_ROOT";

/// Resolve the storage root: `FRAME_STORAGE_ROOT`, else `CLEAN_FS_WRITE_ROOT`.
/// Returns `None` when neither is set (or both are empty) — that means "no
/// writable paths".
fn fs_write_root() -> Option<PathBuf> {
    [STORAGE_ROOT_ENV, LEGACY_STORAGE_ROOT_ENV]
        .iter()
        .find_map(|name| std::env::var_os(name).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// Canonical form of `p` that also works when `p` (and some of its ancestors)
/// do not yet exist. Walks up until an existing ancestor is found, canonicalizes
/// it, then re-appends the trailing components. Returns None if no ancestor
/// exists (e.g. bare relative filename with no cwd).
fn canonicalize_lenient(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    let mut trailing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor: &Path = p;
    loop {
        if let Ok(c) = cursor.canonicalize() {
            let mut out = c;
            for seg in trailing.iter().rev() {
                out.push(seg);
            }
            return Some(out);
        }
        let leaf = cursor.file_name()?;
        trailing.push(leaf);
        cursor = cursor.parent()?;
        if cursor.as_os_str().is_empty() {
            // Relative path with no existing prefix — try cwd.
            let base = Path::new(".").canonicalize().ok()?;
            let mut out = base;
            for seg in trailing.iter().rev() {
                out.push(seg);
            }
            return Some(out);
        }
    }
}

/// Validate `path` against the allowlist and blocked prefixes. On success
/// returns the canonicalized target path. On failure returns the appropriate
/// error code.
fn resolve_allowlisted_path(path: &str) -> Result<PathBuf, i32> {
    if path.is_empty() || path_has_forbidden_syntax(path) {
        return Err(FS_WRITE_ERR_INVALID_PATH);
    }

    let root = fs_write_root().ok_or(FS_WRITE_ERR_INVALID_PATH)?;
    let canonical_root = root.canonicalize().map_err(|_| FS_WRITE_ERR_INVALID_PATH)?;

    // Relative paths are resolved against the storage root (STORAGE-H001),
    // never against the process working directory. Absolute paths are
    // accepted only when they already lie inside the root.
    let requested = Path::new(path);
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        canonical_root.join(requested)
    };
    let canonical_target = canonicalize_lenient(&requested).ok_or(FS_WRITE_ERR_INVALID_PATH)?;

    if !canonical_target.starts_with(&canonical_root) {
        return Err(FS_WRITE_ERR_INVALID_PATH);
    }
    if under_blocked_system_prefix(&canonical_target) {
        return Err(FS_WRITE_ERR_INVALID_PATH);
    }

    Ok(canonical_target)
}

/// Atomic write of `bytes` to `target`: write to `<target>.tmp` then rename.
///
/// Rename is atomic on POSIX when source and destination live on the same
/// filesystem. On Windows the rename is not atomic across drives; both hosts
/// document the caveat (see HOST_BRIDGE.md).
fn atomic_write_bytes(target: &Path, bytes: &[u8]) -> Result<(), i32> {
    // Parent directory: create if missing; reject if it exists as a file.
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            if parent.exists() && !parent.is_dir() {
                return Err(FS_WRITE_ERR_PARENT_NOT_DIR);
            }
            if let Err(e) = fs::create_dir_all(parent) {
                return Err(classify_io_error(&e));
            }
        }
    }

    let mut tmp = target.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    if let Err(e) = fs::write(&tmp, bytes) {
        // Best-effort cleanup; ignore result.
        let _ = fs::remove_file(&tmp);
        return Err(classify_io_error(&e));
    }

    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(classify_io_error(&e));
    }

    Ok(())
}

/// Ensure the parent directory of `path` exists, creating it (and any missing
/// ancestors) if necessary. Returns Ok on success or if `path` has no parent
/// component (e.g. a bare filename). Returns Err if directory creation fails.
fn ensure_parent_dir(path: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        // Path::parent() returns Some("") for bare filenames like "foo.md";
        // skip the call in that case so we don't attempt to create an empty path.
        if !parent.as_os_str().is_empty() {
            return fs::create_dir_all(parent);
        }
    }
    Ok(())
}

/// Write `content` to `path`, creating intermediate parent directories as
/// needed. Returns 1 on success, 0 on failure.
fn write_file_with_parents(path: &str, content: &str) -> i32 {
    if let Err(e) = ensure_parent_dir(path) {
        error!(
            "file_write: Failed to create parent directories for '{}': {}",
            path, e
        );
        return 0;
    }
    match fs::write(path, content) {
        Ok(_) => 1,
        Err(e) => {
            error!("file_write: Failed to write file '{}': {}", path, e);
            0
        }
    }
}

/// Append `content` to `path`, creating intermediate parent directories and
/// the file itself if needed. Returns 1 on success, 0 on failure.
fn append_file_with_parents(path: &str, content: &str) -> i32 {
    if let Err(e) = ensure_parent_dir(path) {
        error!(
            "file_append: Failed to create parent directories for '{}': {}",
            path, e
        );
        return 0;
    }
    match fs::OpenOptions::new().append(true).create(true).open(path) {
        Ok(mut file) => match file.write_all(content.as_bytes()) {
            Ok(_) => 1,
            Err(e) => {
                error!("file_append: Failed to write to file '{}': {}", path, e);
                0
            }
        },
        Err(e) => {
            error!("file_append: Failed to open file '{}': {}", path, e);
            0
        }
    }
}

/// Register all file I/O functions with the linker
pub fn register_functions<S: WasmStateCore>(linker: &mut Linker<S>) -> BridgeResult<()> {
    // =========================================
    // FILE READ
    // =========================================

    // file_read - Read file contents
    // Signature: (path_ptr: i32, path_len: i32, _mode: i32) -> i32
    // Returns: pointer to file contents as length-prefixed string
    linker.func_wrap(
        "env",
        "file_read",
        |mut caller: Caller<'_, S>, path_ptr: i32, path_len: i32, _mode: i32| -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => {
                    error!("file_read: Failed to read path");
                    return write_string_to_caller(&mut caller, "");
                }
            };

            debug!("file_read: path={}", path);

            match fs::read_to_string(&path) {
                Ok(contents) => write_string_to_caller(&mut caller, &contents),
                Err(e) => {
                    error!("file_read: Failed to read file '{}': {}", path, e);
                    write_string_to_caller(&mut caller, "")
                }
            }
        },
    )?;

    // =========================================
    // FILE WRITE
    // =========================================

    // file_write - Write to a file (overwrites if exists)
    // Signature: (path_ptr: i32, path_len: i32, content_ptr: i32, content_len: i32) -> i32 (boolean)
    // Returns: 1 on success, 0 on failure
    linker.func_wrap(
        "env",
        "file_write",
        |mut caller: Caller<'_, S>,
         path_ptr: i32,
         path_len: i32,
         content_ptr: i32,
         content_len: i32|
         -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => {
                    error!("file_write: Failed to read path");
                    return 0;
                }
            };

            let content = match read_raw_string(&mut caller, content_ptr, content_len) {
                Some(s) => s,
                None => {
                    error!("file_write: Failed to read content");
                    return 0;
                }
            };

            debug!("file_write: path={}, content_len={}", path, content.len());

            write_file_with_parents(&path, &content)
        },
    )?;

    // =========================================
    // FILE EXISTS
    // =========================================

    // file_exists - Check if file exists
    // Signature: (path_ptr: i32, path_len: i32) -> i32
    // Returns: 1 if exists, 0 if not
    linker.func_wrap(
        "env",
        "file_exists",
        |mut caller: Caller<'_, S>, path_ptr: i32, path_len: i32| -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => {
                    error!("file_exists: Failed to read path");
                    return 0;
                }
            };

            debug!("file_exists: path={}", path);

            if std::path::Path::new(&path).exists() {
                1
            } else {
                0
            }
        },
    )?;

    // =========================================
    // FILE DELETE
    // =========================================

    // file_delete - Delete a file
    // Signature: (path_ptr: i32, path_len: i32) -> i32 (boolean)
    // Returns: 1 on success, 0 on failure
    linker.func_wrap(
        "env",
        "file_delete",
        |mut caller: Caller<'_, S>, path_ptr: i32, path_len: i32| -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => {
                    error!("file_delete: Failed to read path");
                    return 0;
                }
            };

            debug!("file_delete: path={}", path);

            match fs::remove_file(&path) {
                Ok(_) => 1,
                Err(e) => {
                    error!("file_delete: Failed to delete file '{}': {}", path, e);
                    0
                }
            }
        },
    )?;

    // =========================================
    // FILE APPEND
    // =========================================

    // file_append - Append to a file
    // Signature: (path_ptr: i32, path_len: i32, content_ptr: i32, content_len: i32) -> i32 (boolean)
    // Returns: 1 on success, 0 on failure
    linker.func_wrap(
        "env",
        "file_append",
        |mut caller: Caller<'_, S>,
         path_ptr: i32,
         path_len: i32,
         content_ptr: i32,
         content_len: i32|
         -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => {
                    error!("file_append: Failed to read path");
                    return 0;
                }
            };

            let content = match read_raw_string(&mut caller, content_ptr, content_len) {
                Some(s) => s,
                None => {
                    error!("file_append: Failed to read content");
                    return 0;
                }
            };

            debug!("file_append: path={}, content_len={}", path, content.len());

            append_file_with_parents(&path, &content)
        },
    )?;

    // =========================================
    // FILE EXTRAS (registry hosts = ["server"])
    // =========================================

    // file_size(string) -> i32 — bytes, -1 on error
    linker.func_wrap(
        "env",
        "file_size",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> i32 {
            let path = match read_raw_string(&mut caller, p, l) {
                Some(s) => s,
                None => return -1,
            };
            match fs::metadata(&path) {
                Ok(m) => m.len() as i32,
                Err(_) => -1,
            }
        },
    )?;

    // file_list_dir(string) -> ptr (LP string with JSON array of names)
    linker.func_wrap(
        "env",
        "file_list_dir",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> i32 {
            let path = match read_raw_string(&mut caller, p, l) {
                Some(s) => s,
                None => return write_string_to_caller(&mut caller, "[]"),
            };
            let names: Vec<String> = match fs::read_dir(&path) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect(),
                Err(_) => Vec::new(),
            };
            let json = serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string());
            write_string_to_caller(&mut caller, &json)
        },
    )?;

    // file_mkdir(string) -> i32 (1 on success, 0 on failure) — creates intermediates
    linker.func_wrap(
        "env",
        "file_mkdir",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> i32 {
            let path = match read_raw_string(&mut caller, p, l) {
                Some(s) => s,
                None => return 0,
            };
            if fs::create_dir_all(&path).is_ok() {
                1
            } else {
                0
            }
        },
    )?;

    // file_copy(string, string) -> i32
    linker.func_wrap(
        "env",
        "file_copy",
        |mut caller: Caller<'_, S>, sp: i32, sl: i32, dp: i32, dl: i32| -> i32 {
            let src = match read_raw_string(&mut caller, sp, sl) {
                Some(s) => s,
                None => return 0,
            };
            let dst = match read_raw_string(&mut caller, dp, dl) {
                Some(s) => s,
                None => return 0,
            };
            if let Err(e) = ensure_parent_dir(&dst) {
                error!("file_copy: parent dir for '{}': {}", dst, e);
                return 0;
            }
            if fs::copy(&src, &dst).is_ok() {
                1
            } else {
                0
            }
        },
    )?;

    // file_rename(string, string) -> i32
    linker.func_wrap(
        "env",
        "file_rename",
        |mut caller: Caller<'_, S>, sp: i32, sl: i32, dp: i32, dl: i32| -> i32 {
            let src = match read_raw_string(&mut caller, sp, sl) {
                Some(s) => s,
                None => return 0,
            };
            let dst = match read_raw_string(&mut caller, dp, dl) {
                Some(s) => s,
                None => return 0,
            };
            if let Err(e) = ensure_parent_dir(&dst) {
                error!("file_rename: parent dir for '{}': {}", dst, e);
                return 0;
            }
            if fs::rename(&src, &dst).is_ok() {
                1
            } else {
                0
            }
        },
    )?;

    // file_is_directory(string) -> boolean
    linker.func_wrap(
        "env",
        "file_is_directory",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> i32 {
            let path = match read_raw_string(&mut caller, p, l) {
                Some(s) => s,
                None => return 0,
            };
            if Path::new(&path).is_dir() {
                1
            } else {
                0
            }
        },
    )?;

    // file_read_binary(string) -> ptr — base64-encoded file contents in an LP string
    linker.func_wrap(
        "env",
        "file_read_binary",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> i32 {
            let path = match read_raw_string(&mut caller, p, l) {
                Some(s) => s,
                None => return write_string_to_caller(&mut caller, ""),
            };
            let bytes = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    error!("file_read_binary: '{}': {}", path, e);
                    return write_string_to_caller(&mut caller, "");
                }
            };
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            write_string_to_caller(&mut caller, &encoded)
        },
    )?;

    // file_write_binary(string, string) -> i32 — second string is base64 of bytes to write
    linker.func_wrap(
        "env",
        "file_write_binary",
        |mut caller: Caller<'_, S>, pp: i32, pl: i32, bp: i32, bl: i32| -> i32 {
            let path = match read_raw_string(&mut caller, pp, pl) {
                Some(s) => s,
                None => return 0,
            };
            let b64 = match read_raw_string(&mut caller, bp, bl) {
                Some(s) => s,
                None => return 0,
            };
            let bytes = match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
                Ok(b) => b,
                Err(e) => {
                    error!(
                        "file_write_binary: base64 decode failed for '{}': {}",
                        path, e
                    );
                    return 0;
                }
            };
            if let Err(e) = ensure_parent_dir(&path) {
                error!("file_write_binary: parent for '{}': {}", path, e);
                return 0;
            }
            if fs::write(&path, &bytes).is_ok() {
                1
            } else {
                0
            }
        },
    )?;

    // file_rmdir(string) -> i32 — recursive removal
    linker.func_wrap(
        "env",
        "file_rmdir",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> i32 {
            let path = match read_raw_string(&mut caller, p, l) {
                Some(s) => s,
                None => return 0,
            };
            if fs::remove_dir_all(&path).is_ok() {
                1
            } else {
                0
            }
        },
    )?;

    // =========================================
    // _fs_write_bytes — binary-safe atomic write with allowlist
    // =========================================
    //
    // Signature: (path_ptr: i32, path_len: i32, bytes_ptr: i32) -> i32
    // - `path`  is (ptr, len) — a raw UTF-8 path.
    // - `bytes` is a pointer to a length-prefixed byte buffer
    //   ([4-byte LE length][bytes]) — the exact layout produced by
    //   `_req_body_bytes`, so binary payloads flow request → hash → disk
    //   verbatim without a UTF-8 detour.
    //
    // Returns 0 on success; non-zero error code on failure (see error code
    // constants at the top of this file, and the contract table in
    // foundation/spec/platform/HOST_BRIDGE.md).
    linker.func_wrap(
        "env",
        "_fs_write_bytes",
        |mut caller: Caller<'_, S>, path_ptr: i32, path_len: i32, bytes_ptr: i32| -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => {
                    error!("_fs_write_bytes: failed to read path");
                    return FS_WRITE_ERR_INVALID_PATH;
                }
            };

            let target = match resolve_allowlisted_path(&path) {
                Ok(p) => p,
                Err(code) => {
                    debug!(
                        "_fs_write_bytes: rejected path '{}' with code {}",
                        path, code
                    );
                    return code;
                }
            };

            // Read the [4-byte LE length][bytes] buffer at bytes_ptr from
            // linear memory. We must resolve `memory` here rather than call
            // `read_length_prefixed_bytes` on a borrowed slice returned from
            // `caller`, because that would tie up the caller borrow.
            let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => {
                    error!("_fs_write_bytes: no exported 'memory'");
                    return FS_WRITE_ERR_IO;
                }
            };
            let data = memory.data(&caller);
            let base = bytes_ptr as usize;
            if base + 4 > data.len() {
                error!("_fs_write_bytes: bytes_ptr out of bounds");
                return FS_WRITE_ERR_IO;
            }
            let len_bytes: [u8; 4] = match data[base..base + 4].try_into() {
                Ok(b) => b,
                Err(_) => return FS_WRITE_ERR_IO,
            };
            let payload_len = u32::from_le_bytes(len_bytes) as usize;
            let start = base + 4;
            let end = start + payload_len;
            if end > data.len() {
                error!(
                    "_fs_write_bytes: payload out of bounds: {}..{} (memory size: {})",
                    start,
                    end,
                    data.len()
                );
                return FS_WRITE_ERR_IO;
            }
            let payload = data[start..end].to_vec();

            debug!(
                "_fs_write_bytes: path='{}' target='{}' bytes={}",
                path,
                target.display(),
                payload.len()
            );

            match atomic_write_bytes(&target, &payload) {
                Ok(()) => FS_WRITE_OK,
                Err(code) => code,
            }
        },
    )?;
    // Dot-notation alias (compiler >= 0.30.120 emits both forms).
    linker.alias("env", "_fs_write_bytes", "env", "fs.write_bytes")?;
    // frame.storage plugin surface (registry entry lists storage.write alias).
    linker.alias("env", "_fs_write_bytes", "env", "storage.write")?;

    // =========================================
    // _fs_read_bytes — binary-safe read under allowlist
    // =========================================
    //
    // Signature: (path_ptr: i32, path_len: i32) -> i32
    // Returns a pointer to a [4-byte LE length][bytes] buffer — same layout
    // as `_fs_write_bytes` accepts and `_crypto_sha256_bytes` returns.
    //
    // Missing files, allowlist violations, and any I/O error all return an
    // empty buffer (length 0). Callers disambiguate "missing" from "empty"
    // via `_fs_exists`.
    linker.func_wrap(
        "env",
        "_fs_read_bytes",
        |mut caller: Caller<'_, S>, path_ptr: i32, path_len: i32| -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => {
                    error!("_fs_read_bytes: failed to read path");
                    return write_bytes_to_caller(&mut caller, &[]);
                }
            };

            let target = match resolve_allowlisted_path(&path) {
                Ok(p) => p,
                Err(code) => {
                    debug!(
                        "_fs_read_bytes: rejected path '{}' with code {}",
                        path, code
                    );
                    return write_bytes_to_caller(&mut caller, &[]);
                }
            };

            match fs::read(&target) {
                Ok(bytes) => {
                    debug!(
                        "_fs_read_bytes: path='{}' target='{}' bytes={}",
                        path,
                        target.display(),
                        bytes.len()
                    );
                    write_bytes_to_caller(&mut caller, &bytes)
                }
                Err(e) => {
                    debug!(
                        "_fs_read_bytes: read failed for '{}': {}",
                        target.display(),
                        e
                    );
                    write_bytes_to_caller(&mut caller, &[])
                }
            }
        },
    )?;
    linker.alias("env", "_fs_read_bytes", "env", "fs.read_bytes")?;
    linker.alias("env", "_fs_read_bytes", "env", "storage.read")?;

    // =========================================
    // _fs_exists — allowlisted regular-file existence probe
    // =========================================
    //
    // Signature: (path_ptr: i32, path_len: i32) -> i32
    // Returns 1 iff the resolved allowlisted path is a regular file.
    // Directories, symlinks that resolve outside the allowlist, allowlist
    // violations, and any I/O error all return 0.
    linker.func_wrap(
        "env",
        "_fs_exists",
        |mut caller: Caller<'_, S>, path_ptr: i32, path_len: i32| -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => return 0,
            };

            let target = match resolve_allowlisted_path(&path) {
                Ok(p) => p,
                Err(_) => return 0,
            };

            match fs::metadata(&target) {
                Ok(md) if md.is_file() => 1,
                _ => 0,
            }
        },
    )?;
    linker.alias("env", "_fs_exists", "env", "fs.exists")?;
    linker.alias("env", "_fs_exists", "env", "storage.exists")?;

    // =========================================
    // _fs_delete — allowlisted file removal
    // =========================================
    //
    // Signature: (path_ptr: i32, path_len: i32) -> i32
    // Returns 1 on successful removal, 0 when the file did not exist or
    // removal failed for any reason. Underlying reason is logged so
    // operators can distinguish 'already gone' from 'permission denied'.
    linker.func_wrap(
        "env",
        "_fs_delete",
        |mut caller: Caller<'_, S>, path_ptr: i32, path_len: i32| -> i32 {
            let path = match read_raw_string(&mut caller, path_ptr, path_len) {
                Some(s) => s,
                None => return 0,
            };

            let target = match resolve_allowlisted_path(&path) {
                Ok(p) => p,
                Err(code) => {
                    debug!("_fs_delete: rejected path '{}' with code {}", path, code);
                    return 0;
                }
            };

            match fs::remove_file(&target) {
                Ok(()) => {
                    debug!("_fs_delete: removed '{}'", target.display());
                    1
                }
                Err(e) => {
                    debug!(
                        "_fs_delete: remove failed for '{}': {}",
                        target.display(),
                        e
                    );
                    0
                }
            }
        },
    )?;
    linker.alias("env", "_fs_delete", "env", "fs.delete")?;
    linker.alias("env", "_fs_delete", "env", "storage.delete")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---------- file_write ----------

    #[test]
    fn file_write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a/b/c/file.md");
        let result = write_file_with_parents(path.to_str().unwrap(), "hello");
        assert_eq!(result, 1, "write_file_with_parents should return 1");
        assert!(path.exists(), "file should exist on disk");
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn file_write_existing_parent_still_works() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.md");
        let result = write_file_with_parents(path.to_str().unwrap(), "hi");
        assert_eq!(result, 1);
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi");
    }

    #[test]
    fn file_write_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested/file.md");
        assert_eq!(write_file_with_parents(path.to_str().unwrap(), "first"), 1);
        assert_eq!(write_file_with_parents(path.to_str().unwrap(), "second"), 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn file_write_invalid_path_returns_zero() {
        // Create a regular file, then try to use it AS a parent directory for
        // another write. create_dir_all should fail because the "parent" is a file.
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "i am a file").unwrap();
        let bad_path = blocker.join("child/file.md");
        let result = write_file_with_parents(bad_path.to_str().unwrap(), "data");
        assert_eq!(result, 0, "should return 0 when parent cannot be created");
    }

    #[test]
    fn file_write_bare_filename_no_parent_component() {
        // ensure_parent_dir should be a no-op for bare filenames; the actual
        // write happens in the current directory of the test process. Use a
        // tempdir as cwd via path joining instead so we don't pollute cwd.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("just-a-name.txt");
        assert_eq!(write_file_with_parents(path.to_str().unwrap(), "x"), 1);
        assert!(path.exists());
    }

    // ---------- file_append ----------

    #[test]
    fn file_append_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a/b/c/log.txt");
        let result = append_file_with_parents(path.to_str().unwrap(), "line1\n");
        assert_eq!(result, 1);
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "line1\n");
    }

    #[test]
    fn file_append_existing_parent_still_works() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("log.txt");
        assert_eq!(append_file_with_parents(path.to_str().unwrap(), "a"), 1);
        assert_eq!(append_file_with_parents(path.to_str().unwrap(), "b"), 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "ab");
    }

    #[test]
    fn file_append_invalid_path_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "i am a file").unwrap();
        let bad_path = blocker.join("child/log.txt");
        let result = append_file_with_parents(bad_path.to_str().unwrap(), "data");
        assert_eq!(result, 0);
    }

    // ---------- _fs_write_bytes ----------
    //
    // These tests target the Rust helpers directly (`resolve_allowlisted_path`
    // + `atomic_write_bytes`). The WASM linker entry point is covered by
    // `test_spec_compliance` in wasm_linker/mod.rs, which validates the
    // function signature matches the registry.
    //
    // Env-var access is serialized via `ENV_LOCK` because the storage-root
    // variables are process-global and cargo runs tests in parallel by default.

    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Saves both storage-root variables and restores them on drop.
    struct EnvGuard {
        prev: [Option<std::ffi::OsString>; 2],
    }
    impl EnvGuard {
        fn capture() -> Self {
            Self {
                prev: [
                    std::env::var_os(STORAGE_ROOT_ENV),
                    std::env::var_os(LEGACY_STORAGE_ROOT_ENV),
                ],
            }
        }
        /// Root set through the legacy variable only.
        fn set(root: &Path) -> Self {
            let g = Self::capture();
            std::env::remove_var(STORAGE_ROOT_ENV);
            std::env::set_var(LEGACY_STORAGE_ROOT_ENV, root);
            g
        }
        /// Root set through `FRAME_STORAGE_ROOT` (and optionally a different
        /// legacy root, to check precedence).
        fn set_storage_root(root: &Path, legacy: Option<&Path>) -> Self {
            let g = Self::capture();
            std::env::set_var(STORAGE_ROOT_ENV, root);
            match legacy {
                Some(l) => std::env::set_var(LEGACY_STORAGE_ROOT_ENV, l),
                None => std::env::remove_var(LEGACY_STORAGE_ROOT_ENV),
            }
            g
        }
        fn unset() -> Self {
            let g = Self::capture();
            std::env::remove_var(STORAGE_ROOT_ENV);
            std::env::remove_var(LEGACY_STORAGE_ROOT_ENV);
            g
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, prev) in [STORAGE_ROOT_ENV, LEGACY_STORAGE_ROOT_ENV]
                .iter()
                .zip(self.prev.iter())
            {
                match prev {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn storage_root_env_resolves_relative_paths_inside_root() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = TempDir::new().unwrap();
        let _env = EnvGuard::set_storage_root(root.path(), None);

        // A relative path lands under the root, whatever the cwd is.
        let target = resolve_allowlisted_path("artifacts/a.txt").unwrap();
        assert_eq!(
            target,
            root.path().canonicalize().unwrap().join("artifacts/a.txt")
        );
        atomic_write_bytes(&target, b"hello").unwrap();
        assert_eq!(
            std::fs::read(root.path().join("artifacts/a.txt")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn storage_root_env_takes_precedence_over_legacy_root() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = TempDir::new().unwrap();
        let legacy = TempDir::new().unwrap();
        let _env = EnvGuard::set_storage_root(root.path(), Some(legacy.path()));

        let target = resolve_allowlisted_path("x.bin").unwrap();
        assert!(target.starts_with(root.path().canonicalize().unwrap()));
        // An absolute path inside the legacy root is outside the storage root.
        let outside = legacy.path().canonicalize().unwrap().join("x.bin");
        assert_eq!(
            resolve_allowlisted_path(outside.to_str().unwrap()).unwrap_err(),
            FS_WRITE_ERR_INVALID_PATH
        );
    }

    /// Full round-trip: resolve path via allowlist, then atomically write.
    /// Panics if the allowlist rejects the path. Returns whatever
    /// `atomic_write_bytes` returned.
    fn write_bytes_within_root(rel: &str, root: &Path, bytes: &[u8]) -> Result<PathBuf, i64> {
        let full = root.join(rel);
        let target = resolve_allowlisted_path(full.to_str().unwrap())?;
        atomic_write_bytes(&target, bytes)?;
        Ok(target)
    }

    #[test]
    fn fs_write_bytes_preserves_null_bytes() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        let payload = b"before\x00middle\x00after";
        let written = write_bytes_within_root("null.bin", &root, payload).unwrap();
        let on_disk = fs::read(&written).unwrap();
        assert_eq!(on_disk, payload);
    }

    #[test]
    fn fs_write_bytes_preserves_high_bytes() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        let payload: Vec<u8> = (0u8..=255).collect();
        let mut with_ff = payload.clone();
        with_ff.extend_from_slice(&[0xFF; 32]);

        let written = write_bytes_within_root("all_bytes.bin", &root, &with_ff).unwrap();
        let on_disk = fs::read(&written).unwrap();
        assert_eq!(on_disk, with_ff);
    }

    #[test]
    fn fs_write_bytes_preserves_gzip_magic() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        // gzip magic (1f 8b) + deflate method (08) + FLG (00) + MTIME (0000) + XFL/OS.
        let payload = &[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03];
        let written = write_bytes_within_root("payload.tar.gz", &root, payload).unwrap();
        let on_disk = fs::read(&written).unwrap();
        assert_eq!(on_disk, payload);
        assert_eq!(&on_disk[..2], &[0x1f, 0x8b]);
    }

    #[test]
    fn fs_write_bytes_zero_length_succeeds() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        let written = write_bytes_within_root("empty.bin", &root, &[]).unwrap();
        assert!(written.exists());
        assert_eq!(fs::metadata(&written).unwrap().len(), 0);
    }

    #[test]
    fn fs_write_bytes_rejects_path_outside_root() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root_tmp = TempDir::new().unwrap();
        let outside_tmp = TempDir::new().unwrap();
        let root = root_tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        let outside = outside_tmp.path().canonicalize().unwrap().join("bad.bin");
        let code = resolve_allowlisted_path(outside.to_str().unwrap()).unwrap_err();
        assert_eq!(code, FS_WRITE_ERR_INVALID_PATH);
    }

    #[test]
    fn fs_write_bytes_rejects_when_root_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::unset();

        let path = tmp.path().join("anywhere.bin");
        let code = resolve_allowlisted_path(path.to_str().unwrap()).unwrap_err();
        assert_eq!(code, FS_WRITE_ERR_INVALID_PATH);
    }

    #[test]
    fn fs_write_bytes_rejects_dotdot() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        // The `..` component alone is enough to reject regardless of where it
        // canonicalizes — path_has_forbidden_syntax runs before canonicalization.
        let bad = root.join("sub/../escape.bin");
        let code = resolve_allowlisted_path(bad.to_str().unwrap()).unwrap_err();
        assert_eq!(code, FS_WRITE_ERR_INVALID_PATH);
    }

    #[test]
    fn fs_write_bytes_rejects_null_byte_in_path() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        let bad = format!("{}/bad\0.bin", root.display());
        let code = resolve_allowlisted_path(&bad).unwrap_err();
        assert_eq!(code, FS_WRITE_ERR_INVALID_PATH);
    }

    #[test]
    fn fs_write_bytes_no_tmp_leftover_on_success() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        let written = write_bytes_within_root("atomic.bin", &root, b"payload").unwrap();
        // The transient .tmp file must be gone after a successful rename.
        let tmp_leftover = {
            let mut s = written.as_os_str().to_owned();
            s.push(".tmp");
            PathBuf::from(s)
        };
        assert!(written.exists());
        assert!(!tmp_leftover.exists());
        assert_eq!(fs::read(&written).unwrap(), b"payload");
    }

    #[test]
    fn fs_write_bytes_creates_parent_directory() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        let written = write_bytes_within_root("deeply/nested/dir/f.bin", &root, b"hi").unwrap();
        assert!(written.exists());
        assert!(written.parent().unwrap().is_dir());
    }

    #[test]
    fn fs_write_bytes_overwrites_existing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        let first = write_bytes_within_root("dup.bin", &root, b"first").unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"first");
        let second = write_bytes_within_root("dup.bin", &root, b"second").unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(&second).unwrap(), b"second");
    }

    #[test]
    fn fs_write_bytes_content_length_parity() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        for len in [1usize, 100, 4096, 65_537] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
            let name = format!("len_{}.bin", len);
            let written = write_bytes_within_root(&name, &root, &payload).unwrap();
            assert_eq!(
                fs::metadata(&written).unwrap().len() as usize,
                payload.len()
            );
            assert_eq!(fs::read(&written).unwrap(), payload);
        }
    }

    #[test]
    fn fs_write_bytes_parent_is_file_returns_code_4() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let _env = EnvGuard::set(&root);

        // Create a regular file, then try to write "underneath" it.
        let blocker = root.join("blocker");
        fs::write(&blocker, b"i am a file").unwrap();
        let bad = root.join("blocker/child.bin");
        // resolve_allowlisted_path uses canonicalize_lenient; since the leaf
        // doesn't exist, it canonicalizes the parent — which IS an existing
        // file. That resolves under the root, so allowlist passes; the failure
        // then surfaces from atomic_write_bytes.
        let target = resolve_allowlisted_path(bad.to_str().unwrap()).unwrap();
        let code = atomic_write_bytes(&target, b"data").unwrap_err();
        assert_eq!(code, FS_WRITE_ERR_PARENT_NOT_DIR);
    }

    #[test]
    fn fs_write_bytes_rejects_blocked_system_prefix() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Set root to `/` so the target path passes the allowlist prefix check;
        // the blocked-prefix guard must still refuse `/proc/self/mem`.
        let _env = EnvGuard::set(Path::new("/"));

        let code = resolve_allowlisted_path("/proc/self/mem").unwrap_err();
        assert_eq!(code, FS_WRITE_ERR_INVALID_PATH);
    }
}
