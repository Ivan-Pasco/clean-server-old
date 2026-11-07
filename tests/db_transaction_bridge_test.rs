//! `_db_begin` / `_db_commit` / `_db_rollback` open a real transaction that
//! `_db_query` and `_db_execute` join.
//!
//! Regression: `_db_begin` opened a buffered transaction record that the
//! statement bridges never used — `_db_execute` wrote straight to the pool, so
//! `_db_rollback` undid nothing and frame.data's `transaction:` block kept
//! the writes of a failed body. The test runs against a file-backed SQLite
//! database so the pool really holds several connections.

use clean_server::router::Router;
use clean_server::wasm::{RequestContext, WasmInstance};
use host_bridge::{DbBridge, DbConfig, SharedDbBridge};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Each export runs one scenario and returns the `_db_query` result of
/// `SELECT COUNT(*) AS n FROM t` (taken inside the transaction for
/// `count_inside`).
const DRIVER_WAT: &str = r#"
(module
  (import "env" "_db_execute" (func $exec (param i32 i32 i32 i32) (result i32)))
  (import "env" "_db_query" (func $query (param i32 i32 i32 i32) (result i32)))
  (import "env" "_db_begin" (func $begin (result i32)))
  (import "env" "_db_commit" (func $commit (result i32)))
  (import "env" "_db_rollback" (func $rollback (result i32)))

  (memory (export "memory") 2)
  (global $heap (export "__heap_ptr") (mut i32) (i32.const 65536))

  ;; Bump allocator the host uses to hand back query results.
  (func (export "malloc") (param $size i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $p))

  (data (i32.const 0) "CREATE TABLE t (v INTEGER)")
  (data (i32.const 64) "INSERT INTO t (v) VALUES (1)")
  (data (i32.const 128) "SELECT COUNT(*) AS n FROM t")

  (func $count (result i32)
    (call $query (i32.const 128) (i32.const 27) (i32.const 0) (i32.const 0)))
  (func $insert
    (drop (call $exec (i32.const 64) (i32.const 28) (i32.const 0) (i32.const 0))))

  (func (export "create") (result i32)
    (drop (call $exec (i32.const 0) (i32.const 26) (i32.const 0) (i32.const 0)))
    (call $count))

  (func (export "insert_rollback") (result i32)
    (drop (call $begin))
    (call $insert)
    (drop (call $rollback))
    (call $count))

  (func (export "insert_commit") (result i32)
    (drop (call $begin))
    (call $insert)
    (drop (call $commit))
    (call $count))

  (func (export "count_inside") (result i32)
    (local $r i32)
    (drop (call $begin))
    (call $insert)
    (local.set $r (call $count))
    (drop (call $rollback))
    (local.get $r))

  (func (export "insert_unfinished") (result i32)
    (drop (call $begin))
    (call $insert)
    (call $count))

  (func (export "count") (result i32)
    (call $count))
)
"#;

fn request() -> RequestContext {
    RequestContext {
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: Vec::new(),
        body: String::new(),
        body_bytes: None,
        params: Default::default(),
        query: Default::default(),
    }
}

fn row_count(result_json: &str) -> i64 {
    let v: serde_json::Value = serde_json::from_str(result_json)
        .unwrap_or_else(|e| panic!("query result is not JSON ({e}): {result_json}"));
    v["data"]["rows"][0]["n"]
        .as_i64()
        .unwrap_or_else(|| panic!("no count in query result: {result_json}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn statements_join_the_open_transaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("tx.db");
    std::fs::File::create(&db_path).expect("create database file");

    let mut bridge = DbBridge::new();
    bridge
        .configure(DbConfig {
            database_url: format!("sqlite://{}", db_path.display()),
            max_connections: 4,
            min_connections: 2,
            connection_timeout: 5000,
            query_timeout: 5000,
        })
        .await
        .expect("configure SQLite");
    let db: SharedDbBridge = Arc::new(RwLock::new(bridge));

    let wasm = wat::parse_str(DRIVER_WAT).expect("assemble driver");
    let instance =
        WasmInstance::from_bytes_with_db(&wasm, Arc::new(Router::new()), db).expect("load");
    let call = |name: &str| {
        row_count(
            &instance
                .call_handler(name, request())
                .unwrap_or_else(|e| panic!("{name} failed: {e}")),
        )
    };

    assert_eq!(call("create"), 0);
    assert_eq!(
        call("insert_rollback"),
        0,
        "db.rollback() must undo an insert made inside the transaction"
    );
    assert_eq!(call("insert_commit"), 1, "db.commit() keeps the insert");
    assert_eq!(
        call("count_inside"),
        2,
        "a query inside the transaction sees the transaction's own insert"
    );
    assert_eq!(
        call("count"),
        1,
        "the rolled-back insert is gone afterwards"
    );
    assert_eq!(
        call("insert_unfinished"),
        2,
        "inside the unfinished transaction the insert is visible"
    );
    assert_eq!(
        call("count"),
        1,
        "a transaction left open when the request ends is rolled back"
    );
}
