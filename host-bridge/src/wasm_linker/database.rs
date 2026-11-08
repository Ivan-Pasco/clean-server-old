//! Database Host Functions
//!
//! Provides database operations for WASM modules:
//! - _db_query: Execute SELECT queries
//! - _db_execute: Execute INSERT/UPDATE/DELETE
//! - _db_begin, _db_commit, _db_rollback: Transaction management
//! - _db_configure: Configure connection pool from JSON
//! - _db_paginate: Offset-based paginated query
//! - _db_cursor_page: Cursor-based paginated query
//! - _db_migration_diff: Compare declared model vs live schema
//! - _db_migration_status: List applied + pending migrations
//! - _db_rollback_migration: Rollback a specific migration
//! - _db_run_migrations: Apply all pending migrations
//! - _db_valid_field: Runtime ORDER BY safety check
//!
//! All functions are generic over `WasmStateCore` to work with any runtime.

use super::helpers::{read_raw_string, write_string_to_caller};
use super::state::{SharedDbBridge, WasmStateCore};
use crate::error::BridgeResult;
use serde_json::{json, Value};
use std::cell::RefCell;
use tracing::{debug, error};
use wasmtime::{Caller, Linker};

// Per-thread cache for `_db_*_async` fire-and-forget pattern.
// Rust drivers (sqlx) are inherently async-capable via block_on, so the "async"
// variants delegate to the existing sync impls and cache the result for the
// matching `*_result` getter. See HOST_BRIDGE.md § DB Async.
thread_local! {
    static LAST_QUERY_RESULT_JSON: RefCell<String> = RefCell::new(String::new());
    static LAST_EXECUTE_AFFECTED: RefCell<i32> = const { RefCell::new(0) };
}

/// Match `SELECT [<expr>] LAST_INSERT_ID()` / `SELECT [<expr>] LAST_INSERT_ROWID()`
/// (with an optional `AS <alias>`) and return the alias column name to use in the
/// synthetic single-row response. Returns `None` if the SQL is not a bare
/// `LAST_INSERT_ID()` / `LAST_INSERT_ROWID()` query.
///
/// `LAST_INSERT_ID()` and SQLite's `LAST_INSERT_ROWID()` are
/// session-local. The bridge dispatches each query against a fresh pooled
/// connection, so the id from the prior INSERT is invisible to a follow-up
/// SELECT. To preserve the documented contract from the caller's perspective
/// we serve the cached value from the WASM state instead of hitting the DB.
fn last_insert_id_alias(sql: &str) -> Option<String> {
    let normalized: String = sql
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    let trimmed = normalized.trim_end_matches(';').trim();
    let body = trimmed.strip_prefix("SELECT ")?;
    let body = body.trim();
    let (func, rest) = if let Some(r) = body.strip_prefix("LAST_INSERT_ID ( )") {
        ("id", r)
    } else if let Some(r) = body.strip_prefix("LAST_INSERT_ID()") {
        ("id", r)
    } else if let Some(r) = body.strip_prefix("LAST_INSERT_ROWID ( )") {
        ("id", r)
    } else if let Some(r) = body.strip_prefix("LAST_INSERT_ROWID()") {
        ("id", r)
    } else {
        return None;
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(func.to_string());
    }
    let alias_part = rest.strip_prefix("AS ").unwrap_or(rest).trim();
    // Reject anything with extra clauses (FROM, WHERE, commas — multi-column SELECTs)
    if alias_part.contains(' ') || alias_part.contains(',') {
        return None;
    }
    // Preserve the alias casing from the original SQL where possible.
    let alias_lower = alias_part.to_lowercase();
    let original_alias = sql
        .split_whitespace()
        .find(|tok| tok.trim_end_matches(';').to_lowercase() == alias_lower)
        .map(|tok| tok.trim_end_matches(';').to_string())
        .unwrap_or(alias_part.to_string());
    Some(original_alias)
}

fn last_insert_id_response(alias: &str, id: Option<i64>) -> String {
    let id_val = id.unwrap_or(0);
    let row = json!({ alias: id_val });
    json!({
        "ok": true,
        "data": {
            "rows": [row],
            "count": 1
        }
    })
    .to_string()
}

/// Detect whether `sql` is an INSERT (so the bridge knows to cache the
/// resulting `last_insert_id` after dispatching to `execute`).
/// Run a `DbBridge` statement function (`query`, `execute`, `paginate`,
/// `cursor_page`) for a bridge call: inside the transaction the caller's
/// `_db_begin` opened when there is one, on a pooled connection otherwise.
fn run_statement<S: WasmStateCore>(
    caller: &mut Caller<'_, S>,
    db_bridge: SharedDbBridge,
    function: &str,
    params: Value,
) -> anyhow::Result<Value> {
    let mut tx = caller
        .data_mut()
        .transaction_slot()
        .and_then(|slot| slot.take());
    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let mut bridge = db_bridge.write().await;
            bridge.call_in(function, params, tx.as_mut()).await
        })
    });
    if let (Some(tx), Some(slot)) = (tx, caller.data_mut().transaction_slot()) {
        *slot = Some(tx);
    }
    result
}

fn is_insert(sql: &str) -> bool {
    sql.trim_start().to_uppercase().starts_with("INSERT")
        || sql.trim_start().to_uppercase().starts_with("REPLACE")
}

/// Register all database functions with the linker
pub fn register_functions<S: WasmStateCore>(linker: &mut Linker<S>) -> BridgeResult<()> {
    // =========================================
    // DATABASE QUERY
    // =========================================

    // _db_query - Execute a SELECT query
    // Args: sql_ptr, sql_len, params_ptr, params_len (JSON array of params)
    // Returns: pointer to JSON string with query results
    linker.func_wrap(
        "env",
        "_db_query",
        |mut caller: Caller<'_, S>,
         sql_ptr: i32,
         sql_len: i32,
         params_ptr: i32,
         params_len: i32|
         -> i32 {
            let sql = match read_raw_string(&mut caller, sql_ptr, sql_len) {
                Some(s) => s,
                None => {
                    error!("_db_query: Failed to read SQL string");
                    return write_string_to_caller(
                        &mut caller,
                        r#"{"ok":false,"err":{"code":"MEMORY_ERROR","message":"Failed to read SQL"}}"#,
                    );
                }
            };

            let params_json = if params_len > 0 {
                read_raw_string(&mut caller, params_ptr, params_len).unwrap_or_else(|| "[]".to_string())
            } else {
                "[]".to_string()
            };

            debug!("_db_query: SQL='{}' (len={}), params={}", sql, sql.len(), params_json);

            // Intercept SELECT LAST_INSERT_ID() / LAST_INSERT_ROWID() and serve
            // from the cached value on the WASM state — pool connections lose
            // session-local state between calls, so the underlying SELECT would
            // always return 0.
            if let Some(alias) = last_insert_id_alias(&sql) {
                let cached = caller.data().last_insert_id();
                debug!("_db_query: serving cached last_insert_id={:?} as alias='{}'", cached, alias);
                return write_string_to_caller(
                    &mut caller,
                    &last_insert_id_response(&alias, cached),
                );
            }

            let params: Vec<serde_json::Value> =
                serde_json::from_str(&params_json).unwrap_or_default();

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    return write_string_to_caller(
                        &mut caller,
                        r#"{"ok":false,"err":{"code":"NO_DB","message":"No database configured"}}"#,
                    );
                }
            };

            let sql_upper = sql.trim_start().to_uppercase();
            let method = if sql_upper.starts_with("INSERT")
                || sql_upper.starts_with("UPDATE")
                || sql_upper.starts_with("DELETE")
                || sql_upper.starts_with("CREATE")
                || sql_upper.starts_with("DROP")
                || sql_upper.starts_with("ALTER")
                || sql_upper.starts_with("TRUNCATE")
                || sql_upper.starts_with("REPLACE")
            {
                "execute"
            } else {
                "query"
            };

            let result = run_statement(
                &mut caller,
                db_bridge,
                method,
                json!({
                    "sql": sql,
                    "params": params
                }),
            );

            // Cache last_insert_id from successful INSERT/REPLACE so a follow-up
            // `SELECT LAST_INSERT_ID()` can be served from state.
            if is_insert(&sql) {
                if let Ok(ref v) = result {
                    if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                        if let Some(id) = v
                            .get("data")
                            .and_then(|d| d.get("last_insert_id"))
                            .and_then(|i| i.as_i64())
                        {
                            caller.data_mut().set_last_insert_id(Some(id));
                        }
                    }
                }
            }

            let result_str = match result {
                Ok(v) => {
                    let s = v.to_string();
                    debug!("_db_query: Query succeeded, result JSON ({} bytes): {}",
                           s.len(), if s.len() > 200 { format!("{}...", &s[..200]) } else { s.clone() });
                    s
                }
                Err(e) => {
                    let err_json = json!({
                        "ok": false,
                        "err": {
                            "code": "DB_ERROR",
                            "message": e.to_string()
                        }
                    });
                    error!("_db_query: Query failed: {}", e);
                    err_json.to_string()
                }
            };

            // DIAG-DB-QUERY-SHAPE: log full raw JSON bytes returned to WASM so
            // downstream json.get behaviour can be reconciled against the
            // envelope actually delivered. Debug level: at info it wrote every
            // query result (user data included) into the server log on every
            // request. Prefixed for greppability.
            debug!(
                "DIAG-DB-QUERY-SHAPE sql={:?} bytes={} raw_json={}",
                sql.chars().take(120).collect::<String>(),
                result_str.len(),
                result_str
            );
            debug!("_db_query: Result string is {} bytes", result_str.len());
            write_string_to_caller(&mut caller, &result_str)
        },
    )?;

    // =========================================
    // DATABASE EXECUTE
    // =========================================

    // _db_execute - Execute an INSERT/UPDATE/DELETE
    // Args: sql_ptr, sql_len, params_ptr, params_len (JSON array of params)
    // Returns: number of affected rows as i32 (or -1 on error)
    linker.func_wrap(
        "env",
        "_db_execute",
        |mut caller: Caller<'_, S>,
         sql_ptr: i32,
         sql_len: i32,
         params_ptr: i32,
         params_len: i32|
         -> i32 {
            let sql = match read_raw_string(&mut caller, sql_ptr, sql_len) {
                Some(s) => s,
                None => {
                    error!("_db_execute: Failed to read SQL string");
                    return -1;
                }
            };

            let params_json = if params_len > 0 {
                read_raw_string(&mut caller, params_ptr, params_len)
                    .unwrap_or_else(|| "[]".to_string())
            } else {
                "[]".to_string()
            };

            debug!("_db_execute: SQL={}, params={}", sql, params_json);

            let params: Vec<serde_json::Value> =
                serde_json::from_str(&params_json).unwrap_or_default();

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    error!("_db_execute: No database configured");
                    return -1;
                }
            };

            let result = run_statement(
                &mut caller,
                db_bridge,
                "execute",
                json!({
                    "sql": sql,
                    "params": params
                }),
            );

            match result {
                Ok(v) => {
                    if let Some(ok) = v.get("ok").and_then(|o| o.as_bool()) {
                        if ok {
                            // Cache last_insert_id for INSERT/REPLACE so a follow-up
                            // `SELECT LAST_INSERT_ID()` can be served from state.
                            if is_insert(&sql) {
                                if let Some(id) = v
                                    .get("data")
                                    .and_then(|d| d.get("last_insert_id"))
                                    .and_then(|i| i.as_i64())
                                {
                                    caller.data_mut().set_last_insert_id(Some(id));
                                }
                            }
                            v.get("data")
                                .and_then(|d| d.get("affected_rows"))
                                .and_then(|r| r.as_i64())
                                .unwrap_or(0) as i32
                        } else {
                            error!("_db_execute: Database execute failed: {:?}", v.get("err"));
                            -1
                        }
                    } else {
                        0
                    }
                }
                Err(e) => {
                    error!("_db_execute: Database error: {}", e);
                    -1
                }
            }
        },
    )?;

    // =========================================
    // TRANSACTIONS
    // =========================================

    // A transaction holds its own pooled connection from `_db_begin` until
    // `_db_commit` / `_db_rollback`; the statements run in between join it
    // (see `run_statement`). One transaction is open at a time per caller.
    linker.func_wrap("env", "_db_begin", |mut caller: Caller<'_, S>| -> i32 {
        let db_bridge = match caller.data().db_bridge() {
            Some(db) => db,
            None => {
                error!("_db_begin: No database configured");
                return 0;
            }
        };
        match caller.data_mut().transaction_slot() {
            None => {
                error!("_db_begin: this runtime does not support transactions");
                return 0;
            }
            Some(Some(_)) => {
                error!("_db_begin: a transaction is already open");
                return 0;
            }
            Some(None) => {}
        }

        let opened = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { db_bridge.read().await.begin_transaction().await })
        });

        match opened {
            Ok(tx) => {
                if let Some(slot) = caller.data_mut().transaction_slot() {
                    *slot = Some(tx);
                }
                debug!("_db_begin: Transaction started");
                1
            }
            Err(e) => {
                error!("_db_begin: Transaction begin error: {}", e);
                0
            }
        }
    })?;

    linker.func_wrap("env", "_db_commit", |mut caller: Caller<'_, S>| -> i32 {
        let tx = match caller.data_mut().transaction_slot().and_then(|s| s.take()) {
            Some(tx) => tx,
            None => {
                error!("_db_commit: No active transaction");
                return 0;
            }
        };
        let result =
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(tx.commit()));
        match result {
            Ok(()) => 1,
            Err(e) => {
                error!("_db_commit: {}", e);
                0
            }
        }
    })?;

    linker.func_wrap("env", "_db_rollback", |mut caller: Caller<'_, S>| -> i32 {
        let tx = match caller.data_mut().transaction_slot().and_then(|s| s.take()) {
            Some(tx) => tx,
            None => {
                error!("_db_rollback: No active transaction");
                return 0;
            }
        };
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(tx.rollback())
        });
        match result {
            Ok(()) => 1,
            Err(e) => {
                error!("_db_rollback: {}", e);
                0
            }
        }
    })?;

    // =========================================
    // MIGRATION REGISTRATION
    // =========================================

    linker.func_wrap(
        "env",
        "_db_register_migration",
        |mut caller: Caller<'_, S>,
         name_ptr: i32,
         name_len: i32,
         up_ptr: i32,
         up_len: i32,
         down_ptr: i32,
         down_len: i32|
         -> i32 {
            let name = match read_raw_string(&mut caller, name_ptr, name_len) {
                Some(s) => s,
                None => {
                    error!("_db_register_migration: Failed to read name string");
                    return 0;
                }
            };
            let up_sql = read_raw_string(&mut caller, up_ptr, up_len).unwrap_or_default();
            let down_sql = read_raw_string(&mut caller, down_ptr, down_len).unwrap_or_default();

            debug!(
                "_db_register_migration: name='{}', up_sql={} bytes, down_sql={} bytes",
                name,
                up_sql.len(),
                down_sql.len()
            );

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    error!("_db_register_migration: No database configured");
                    return 0;
                }
            };

            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut bridge = db_bridge.write().await;
                    bridge
                        .call(
                            "register_migration",
                            serde_json::json!({
                                "name": name,
                                "up_sql": up_sql,
                                "down_sql": down_sql
                            }),
                        )
                        .await
                })
            });

            match result {
                Ok(v) => {
                    if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                        1
                    } else {
                        error!("_db_register_migration: Failed: {:?}", v.get("err"));
                        0
                    }
                }
                Err(e) => {
                    error!("_db_register_migration: Error: {}", e);
                    0
                }
            }
        },
    )?;

    // =========================================
    // CONFIGURE — _db_configure
    // =========================================

    // _db_configure - Configure the connection pool from a JSON config string.
    // Args: config_ptr, config_len
    //   The JSON may be a full DbConfig object ({database_url, max_connections, ...})
    //   or a bare URL string.
    // Returns: 0 on success, -1 on error
    linker.func_wrap(
        "env",
        "_db_configure",
        |mut caller: Caller<'_, S>, config_ptr: i32, config_len: i32| -> i32 {
            let config_json = match read_raw_string(&mut caller, config_ptr, config_len) {
                Some(s) => s,
                None => {
                    error!("_db_configure: Failed to read config JSON");
                    return -1;
                }
            };

            debug!("_db_configure: config_len={}", config_json.len());

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    error!("_db_configure: No database bridge available");
                    return -1;
                }
            };

            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut bridge = db_bridge.write().await;
                    bridge.configure_from_json(&config_json).await
                })
            });

            match result {
                Ok(v) => {
                    if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                        0
                    } else {
                        error!("_db_configure: Configuration failed: {:?}", v.get("err"));
                        -1
                    }
                }
                Err(e) => {
                    error!("_db_configure: Error: {}", e);
                    -1
                }
            }
        },
    )?;

    // =========================================
    // PAGINATION — _db_paginate
    // =========================================

    // _db_paginate - Offset-based paginated query.
    // Args: table_ptr, table_len, where_ptr, where_len, page (i64, 1-based), per_page (i64)
    // Returns: ptr to PagedResult JSON string
    linker.func_wrap(
        "env",
        "_db_paginate",
        |mut caller: Caller<'_, S>,
         table_ptr: i32,
         table_len: i32,
         where_ptr: i32,
         where_len: i32,
         page: i64,
         per_page: i64|
         -> i32 {
            let table = match read_raw_string(&mut caller, table_ptr, table_len) {
                Some(s) => s,
                None => {
                    error!("_db_paginate: Failed to read table name");
                    return write_string_to_caller(
                        &mut caller,
                        r#"{"ok":false,"err":{"code":"MEMORY_ERROR","message":"Failed to read table name"}}"#,
                    );
                }
            };
            let where_json = if where_len > 0 {
                read_raw_string(&mut caller, where_ptr, where_len)
                    .unwrap_or_else(|| "{}".to_string())
            } else {
                "{}".to_string()
            };

            debug!("_db_paginate: table={}, page={}, per_page={}", table, page, per_page);

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    return write_string_to_caller(
                        &mut caller,
                        r#"{"ok":false,"err":{"code":"NO_DB","message":"No database configured"}}"#,
                    );
                }
            };

            let where_val: serde_json::Value =
                serde_json::from_str(&where_json).unwrap_or(serde_json::json!({}));

            let result = run_statement(
                &mut caller,
                db_bridge,
                "paginate",
                serde_json::json!({
                    "table": table,
                    "where": where_val,
                    "page": page,
                    "per_page": per_page
                }),
            );

            let result_str = match result {
                Ok(v) => v.to_string(),
                Err(e) => {
                    error!("_db_paginate: Error: {}", e);
                    serde_json::json!({
                        "ok": false,
                        "err": {"code": "DB_ERROR", "message": e.to_string()}
                    })
                    .to_string()
                }
            };

            write_string_to_caller(&mut caller, &result_str)
        },
    )?;

    // =========================================
    // CURSOR PAGINATION — _db_cursor_page
    // =========================================

    // _db_cursor_page - Cursor-based paginated query.
    // Args: table_ptr, table_len, where_ptr, where_len, per_page (i64),
    //       after_ptr, after_len, by_field_ptr, by_field_len
    // Returns: ptr to CursorResult JSON string
    linker.func_wrap(
        "env",
        "_db_cursor_page",
        |mut caller: Caller<'_, S>,
         table_ptr: i32,
         table_len: i32,
         where_ptr: i32,
         where_len: i32,
         per_page: i64,
         after_ptr: i32,
         after_len: i32,
         by_field_ptr: i32,
         by_field_len: i32|
         -> i32 {
            let table = match read_raw_string(&mut caller, table_ptr, table_len) {
                Some(s) => s,
                None => {
                    error!("_db_cursor_page: Failed to read table name");
                    return write_string_to_caller(
                        &mut caller,
                        r#"{"ok":false,"err":{"code":"MEMORY_ERROR","message":"Failed to read table name"}}"#,
                    );
                }
            };
            let where_json = if where_len > 0 {
                read_raw_string(&mut caller, where_ptr, where_len)
                    .unwrap_or_else(|| "{}".to_string())
            } else {
                "{}".to_string()
            };
            let after = if after_len > 0 {
                read_raw_string(&mut caller, after_ptr, after_len).unwrap_or_default()
            } else {
                String::new()
            };
            let by_field = if by_field_len > 0 {
                read_raw_string(&mut caller, by_field_ptr, by_field_len)
                    .unwrap_or_else(|| "id".to_string())
            } else {
                "id".to_string()
            };

            debug!(
                "_db_cursor_page: table={}, per_page={}, after={}, by_field={}",
                table, per_page, after, by_field
            );

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    return write_string_to_caller(
                        &mut caller,
                        r#"{"ok":false,"err":{"code":"NO_DB","message":"No database configured"}}"#,
                    );
                }
            };

            let where_val: serde_json::Value =
                serde_json::from_str(&where_json).unwrap_or(serde_json::json!({}));

            let result = run_statement(
                &mut caller,
                db_bridge,
                "cursor_page",
                serde_json::json!({
                    "table": table,
                    "where": where_val,
                    "per_page": per_page,
                    "after": after,
                    "by_field": by_field
                }),
            );

            let result_str = match result {
                Ok(v) => v.to_string(),
                Err(e) => {
                    error!("_db_cursor_page: Error: {}", e);
                    serde_json::json!({
                        "ok": false,
                        "err": {"code": "DB_ERROR", "message": e.to_string()}
                    })
                    .to_string()
                }
            };

            write_string_to_caller(&mut caller, &result_str)
        },
    )?;

    // =========================================
    // MIGRATION DIFF — _db_migration_diff
    // =========================================

    // _db_migration_diff - Compare declared model vs live schema.
    // Args: declared_ptr, declared_len  — JSON {column: "type", ...} for the declared model
    //       live_ptr, live_len          — table name string OR live-schema JSON object
    // Returns: ptr to ALTER TABLE SQL string (empty string when already in sync)
    linker.func_wrap(
        "env",
        "_db_migration_diff",
        |mut caller: Caller<'_, S>,
         declared_ptr: i32,
         declared_len: i32,
         live_ptr: i32,
         live_len: i32|
         -> i32 {
            let declared_json = match read_raw_string(&mut caller, declared_ptr, declared_len) {
                Some(s) => s,
                None => {
                    error!("_db_migration_diff: Failed to read declared JSON");
                    return write_string_to_caller(&mut caller, "");
                }
            };
            let live_json = if live_len > 0 {
                read_raw_string(&mut caller, live_ptr, live_len).unwrap_or_else(|| "{}".to_string())
            } else {
                "{}".to_string()
            };

            debug!(
                "_db_migration_diff: declared_len={}, live_len={}",
                declared_len, live_len
            );

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    error!("_db_migration_diff: No database configured");
                    return write_string_to_caller(&mut caller, "");
                }
            };

            let declared_val: serde_json::Value =
                serde_json::from_str(&declared_json).unwrap_or(serde_json::json!({}));

            // The second argument is either a JSON object (live schema) or a plain table name.
            let (table_opt, live_val) = match serde_json::from_str::<serde_json::Value>(&live_json)
            {
                Ok(v) if v.is_object() => (None::<String>, v),
                Ok(v) if v.is_string() => {
                    (v.as_str().map(|s| s.to_string()), serde_json::json!({}))
                }
                _ => (
                    Some(live_json.trim().trim_matches('"').to_string()),
                    serde_json::json!({}),
                ),
            };

            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut bridge = db_bridge.write().await;
                    bridge
                        .call(
                            "migration_diff",
                            serde_json::json!({
                                "table": table_opt,
                                "declared": declared_val,
                                "live": live_val
                            }),
                        )
                        .await
                })
            });

            let diff_sql = match result {
                Ok(v) => v
                    .get("data")
                    .and_then(|d| d.get("sql"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                Err(e) => {
                    error!("_db_migration_diff: Error: {}", e);
                    String::new()
                }
            };

            write_string_to_caller(&mut caller, &diff_sql)
        },
    )?;

    // =========================================
    // MIGRATION STATUS — _db_migration_status
    // =========================================

    // _db_migration_status - Return JSON array of migration status records.
    // Args: none
    // Returns: ptr to JSON array string: [{"name":"...","applied_at":"..."|null}, ...]
    linker.func_wrap(
        "env",
        "_db_migration_status",
        |mut caller: Caller<'_, S>| -> i32 {
            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    return write_string_to_caller(&mut caller, "[]");
                }
            };

            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut bridge = db_bridge.write().await;
                    bridge.call("migration_status", serde_json::json!({})).await
                })
            });

            let result_str = match result {
                Ok(v) => {
                    // Unwrap the envelope and return just the migrations array
                    if let Some(arr) = v.get("data").and_then(|d| d.get("migrations")) {
                        arr.to_string()
                    } else {
                        "[]".to_string()
                    }
                }
                Err(e) => {
                    error!("_db_migration_status: Error: {}", e);
                    "[]".to_string()
                }
            };

            write_string_to_caller(&mut caller, &result_str)
        },
    )?;

    // =========================================
    // ROLLBACK MIGRATION — _db_rollback_migration
    // =========================================

    // _db_rollback_migration - Rollback a specific migration by name.
    // Args: name_ptr, name_len
    // Returns: 0 on success, -1 on error
    linker.func_wrap(
        "env",
        "_db_rollback_migration",
        |mut caller: Caller<'_, S>, name_ptr: i32, name_len: i32| -> i32 {
            let name = match read_raw_string(&mut caller, name_ptr, name_len) {
                Some(s) => s,
                None => {
                    error!("_db_rollback_migration: Failed to read migration name");
                    return -1;
                }
            };

            debug!("_db_rollback_migration: name={}", name);

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    error!("_db_rollback_migration: No database configured");
                    return -1;
                }
            };

            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut bridge = db_bridge.write().await;
                    bridge
                        .call("rollback_migration", serde_json::json!({"name": name}))
                        .await
                })
            });

            match result {
                Ok(v) => {
                    if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                        0
                    } else {
                        error!("_db_rollback_migration: Failed: {:?}", v.get("err"));
                        -1
                    }
                }
                Err(e) => {
                    error!("_db_rollback_migration: Error: {}", e);
                    -1
                }
            }
        },
    )?;

    // =========================================
    // RUN MIGRATIONS — _db_run_migrations
    // =========================================

    // _db_run_migrations - Apply all pending registered migrations in version order.
    // Args: none
    // Returns: count of migrations applied (i32 >= 0), or -1 on error
    linker.func_wrap(
        "env",
        "_db_run_migrations",
        |caller: Caller<'_, S>| -> i32 {
            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    error!("_db_run_migrations: No database configured");
                    return -1;
                }
            };

            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut bridge = db_bridge.write().await;
                    bridge.call("run_migrations", serde_json::json!({})).await
                })
            });

            match result {
                Ok(v) => {
                    if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                        v.get("data")
                            .and_then(|d| d.get("applied"))
                            .and_then(|a| a.as_i64())
                            .unwrap_or(0) as i32
                    } else {
                        error!("_db_run_migrations: Failed: {:?}", v.get("err"));
                        -1
                    }
                }
                Err(e) => {
                    error!("_db_run_migrations: Error: {}", e);
                    -1
                }
            }
        },
    )?;

    // =========================================
    // FIELD VALIDATION — _db_valid_field
    // =========================================

    // _db_valid_field - Runtime ORDER BY safety check.
    // Args: table_ptr, table_len, field_ptr, field_len
    // Returns: 1 (true) when field is a valid column in table, 0 otherwise
    linker.func_wrap(
        "env",
        "_db_valid_field",
        |mut caller: Caller<'_, S>,
         table_ptr: i32,
         table_len: i32,
         field_ptr: i32,
         field_len: i32|
         -> i32 {
            let table = match read_raw_string(&mut caller, table_ptr, table_len) {
                Some(s) => s,
                None => {
                    error!("_db_valid_field: Failed to read table name");
                    return 0;
                }
            };
            let field = match read_raw_string(&mut caller, field_ptr, field_len) {
                Some(s) => s,
                None => {
                    error!("_db_valid_field: Failed to read field name");
                    return 0;
                }
            };

            debug!("_db_valid_field: table={}, field={}", table, field);

            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    // No DB configured: conservatively deny
                    return 0;
                }
            };

            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut bridge = db_bridge.write().await;
                    bridge
                        .call(
                            "valid_field",
                            serde_json::json!({"table": table, "field": field}),
                        )
                        .await
                })
            });

            match result {
                Ok(v) => {
                    if v.get("data")
                        .and_then(|d| d.get("valid"))
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false)
                    {
                        1
                    } else {
                        0
                    }
                }
                Err(e) => {
                    error!("_db_valid_field: Error: {}", e);
                    0
                }
            }
        },
    )?;

    // =========================================
    // DB ASYNC EXTRAS (Phase 2)
    //
    // Async-only drivers split each query into "fire" (start) and "result"
    // (collect) host calls. Rust drivers (sqlx) are sync-capable via
    // block_on, so the "async" variant here just runs the same sync path and
    // caches the result for the matching getter. See HOST_BRIDGE.md § DB Async.
    // =========================================

    // _db_connected() -> boolean
    linker.func_wrap("env", "_db_connected", |caller: Caller<'_, S>| -> i32 {
        if caller.data().db_bridge().is_some() {
            1
        } else {
            0
        }
    })?;

    // _db_query_async(sql, params) -> void  — runs sync, caches into thread_local
    linker.func_wrap("env", "_db_query_async",
        |mut caller: Caller<'_, S>, sp: i32, sl: i32, pp: i32, pl: i32| {
            let sql = read_raw_string(&mut caller, sp, sl).unwrap_or_default();
            let params_json = if pl > 0 {
                read_raw_string(&mut caller, pp, pl).unwrap_or_else(|| "[]".to_string())
            } else {
                "[]".to_string()
            };
            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    LAST_QUERY_RESULT_JSON.with(|c| *c.borrow_mut() = r#"{"ok":false,"err":{"code":"NO_DB","message":"No database configured"}}"#.to_string());
                    return;
                }
            };
            let params: Vec<serde_json::Value> = serde_json::from_str(&params_json).unwrap_or_default();
            let sql_upper = sql.trim_start().to_uppercase();
            let method = if sql_upper.starts_with("INSERT") || sql_upper.starts_with("UPDATE")
                || sql_upper.starts_with("DELETE") || sql_upper.starts_with("CREATE")
                || sql_upper.starts_with("DROP") || sql_upper.starts_with("ALTER")
                || sql_upper.starts_with("TRUNCATE") || sql_upper.starts_with("REPLACE")
            { "execute" } else { "query" };
            let result = run_statement(
                &mut caller,
                db_bridge,
                method,
                json!({"sql": sql, "params": params}),
            );
            let s = match result {
                Ok(v) => v.to_string(),
                Err(e) => json!({"ok":false,"err":{"code":"DB_ERROR","message":e.to_string()}}).to_string(),
            };
            LAST_QUERY_RESULT_JSON.with(|c| *c.borrow_mut() = s);
        })?;

    // _db_query_result() -> ptr (last cached query JSON)
    linker.func_wrap(
        "env",
        "_db_query_result",
        |mut caller: Caller<'_, S>| -> i32 {
            let s = LAST_QUERY_RESULT_JSON.with(|c| c.borrow().clone());
            write_string_to_caller(&mut caller, &s)
        },
    )?;

    // _db_execute_async(sql, params) -> void — runs sync, caches affected_rows
    linker.func_wrap(
        "env",
        "_db_execute_async",
        |mut caller: Caller<'_, S>, sp: i32, sl: i32, pp: i32, pl: i32| {
            let sql = read_raw_string(&mut caller, sp, sl).unwrap_or_default();
            let params_json = if pl > 0 {
                read_raw_string(&mut caller, pp, pl).unwrap_or_else(|| "[]".to_string())
            } else {
                "[]".to_string()
            };
            let db_bridge = match caller.data().db_bridge() {
                Some(db) => db,
                None => {
                    LAST_EXECUTE_AFFECTED.with(|c| *c.borrow_mut() = -1);
                    return;
                }
            };
            let params: Vec<serde_json::Value> =
                serde_json::from_str(&params_json).unwrap_or_default();
            let result = run_statement(
                &mut caller,
                db_bridge,
                "execute",
                json!({"sql": sql, "params": params}),
            );
            let affected: i32 = match result {
                Ok(v) => v
                    .get("data")
                    .and_then(|d| d.get("affected_rows"))
                    .and_then(|n| n.as_i64())
                    .map(|n| n as i32)
                    .unwrap_or(0),
                Err(_) => -1,
            };
            LAST_EXECUTE_AFFECTED.with(|c| *c.borrow_mut() = affected);
        },
    )?;

    // _db_execute_result() -> i32 (last cached affected_rows)
    linker.func_wrap("env", "_db_execute_result", |_: Caller<'_, S>| -> i32 {
        LAST_EXECUTE_AFFECTED.with(|c| *c.borrow())
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_insert, last_insert_id_alias, last_insert_id_response};

    // -----------------------------------------------------------------
    // Reproduces FRAME-DATA-LAST-INSERT-ID-ZERO (server `ba39f757de3c`).
    //
    // The original bridge dispatched `SELECT LAST_INSERT_ID() AS id` to the
    // MySQL pool, which acquired a fresh connection — losing the session-local
    // id from the prior INSERT — and returned 0. The fix moves that pattern
    // into the bridge: `last_insert_id_alias` recognizes the SQL shape, the
    // INSERT path caches the id on the WASM state, and the synthetic SELECT
    // response is built without touching the pool.
    // -----------------------------------------------------------------

    #[test]
    fn last_insert_id_alias_matches_bare_mysql() {
        assert_eq!(
            last_insert_id_alias("SELECT LAST_INSERT_ID()"),
            Some("id".to_string())
        );
    }

    #[test]
    fn last_insert_id_alias_matches_bare_sqlite() {
        assert_eq!(
            last_insert_id_alias("SELECT LAST_INSERT_ROWID()"),
            Some("id".to_string())
        );
    }

    #[test]
    fn last_insert_id_alias_matches_aliased() {
        assert_eq!(
            last_insert_id_alias("SELECT LAST_INSERT_ID() AS id"),
            Some("id".to_string())
        );
        assert_eq!(
            last_insert_id_alias("SELECT LAST_INSERT_ID() AS new_id"),
            Some("new_id".to_string())
        );
    }

    #[test]
    fn last_insert_id_alias_tolerates_whitespace_and_case() {
        assert_eq!(
            last_insert_id_alias("  select   last_insert_id()   as   id  "),
            Some("id".to_string())
        );
        assert_eq!(
            last_insert_id_alias("SELECT LAST_INSERT_ID ( ) AS id"),
            Some("id".to_string())
        );
        assert_eq!(
            last_insert_id_alias("SELECT LAST_INSERT_ID() AS id;"),
            Some("id".to_string())
        );
    }

    #[test]
    fn last_insert_id_alias_rejects_unrelated_queries() {
        assert_eq!(last_insert_id_alias("SELECT * FROM t"), None);
        assert_eq!(
            last_insert_id_alias("SELECT id FROM t WHERE id = LAST_INSERT_ID()"),
            None
        );
        assert_eq!(
            last_insert_id_alias("SELECT LAST_INSERT_ID(), other_col FROM t"),
            None
        );
    }

    #[test]
    fn last_insert_id_response_shape_matches_select_envelope() {
        let body = last_insert_id_response("id", Some(42));
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["data"]["count"], serde_json::json!(1));
        assert_eq!(v["data"]["rows"][0]["id"], serde_json::json!(42));
    }

    #[test]
    fn last_insert_id_response_returns_zero_when_cache_empty() {
        // Before any INSERT, the cache is empty — match the historical
        // behavior so callers see "0" rather than a missing field. The bug
        // is fixed precisely by the cache being populated after INSERT.
        let body = last_insert_id_response("new_id", None);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["data"]["rows"][0]["new_id"], serde_json::json!(0));
    }

    #[test]
    fn is_insert_recognizes_insert_and_replace() {
        assert!(is_insert("INSERT INTO t VALUES (1)"));
        assert!(is_insert("  insert  into t values (1)"));
        assert!(is_insert("REPLACE INTO t VALUES (1)"));
        assert!(!is_insert("SELECT 1"));
        assert!(!is_insert("UPDATE t SET n = 1"));
    }
}
