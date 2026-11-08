use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Row, TypeInfo};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

// ============================================================================
// DATABASE DRIVER - Native driver with runtime dispatch
// ============================================================================

/// The database engines clean-server (and frame.data) support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseKind {
    /// `postgres://` or `postgresql://`
    Postgres,
    /// `sqlite:` (`sqlite::memory:`, `sqlite:app.db`, `sqlite://path`)
    Sqlite,
}

/// The URL forms accepted for `DATABASE_URL` / `--database`.
pub const SUPPORTED_DATABASE_URLS: &str =
    "PostgreSQL (postgres://, postgresql://) and SQLite (sqlite:)";

/// Check that `url` names a supported database engine, without connecting.
///
/// MySQL and MariaDB are rejected by name: frame.data generates
/// PostgreSQL/SQLite SQL (`$n` placeholders, `INSERT ... RETURNING`), which
/// MySQL does not accept, so a MySQL URL would start a server whose writes
/// fail at run time.
pub fn check_database_url(url: &str) -> Result<DatabaseKind> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        return Ok(DatabaseKind::Postgres);
    }
    if url.starts_with("sqlite:") {
        return Ok(DatabaseKind::Sqlite);
    }
    if url.starts_with("mysql:") || url.starts_with("mariadb:") {
        return Err(anyhow::anyhow!(
            "MySQL is not supported. Supported databases: {}",
            SUPPORTED_DATABASE_URLS
        ));
    }
    let scheme = url.split(':').next().unwrap_or("");
    Err(anyhow::anyhow!(
        "unsupported database URL scheme '{}'. Supported databases: {}",
        scheme,
        SUPPORTED_DATABASE_URLS
    ))
}

/// Native database driver with full type support
/// Dispatches to PostgreSQL or SQLite based on connection URL
/// (see [`check_database_url`]; MySQL is not supported)
#[derive(Clone)]
pub enum DatabaseDriver {
    Postgres(PgPool),
    Sqlite(SqlitePool),
}

impl DatabaseDriver {
    /// Connect to a database based on the URL scheme
    pub async fn connect(url: &str, config: &DbConfig) -> Result<Self> {
        match check_database_url(url)? {
            DatabaseKind::Postgres => {
                let pool = PgPoolOptions::new()
                    .max_connections(config.max_connections)
                    .min_connections(config.min_connections)
                    .acquire_timeout(Duration::from_millis(config.connection_timeout))
                    .idle_timeout(Duration::from_secs(90))
                    .max_lifetime(Duration::from_secs(1800))
                    .test_before_acquire(true)
                    .connect(url)
                    .await?;
                Ok(Self::Postgres(pool))
            }
            DatabaseKind::Sqlite => {
                let pool = SqlitePoolOptions::new()
                    .max_connections(config.max_connections)
                    .min_connections(config.min_connections)
                    .acquire_timeout(Duration::from_millis(config.connection_timeout))
                    .idle_timeout(Duration::from_secs(90))
                    .test_before_acquire(true)
                    .connect(url)
                    .await?;
                Ok(Self::Sqlite(pool))
            }
        }
    }

    /// Adapt SQL the host builds itself (written with `?` placeholders) to the
    /// driver's placeholder syntax: PostgreSQL numbers them `$1, $2, ...`.
    /// Application SQL is passed through untouched by the query paths; only
    /// host-generated statements go through here.
    pub fn host_sql(&self, sql: &str) -> String {
        match self {
            Self::Postgres(_) => number_placeholders(sql),
            Self::Sqlite(_) => sql.to_string(),
        }
    }

    /// Execute a SELECT query and return rows as JSON
    pub async fn query(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<serde_json::Map<String, Value>>> {
        match self {
            Self::Postgres(pool) => Self::query_postgres(pool, sql, params).await,
            Self::Sqlite(pool) => Self::query_sqlite(pool, sql, params).await,
        }
    }

    /// Execute an INSERT/UPDATE/DELETE and return affected rows
    pub async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult> {
        match self {
            Self::Postgres(pool) => Self::execute_postgres(pool, sql, params).await,
            Self::Sqlite(pool) => Self::execute_sqlite(pool, sql, params).await,
        }
    }

    /// Open a transaction on a connection taken from the pool. Every
    /// statement run through the returned handle joins the transaction;
    /// dropping the handle without [`OpenTransaction::commit`] rolls it back.
    pub async fn begin(&self) -> Result<OpenTransaction> {
        Ok(match self {
            Self::Postgres(pool) => OpenTransaction::Postgres(pool.begin().await?),
            Self::Sqlite(pool) => OpenTransaction::Sqlite(pool.begin().await?),
        })
    }

    /// Begin a real database transaction and execute operations
    pub async fn execute_transaction(&self, operations: &[(String, Vec<Value>)]) -> Result<()> {
        match self {
            Self::Postgres(pool) => Self::execute_transaction_postgres(pool, operations).await,
            Self::Sqlite(pool) => Self::execute_transaction_sqlite(pool, operations).await,
        }
    }

    // ========================================================================
    // Typed bind tags
    // ========================================================================

    /// If a JSON object carries a typed bind tag, decode it into a value sqlx
    /// can bind to the driver's native datetime/timestamp column type.
    ///
    /// Recognised shapes:
    ///   `{"__type": "epoch_s",      "value": <i64>}`  → unix epoch in seconds
    ///   `{"__type": "epoch_ms",     "value": <i64>}`  → unix epoch in milliseconds
    ///   `{"__type": "datetime_iso", "value": "<RFC3339 string>"}`
    ///
    /// Returns `Some(NaiveDateTime)` when the tag is present and parses,
    /// `None` otherwise (let the caller fall through to normal binding).
    fn decode_typed_bind(param: &Value) -> Option<chrono::NaiveDateTime> {
        let obj = param.as_object()?;
        let tag = obj.get("__type")?.as_str()?;
        let value = obj.get("value")?;
        match tag {
            "epoch_s" => {
                let secs = value.as_i64()?;
                chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).map(|dt| dt.naive_utc())
            }
            "epoch_ms" => {
                let ms = value.as_i64()?;
                chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|dt| dt.naive_utc())
            }
            "datetime_iso" => {
                let s = value.as_str()?;
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.naive_utc())
            }
            _ => None,
        }
    }

    // ========================================================================
    // PostgreSQL Implementation
    // ========================================================================

    async fn query_postgres<'c, E>(
        executor: E,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<serde_json::Map<String, Value>>>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let mut query = sqlx::query(sql);

        for param in params {
            query = Self::bind_param_postgres(query, param);
        }

        let rows = query.fetch_all(executor).await?;

        let mut result = Vec::new();
        for row in rows {
            let map = Self::row_to_json_postgres(&row)?;
            result.push(map);
        }

        Ok(result)
    }

    fn bind_param_postgres<'q>(
        query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
        param: &Value,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        if let Some(dt) = Self::decode_typed_bind(param) {
            return query.bind(dt);
        }
        match param {
            Value::Null => query.bind(None::<String>),
            Value::Bool(b) => query.bind(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    query.bind(i)
                } else if let Some(f) = n.as_f64() {
                    query.bind(f)
                } else {
                    query.bind(n.to_string())
                }
            }
            Value::String(s) => query.bind(s.clone()),
            Value::Array(_) | Value::Object(_) => query.bind(param.to_string()),
        }
    }

    fn row_to_json_postgres(row: &PgRow) -> Result<serde_json::Map<String, Value>> {
        let mut map = serde_json::Map::new();

        for (i, column) in row.columns().iter().enumerate() {
            let name = column.name().to_string();
            let type_info = column.type_info();
            let type_name = type_info.name();

            let value = match type_name {
                "BOOL" => row
                    .try_get::<bool, _>(i)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                "INT2" | "SMALLINT" => row
                    .try_get::<i16, _>(i)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                "INT4" | "INT" | "INTEGER" => row
                    .try_get::<i32, _>(i)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                "INT8" | "BIGINT" => row
                    .try_get::<i64, _>(i)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                "FLOAT4" | "REAL" => row
                    .try_get::<f32, _>(i)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                "FLOAT8" | "DOUBLE PRECISION" => row
                    .try_get::<f64, _>(i)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                "TEXT" | "VARCHAR" | "CHAR" | "BPCHAR" | "NAME" => row
                    .try_get::<String, _>(i)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                "TIMESTAMP" | "TIMESTAMPTZ" => {
                    // Use chrono for proper timestamp handling
                    row.try_get::<chrono::NaiveDateTime, _>(i)
                        .map(|v| json!(v.to_string()))
                        .or_else(|_| {
                            row.try_get::<chrono::DateTime<chrono::Utc>, _>(i)
                                .map(|v| json!(v.to_rfc3339()))
                        })
                        .unwrap_or(Value::Null)
                }
                "DATE" => row
                    .try_get::<chrono::NaiveDate, _>(i)
                    .map(|v| json!(v.to_string()))
                    .unwrap_or(Value::Null),
                "TIME" | "TIMETZ" => row
                    .try_get::<chrono::NaiveTime, _>(i)
                    .map(|v| json!(v.to_string()))
                    .unwrap_or(Value::Null),
                "UUID" => row
                    .try_get::<uuid::Uuid, _>(i)
                    .map(|v| json!(v.to_string()))
                    .unwrap_or(Value::Null),
                "JSON" | "JSONB" => row
                    .try_get::<serde_json::Value, _>(i)
                    .unwrap_or(Value::Null),
                "BYTEA" => row
                    .try_get::<Vec<u8>, _>(i)
                    .map(|v| {
                        json!(base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &v
                        ))
                    })
                    .unwrap_or(Value::Null),
                _ => {
                    // Fallback: try to get as string
                    row.try_get::<String, _>(i)
                        .map(|v| json!(v))
                        .unwrap_or(Value::Null)
                }
            };

            map.insert(name, value);
        }

        Ok(map)
    }

    async fn execute_postgres<'c, E>(
        executor: E,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecuteResult>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let mut query = sqlx::query(sql);

        for param in params {
            query = Self::bind_param_postgres(query, param);
        }

        let result = query.execute(executor).await?;

        Ok(ExecuteResult {
            rows_affected: result.rows_affected(),
            last_insert_id: None, // PostgreSQL uses RETURNING clause instead
        })
    }

    async fn execute_transaction_postgres(
        pool: &PgPool,
        operations: &[(String, Vec<Value>)],
    ) -> Result<()> {
        let mut tx = pool.begin().await?;

        for (sql, params) in operations {
            let mut query = sqlx::query(sql);
            for param in params {
                query = Self::bind_param_postgres(query, param);
            }
            query.execute(&mut *tx).await?;
        }

        tx.commit().await?;
        Ok(())
    }

    // ========================================================================
    // SQLite Implementation
    // ========================================================================

    async fn query_sqlite<'c, E>(
        executor: E,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<serde_json::Map<String, Value>>>
    where
        E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
    {
        let mut query = sqlx::query(sql);

        for param in params {
            query = Self::bind_param_sqlite(query, param);
        }

        let rows = query.fetch_all(executor).await?;

        let mut result = Vec::new();
        for row in rows {
            let map = Self::row_to_json_sqlite(&row)?;
            result.push(map);
        }

        Ok(result)
    }

    fn bind_param_sqlite<'q>(
        query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
        param: &Value,
    ) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
        if let Some(dt) = Self::decode_typed_bind(param) {
            return query.bind(dt);
        }
        match param {
            Value::Null => query.bind(None::<String>),
            Value::Bool(b) => query.bind(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    query.bind(i)
                } else if let Some(f) = n.as_f64() {
                    query.bind(f)
                } else {
                    query.bind(n.to_string())
                }
            }
            Value::String(s) => query.bind(s.clone()),
            Value::Array(_) | Value::Object(_) => query.bind(param.to_string()),
        }
    }

    fn row_to_json_sqlite(row: &SqliteRow) -> Result<serde_json::Map<String, Value>> {
        let mut map = serde_json::Map::new();

        for (i, column) in row.columns().iter().enumerate() {
            let name = column.name().to_string();

            // SQLite is dynamically typed - always try to determine the actual type at runtime
            // Type info from SQLite can be unreliable for expressions, aliases, and aggregates
            let value = if let Ok(v) = row.try_get::<i64, _>(i) {
                json!(v)
            } else if let Ok(v) = row.try_get::<i32, _>(i) {
                json!(v)
            } else if let Ok(v) = row.try_get::<f64, _>(i) {
                json!(v)
            } else if let Ok(v) = row.try_get::<String, _>(i) {
                json!(v)
            } else if let Ok(v) = row.try_get::<bool, _>(i) {
                json!(v)
            } else if let Ok(v) = row.try_get::<Vec<u8>, _>(i) {
                json!(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &v
                ))
            } else {
                Value::Null
            };

            map.insert(name, value);
        }

        Ok(map)
    }

    async fn execute_sqlite<'c, E>(
        executor: E,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecuteResult>
    where
        E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
    {
        let mut query = sqlx::query(sql);

        for param in params {
            query = Self::bind_param_sqlite(query, param);
        }

        let result = query.execute(executor).await?;

        Ok(ExecuteResult {
            rows_affected: result.rows_affected(),
            last_insert_id: Some(result.last_insert_rowid()),
        })
    }

    async fn execute_transaction_sqlite(
        pool: &SqlitePool,
        operations: &[(String, Vec<Value>)],
    ) -> Result<()> {
        let mut tx = pool.begin().await?;

        for (sql, params) in operations {
            let mut query = sqlx::query(sql);
            for param in params {
                query = Self::bind_param_sqlite(query, param);
            }
            query.execute(&mut *tx).await?;
        }

        tx.commit().await?;
        Ok(())
    }
}

// ============================================================================
// OPEN TRANSACTIONS
// ============================================================================

/// A database transaction opened by `_db_begin`, holding its own pooled
/// connection until it is committed or rolled back. Queries issued while it
/// is open run on that connection, so they see the transaction's writes and
/// `rollback` undoes them. Dropping it without committing rolls it back.
pub enum OpenTransaction {
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
}

impl OpenTransaction {
    /// Run a SELECT inside the transaction.
    pub async fn query(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<serde_json::Map<String, Value>>> {
        match self {
            Self::Postgres(tx) => DatabaseDriver::query_postgres(&mut **tx, sql, params).await,
            Self::Sqlite(tx) => DatabaseDriver::query_sqlite(&mut **tx, sql, params).await,
        }
    }

    /// Run an INSERT/UPDATE/DELETE (or DDL) inside the transaction.
    pub async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<ExecuteResult> {
        match self {
            Self::Postgres(tx) => DatabaseDriver::execute_postgres(&mut **tx, sql, params).await,
            Self::Sqlite(tx) => DatabaseDriver::execute_sqlite(&mut **tx, sql, params).await,
        }
    }

    /// Make the transaction's writes permanent.
    pub async fn commit(self) -> Result<()> {
        match self {
            Self::Postgres(tx) => tx.commit().await?,
            Self::Sqlite(tx) => tx.commit().await?,
        }
        Ok(())
    }

    /// Undo the transaction's writes.
    pub async fn rollback(self) -> Result<()> {
        match self {
            Self::Postgres(tx) => tx.rollback().await?,
            Self::Sqlite(tx) => tx.rollback().await?,
        }
        Ok(())
    }

    /// See [`DatabaseDriver::host_sql`].
    fn host_sql(&self, sql: &str) -> String {
        match self {
            Self::Postgres(_) => number_placeholders(sql),
            Self::Sqlite(_) => sql.to_string(),
        }
    }
}

/// Where a bridge statement runs: on a pooled connection, or inside the
/// caller's open transaction.
enum QueryTarget<'a> {
    Pool(DatabaseDriver),
    Tx(&'a mut OpenTransaction),
}

impl<'a> QueryTarget<'a> {
    fn new(driver: &DatabaseDriver, tx: Option<&'a mut OpenTransaction>) -> QueryTarget<'a> {
        match tx {
            Some(tx) => QueryTarget::Tx(tx),
            None => QueryTarget::Pool(driver.clone()),
        }
    }

    async fn query(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<serde_json::Map<String, Value>>> {
        match self {
            Self::Pool(driver) => driver.query(sql, params).await,
            Self::Tx(tx) => tx.query(sql, params).await,
        }
    }

    async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<ExecuteResult> {
        match self {
            Self::Pool(driver) => driver.execute(sql, params).await,
            Self::Tx(tx) => tx.execute(sql, params).await,
        }
    }

    fn host_sql(&self, sql: &str) -> String {
        match self {
            Self::Pool(driver) => driver.host_sql(sql),
            Self::Tx(tx) => tx.host_sql(sql),
        }
    }
}

// ============================================================================
// DATABASE BRIDGE
// ============================================================================

/// A single migration definition registered via `_db_register_migration`
#[derive(Debug, Clone)]
pub struct MigrationEntry {
    pub name: String,
    pub up_sql: String,
    /// Stored for future rollback support; not used during `run_pending_migrations`
    #[allow(dead_code)]
    pub down_sql: String,
}

/// Database bridge providing database access capabilities
pub struct DbBridge {
    driver: Arc<RwLock<Option<DatabaseDriver>>>,
    config: Arc<RwLock<Option<DbConfig>>>,
    transactions: Arc<RwLock<HashMap<String, Transaction>>>,
    /// Pending migration definitions registered by WASM at startup via `_db_register_migration`
    pending_migrations: Arc<RwLock<Vec<MigrationEntry>>>,
}

/// Database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConfig {
    pub database_url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_min_connections")]
    pub min_connections: u32,
    #[serde(default = "default_connection_timeout")]
    pub connection_timeout: u64,
    #[serde(default = "default_query_timeout")]
    pub query_timeout: u64,
}

fn default_max_connections() -> u32 {
    10
}

fn default_min_connections() -> u32 {
    2
}

fn default_connection_timeout() -> u64 {
    10000 // 10 seconds
}

fn default_query_timeout() -> u64 {
    30000 // 30 seconds
}

/// Request parameters for host:db.query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbQueryRequest {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
}

/// Request parameters for host:db.execute
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbExecuteRequest {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
}

/// Request parameters for host:db.transaction_commit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbTransactionCommitRequest {
    pub tx_id: String,
}

/// Request parameters for host:db.transaction_rollback
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbTransactionRollbackRequest {
    pub tx_id: String,
}

/// Request parameters for host:db.query_in_tx
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbQueryInTxRequest {
    pub tx_id: String,
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
}

/// Request parameters for host:db.execute_in_tx
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbExecuteInTxRequest {
    pub tx_id: String,
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
}

/// Query result structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbQuery {
    pub sql: String,
    pub params: Vec<Value>,
}

/// Database result structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbResult {
    pub rows: Vec<serde_json::Map<String, Value>>,
    pub affected: usize,
}

/// Result from execute operations
#[derive(Debug)]
pub struct ExecuteResult {
    pub rows_affected: u64,
    pub last_insert_id: Option<i64>,
}

/// Transaction state
struct Transaction {
    #[allow(dead_code)]
    id: String,
    committed: bool,
    rolled_back: bool,
    operations: Vec<(String, Vec<Value>)>,
}

impl DbBridge {
    /// Create a new DbBridge without an active connection
    pub fn new() -> Self {
        Self {
            driver: Arc::new(RwLock::new(None)),
            config: Arc::new(RwLock::new(None)),
            transactions: Arc::new(RwLock::new(HashMap::new())),
            pending_migrations: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Configure the database connection
    pub async fn configure(&mut self, config: DbConfig) -> Result<()> {
        // Validate database URL
        if config.database_url.is_empty() {
            return Err(anyhow::anyhow!("Database URL cannot be empty"));
        }

        // Validate connection parameters
        if config.max_connections == 0 {
            return Err(anyhow::anyhow!("max_connections must be greater than 0"));
        }

        if config.min_connections > config.max_connections {
            return Err(anyhow::anyhow!(
                "min_connections cannot be greater than max_connections"
            ));
        }

        // Connect using the appropriate driver
        let driver = DatabaseDriver::connect(&config.database_url, &config).await?;

        // Store the driver and config
        *self.driver.write().await = Some(driver);
        *self.config.write().await = Some(config);

        Ok(())
    }

    /// Return the underlying SQLite pool when the configured driver is SQLite.
    ///
    /// Returns `None` when no database is configured, or when the driver is
    /// PostgreSQL.  Used by the jobs runtime to write-through job
    /// records to the same pool without introducing a second connection.
    pub async fn get_sqlite_pool(&self) -> Option<sqlx::SqlitePool> {
        let guard = self.driver.read().await;
        match guard.as_ref()? {
            DatabaseDriver::Sqlite(pool) => Some(pool.clone()),
            _ => None,
        }
    }

    /// Get the database driver
    async fn get_driver(&self) -> Result<DatabaseDriver> {
        let driver_guard = self.driver.read().await;

        if let Some(driver) = driver_guard.as_ref() {
            Ok(driver.clone())
        } else {
            Err(anyhow::anyhow!(
				"Database not configured. Call configure() first or set DATABASE_URL environment variable."
			))
        }
    }

    /// Main call dispatcher for the DB bridge
    pub async fn call(&mut self, function: &str, params: Value) -> Result<Value> {
        self.call_in(function, params, None).await
    }

    /// Open a transaction for `_db_begin`. The caller keeps the handle for
    /// the rest of its request and passes it to [`DbBridge::call_in`] so the
    /// statements it runs join the transaction.
    pub async fn begin_transaction(&self) -> Result<OpenTransaction> {
        self.get_driver().await?.begin().await
    }

    /// [`DbBridge::call`], with the statement functions (`query`, `execute`,
    /// `paginate`, `cursor_page`) running inside `tx` when it is given.
    pub async fn call_in(
        &mut self,
        function: &str,
        params: Value,
        tx: Option<&mut OpenTransaction>,
    ) -> Result<Value> {
        match function {
            "query" => self.query(params, tx).await,
            "execute" => self.execute(params, tx).await,
            "transaction_begin" => self.transaction_begin(params).await,
            "transaction_commit" => self.transaction_commit(params).await,
            "transaction_rollback" => self.transaction_rollback(params).await,
            "query_in_tx" => self.query_in_tx(params).await,
            "execute_in_tx" => self.execute_in_tx(params).await,
            "config" => self.config_call(params).await,
            "configure" => self.config_call(params).await,
            "register_migration" => self.register_migration(params).await,
            "paginate" => self.paginate(params, tx).await,
            "cursor_page" => self.cursor_page(params, tx).await,
            "migration_diff" => self.migration_diff(params).await,
            "migration_status" => self.migration_status().await,
            "rollback_migration" => self.rollback_migration(params).await,
            "run_migrations" => self.run_migrations_call(params).await,
            "valid_field" => self.valid_field(params).await,
            _ => Ok(json!({
                "ok": false,
                "err": {
                    "code": "DB_ERROR",
                    "message": format!("Unknown db function: {}", function),
                    "details": {}
                }
            })),
        }
    }

    /// Register a migration definition (called by `_db_register_migration` at WASM startup)
    async fn register_migration(&self, params: Value) -> Result<Value> {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": "register_migration requires 'name' field",
                        "details": {}
                    }
                }));
            }
        };
        let up_sql = params
            .get("up_sql")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let down_sql = params
            .get("down_sql")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut migrations = self.pending_migrations.write().await;
        if migrations.iter().any(|m| m.name == name) {
            return Ok(json!({ "ok": true, "data": { "status": "already_registered" } }));
        }
        migrations.push(MigrationEntry {
            name,
            up_sql,
            down_sql,
        });

        Ok(json!({ "ok": true, "data": null }))
    }

    /// Run all pending migrations that have not yet been applied to the database.
    ///
    /// Called once after WASM startup completes. Creates the `_clean_migrations`
    /// tracking table if it doesn't exist, then applies each registered migration
    /// in name-sorted order, skipping those already recorded as applied.
    pub async fn run_pending_migrations(&self) -> Result<()> {
        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(_) => {
                // No DB configured — nothing to run
                return Ok(());
            }
        };

        let migrations = {
            let guard = self.pending_migrations.read().await;
            let mut list = guard.clone();
            list.sort_by(|a, b| a.name.cmp(&b.name));
            list
        };

        if migrations.is_empty() {
            return Ok(());
        }

        // Ensure tracking table exists (syntax works for all three drivers)
        let create_tracking = "CREATE TABLE IF NOT EXISTS _clean_migrations \
			(name VARCHAR(255) PRIMARY KEY, applied_at VARCHAR(64) NOT NULL)";
        driver.execute(create_tracking, &[]).await?;

        for migration in &migrations {
            // Check if already applied
            let check_sql =
                driver.host_sql("SELECT COUNT(*) AS cnt FROM _clean_migrations WHERE name = ?");
            let rows = driver
                .query(&check_sql, &[Value::String(migration.name.clone())])
                .await;
            let already_applied = match rows {
                Ok(ref r) => {
                    r.first()
                        .and_then(|row| row.get("cnt"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0)
                        > 0
                }
                Err(_) => false,
            };

            if already_applied {
                continue;
            }

            info!("Applying migration: {}", migration.name);

            if !migration.up_sql.is_empty() {
                if let Err(e) = driver.execute(&migration.up_sql, &[]).await {
                    warn!("Migration '{}' failed: {}", migration.name, e);
                    return Err(anyhow::anyhow!(
                        "Migration '{}' failed: {}",
                        migration.name,
                        e
                    ));
                }
            }

            // Record as applied
            let now = chrono::Utc::now().to_rfc3339();
            let record_sql =
                driver.host_sql("INSERT INTO _clean_migrations (name, applied_at) VALUES (?, ?)");
            driver
                .execute(
                    &record_sql,
                    &[Value::String(migration.name.clone()), Value::String(now)],
                )
                .await?;

            info!("Migration '{}' applied successfully", migration.name);
        }

        Ok(())
    }

    /// Execute a SELECT query and return rows
    async fn query(&self, params: Value, tx: Option<&mut OpenTransaction>) -> Result<Value> {
        let req: DbQueryRequest = match serde_json::from_value(params) {
            Ok(req) => req,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": format!("Invalid request format: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        // Validate SQL
        let sql_upper = req.sql.trim().to_uppercase();
        if !sql_upper.starts_with("SELECT") && !sql_upper.starts_with("WITH") {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "VALIDATION_ERROR",
                    "message": "query() only accepts SELECT or WITH queries. Use execute() for INSERT/UPDATE/DELETE.",
                    "details": {}
                }
            }));
        }

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "CONNECTION_ERROR",
                        "message": format!("Failed to get database connection: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        let timeout = {
            let config_guard = self.config.read().await;
            config_guard
                .as_ref()
                .map(|c| c.query_timeout)
                .unwrap_or(30000)
        };

        let mut target = QueryTarget::new(&driver, tx);
        let result = tokio::time::timeout(
            Duration::from_millis(timeout),
            target.query(&req.sql, &req.params),
        )
        .await;

        match result {
            Ok(Ok(rows)) => Ok(json!({
                "ok": true,
                "data": {
                    "rows": rows,
                    "count": rows.len()
                }
            })),
            Ok(Err(e)) => {
                let (code, message) = self.categorize_error(&format!("{}", e));
                Ok(json!({
                    "ok": false,
                    "err": {
                        "code": code,
                        "message": message,
                        "details": {}
                    }
                }))
            }
            Err(_) => Ok(json!({
                "ok": false,
                "err": {
                    "code": "TIMEOUT",
                    "message": format!("Query timeout exceeded ({} ms)", timeout),
                    "details": {}
                }
            })),
        }
    }

    /// Execute an INSERT/UPDATE/DELETE query
    async fn execute(&self, params: Value, tx: Option<&mut OpenTransaction>) -> Result<Value> {
        let req: DbExecuteRequest = match serde_json::from_value(params) {
            Ok(req) => req,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": format!("Invalid request format: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        // Validate SQL
        let is_valid = is_write_statement(&req.sql);

        if !is_valid {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "VALIDATION_ERROR",
                    "message": "execute() only accepts INSERT/UPDATE/DELETE/CREATE/DROP/ALTER queries. Use query() for SELECT.",
                    "details": {}
                }
            }));
        }

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "CONNECTION_ERROR",
                        "message": format!("Failed to get database connection: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        let timeout = {
            let config_guard = self.config.read().await;
            config_guard
                .as_ref()
                .map(|c| c.query_timeout)
                .unwrap_or(30000)
        };

        let mut target = QueryTarget::new(&driver, tx);
        let result = tokio::time::timeout(
            Duration::from_millis(timeout),
            target.execute(&req.sql, &req.params),
        )
        .await;

        match result {
            Ok(Ok(exec_result)) => Ok(json!({
                "ok": true,
                "data": {
                    "affected_rows": exec_result.rows_affected,
                    "last_insert_id": exec_result.last_insert_id
                }
            })),
            Ok(Err(e)) => {
                let (code, message) = self.categorize_error(&format!("{}", e));
                Ok(json!({
                    "ok": false,
                    "err": {
                        "code": code,
                        "message": message,
                        "details": {}
                    }
                }))
            }
            Err(_) => Ok(json!({
                "ok": false,
                "err": {
                    "code": "TIMEOUT",
                    "message": format!("Query timeout exceeded ({} ms)", timeout),
                    "details": {}
                }
            })),
        }
    }

    /// Begin a new transaction
    async fn transaction_begin(&self, _params: Value) -> Result<Value> {
        let tx_id = format!("tx_{}", Uuid::new_v4().to_string().replace("-", ""));

        let transaction = Transaction {
            id: tx_id.clone(),
            committed: false,
            rolled_back: false,
            operations: Vec::new(),
        };

        let mut transactions = self.transactions.write().await;
        transactions.insert(tx_id.clone(), transaction);

        Ok(json!({
            "ok": true,
            "data": {
                "tx_id": tx_id
            }
        }))
    }

    /// Commit a transaction
    async fn transaction_commit(&self, params: Value) -> Result<Value> {
        let req: DbTransactionCommitRequest = match serde_json::from_value(params) {
            Ok(req) => req,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": format!("Invalid request format: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        let mut transactions = self.transactions.write().await;
        let transaction = match transactions.get_mut(&req.tx_id) {
            Some(tx) => tx,
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "TRANSACTION_ERROR",
                        "message": format!("Transaction not found: {}", req.tx_id),
                        "details": {}
                    }
                }));
            }
        };

        if transaction.committed {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "TRANSACTION_ERROR",
                    "message": "Transaction already committed",
                    "details": {}
                }
            }));
        }

        if transaction.rolled_back {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "TRANSACTION_ERROR",
                    "message": "Transaction already rolled back",
                    "details": {}
                }
            }));
        }

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "CONNECTION_ERROR",
                        "message": format!("Failed to get database connection: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        let operations = transaction.operations.clone();
        drop(transactions);

        // Execute all operations in a real transaction
        if let Err(e) = driver.execute_transaction(&operations).await {
            let (code, message) = self.categorize_error(&format!("{}", e));
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": code,
                    "message": message,
                    "details": {}
                }
            }));
        }

        // Mark as committed and remove from tracking
        let mut transactions = self.transactions.write().await;
        if let Some(tx) = transactions.get_mut(&req.tx_id) {
            tx.committed = true;
        }
        transactions.remove(&req.tx_id);

        Ok(json!({
            "ok": true,
            "data": null
        }))
    }

    /// Rollback a transaction
    async fn transaction_rollback(&self, params: Value) -> Result<Value> {
        let req: DbTransactionRollbackRequest = match serde_json::from_value(params) {
            Ok(req) => req,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": format!("Invalid request format: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        let mut transactions = self.transactions.write().await;
        let transaction = match transactions.get_mut(&req.tx_id) {
            Some(tx) => tx,
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "TRANSACTION_ERROR",
                        "message": format!("Transaction not found: {}", req.tx_id),
                        "details": {}
                    }
                }));
            }
        };

        if transaction.committed {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "TRANSACTION_ERROR",
                    "message": "Transaction already committed, cannot rollback",
                    "details": {}
                }
            }));
        }

        if transaction.rolled_back {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "TRANSACTION_ERROR",
                    "message": "Transaction already rolled back",
                    "details": {}
                }
            }));
        }

        transaction.rolled_back = true;
        transactions.remove(&req.tx_id);

        Ok(json!({
            "ok": true,
            "data": null
        }))
    }

    /// Execute a query within a transaction
    async fn query_in_tx(&self, params: Value) -> Result<Value> {
        let req: DbQueryInTxRequest = match serde_json::from_value(params) {
            Ok(req) => req,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": format!("Invalid request format: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        let sql_upper = req.sql.trim().to_uppercase();
        if !sql_upper.starts_with("SELECT") && !sql_upper.starts_with("WITH") {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "VALIDATION_ERROR",
                    "message": "query_in_tx() only accepts SELECT or WITH queries. Use execute_in_tx() for INSERT/UPDATE/DELETE.",
                    "details": {}
                }
            }));
        }

        let transactions = self.transactions.read().await;
        let transaction = match transactions.get(&req.tx_id) {
            Some(tx) => tx,
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "TRANSACTION_ERROR",
                        "message": format!("Transaction not found: {}", req.tx_id),
                        "details": {}
                    }
                }));
            }
        };

        if transaction.committed {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "TRANSACTION_ERROR",
                    "message": "Transaction already committed",
                    "details": {}
                }
            }));
        }

        if transaction.rolled_back {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "TRANSACTION_ERROR",
                    "message": "Transaction already rolled back",
                    "details": {}
                }
            }));
        }
        drop(transactions);

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "CONNECTION_ERROR",
                        "message": format!("Failed to get database connection: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        match driver.query(&req.sql, &req.params).await {
            Ok(rows) => Ok(json!({
                "ok": true,
                "data": {
                    "rows": rows,
                    "count": rows.len()
                }
            })),
            Err(e) => {
                let (code, message) = self.categorize_error(&format!("{}", e));
                Ok(json!({
                    "ok": false,
                    "err": {
                        "code": code,
                        "message": message,
                        "details": {}
                    }
                }))
            }
        }
    }

    /// Execute a command within a transaction
    async fn execute_in_tx(&self, params: Value) -> Result<Value> {
        let req: DbExecuteInTxRequest = match serde_json::from_value(params) {
            Ok(req) => req,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": format!("Invalid request format: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        let is_valid = is_write_statement(&req.sql);

        if !is_valid {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "VALIDATION_ERROR",
                    "message": "execute_in_tx() only accepts INSERT/UPDATE/DELETE/CREATE/DROP/ALTER queries.",
                    "details": {}
                }
            }));
        }

        let mut transactions = self.transactions.write().await;
        let transaction = match transactions.get_mut(&req.tx_id) {
            Some(tx) => tx,
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "TRANSACTION_ERROR",
                        "message": format!("Transaction not found: {}", req.tx_id),
                        "details": {}
                    }
                }));
            }
        };

        if transaction.committed {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "TRANSACTION_ERROR",
                    "message": "Transaction already committed",
                    "details": {}
                }
            }));
        }

        if transaction.rolled_back {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "TRANSACTION_ERROR",
                    "message": "Transaction already rolled back",
                    "details": {}
                }
            }));
        }

        transaction
            .operations
            .push((req.sql.clone(), req.params.clone()));

        Ok(json!({
            "ok": true,
            "data": {
                "affected_rows": 0,
                "last_insert_id": null
            }
        }))
    }

    /// Configure database connection
    async fn config_call(&mut self, params: Value) -> Result<Value> {
        let config: DbConfig = match serde_json::from_value(params) {
            Ok(cfg) => cfg,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": format!("Invalid configuration format: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        match self.configure(config).await {
            Ok(_) => Ok(json!({
                "ok": true,
                "data": null
            })),
            Err(e) => Ok(json!({
                "ok": false,
                "err": {
                    "code": "CONNECTION_ERROR",
                    "message": format!("Failed to configure database: {}", e),
                    "details": {}
                }
            })),
        }
    }

    // ============================================================================
    // NEW BRIDGE METHODS — frame.data plugin bridge functions
    // ============================================================================

    /// Configure the connection pool from a JSON config string.
    ///
    /// The JSON must contain at minimum a `database_url` field.  Optional fields:
    /// `max_connections`, `min_connections`, `connection_timeout`, `query_timeout`.
    ///
    /// Returns `{"ok": true, "data": null}` on success.
    /// The WASM bridge function (`_db_configure`) converts this to `0` / `-1`.
    pub async fn configure_from_json(&mut self, config_json: &str) -> Result<Value> {
        let config: DbConfig = match serde_json::from_str(config_json) {
            Ok(cfg) => cfg,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "VALIDATION_ERROR",
                        "message": format!("Invalid JSON config: {}", e),
                        "details": {}
                    }
                }));
            }
        };

        self.config_call(serde_json::to_value(config)?).await
    }

    /// Offset-based paginated query.
    ///
    /// Expected `params` keys:
    /// - `table` (string, required) — table name
    /// - `where` (object, optional) — field => value equality filters
    /// - `page` (integer, default 1, 1-based)
    /// - `per_page` (integer, default 20)
    ///
    /// Returns PagedResult JSON envelope.
    async fn paginate(&self, params: Value, tx: Option<&mut OpenTransaction>) -> Result<Value> {
        let table = match params.get("table").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "VALIDATION_ERROR", "message": "paginate requires table", "details": {} }
                }));
            }
        };
        let where_json = params.get("where").cloned().unwrap_or(json!({}));
        let page = params
            .get("page")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .max(1);
        let per_page = params
            .get("per_page")
            .and_then(|v| v.as_i64())
            .unwrap_or(20)
            .max(1);

        if !is_safe_identifier(&table) {
            return Ok(json!({
                "ok": false,
                "err": { "code": "VALIDATION_ERROR", "message": "Invalid table name", "details": {} }
            }));
        }

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "CONNECTION_ERROR", "message": format!("{}", e), "details": {} }
                }));
            }
        };

        let timeout = {
            let cfg = self.config.read().await;
            cfg.as_ref().map(|c| c.query_timeout).unwrap_or(30000)
        };

        let (where_clause, bind_params) = build_where_clause(&where_json);
        let order_by = match order_by_clause(&where_json) {
            Ok(o) => o,
            Err(msg) => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "VALIDATION_ERROR", "message": msg, "details": {} }
                }));
            }
        };

        let count_sql = if where_clause.is_empty() {
            format!("SELECT COUNT(*) AS __total FROM {}", table)
        } else {
            format!(
                "SELECT COUNT(*) AS __total FROM {} WHERE {}",
                table, where_clause
            )
        };

        let mut target = QueryTarget::new(&driver, tx);
        let count_sql = target.host_sql(&count_sql);
        let count_result = tokio::time::timeout(
            Duration::from_millis(timeout),
            target.query(&count_sql, &bind_params),
        )
        .await;

        let total: i64 = match count_result {
            Ok(Ok(ref rows)) => rows
                .first()
                .and_then(|r| r.get("__total"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            Ok(Err(e)) => {
                let (code, msg) = self.categorize_error(&e.to_string());
                return Ok(json!({
                    "ok": false,
                    "err": { "code": code, "message": msg, "details": {} }
                }));
            }
            Err(_) => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "TIMEOUT", "message": "Count query timed out", "details": {} }
                }));
            }
        };

        let offset = (page - 1) * per_page;
        let total_pages = if per_page > 0 {
            (total + per_page - 1) / per_page
        } else {
            0
        };

        let where_sql = if where_clause.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", where_clause)
        };
        let order_sql = order_by
            .map(|o| format!(" ORDER BY {}", o))
            .unwrap_or_default();
        let data_sql = target.host_sql(&format!(
            "SELECT * FROM {}{}{} LIMIT {} OFFSET {}",
            table, where_sql, order_sql, per_page, offset
        ));

        let data_result = tokio::time::timeout(
            Duration::from_millis(timeout),
            target.query(&data_sql, &bind_params),
        )
        .await;

        match data_result {
            Ok(Ok(rows)) => Ok(json!({
                "ok": true,
                "data": {
                    "data": rows,
                    "total": total,
                    "page": page,
                    "per_page": per_page,
                    "total_pages": total_pages
                }
            })),
            Ok(Err(e)) => {
                let (code, msg) = self.categorize_error(&e.to_string());
                Ok(json!({
                    "ok": false,
                    "err": { "code": code, "message": msg, "details": {} }
                }))
            }
            Err(_) => Ok(json!({
                "ok": false,
                "err": { "code": "TIMEOUT", "message": "Page query timed out", "details": {} }
            })),
        }
    }

    /// Cursor-based paginated query.
    ///
    /// Expected `params` keys:
    /// - `table` (string, required)
    /// - `where` (object, optional)
    /// - `per_page` (integer, default 20)
    /// - `after` (string, optional) — opaque cursor (last seen `by_field` value)
    /// - `by_field` (string, default "id")
    ///
    /// Returns CursorResult JSON envelope.
    async fn cursor_page(&self, params: Value, tx: Option<&mut OpenTransaction>) -> Result<Value> {
        let table = match params.get("table").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "VALIDATION_ERROR", "message": "cursor_page requires table", "details": {} }
                }));
            }
        };
        let where_json = params.get("where").cloned().unwrap_or(json!({}));
        let per_page = params
            .get("per_page")
            .and_then(|v| v.as_i64())
            .unwrap_or(20)
            .max(1);
        let after = params
            .get("after")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let by_field = params
            .get("by_field")
            .and_then(|v| v.as_str())
            .unwrap_or("id")
            .to_string();

        if !is_safe_identifier(&table) {
            return Ok(json!({
                "ok": false,
                "err": { "code": "VALIDATION_ERROR", "message": "Invalid table name", "details": {} }
            }));
        }
        if !is_safe_identifier(&by_field) {
            return Ok(json!({
                "ok": false,
                "err": { "code": "VALIDATION_ERROR", "message": "Invalid cursor field name", "details": {} }
            }));
        }

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "CONNECTION_ERROR", "message": format!("{}", e), "details": {} }
                }));
            }
        };

        let timeout = {
            let cfg = self.config.read().await;
            cfg.as_ref().map(|c| c.query_timeout).unwrap_or(30000)
        };

        let (where_clause, mut bind_params) = build_where_clause(&where_json);

        let full_where = if after.is_empty() {
            where_clause.clone()
        } else {
            let numeric = cursor_column_is_numeric(&driver, &table, &by_field).await;
            bind_params.push(cursor_bind_value(&after, numeric));
            let cursor_cond = format!("{} > ?", by_field);
            if where_clause.is_empty() {
                cursor_cond
            } else {
                format!("{} AND {}", where_clause, cursor_cond)
            }
        };

        let sql = if full_where.is_empty() {
            format!(
                "SELECT * FROM {} ORDER BY {} ASC LIMIT {}",
                table,
                by_field,
                per_page + 1
            )
        } else {
            format!(
                "SELECT * FROM {} WHERE {} ORDER BY {} ASC LIMIT {}",
                table,
                full_where,
                by_field,
                per_page + 1
            )
        };

        let mut target = QueryTarget::new(&driver, tx);
        let sql = target.host_sql(&sql);
        let result = tokio::time::timeout(
            Duration::from_millis(timeout),
            target.query(&sql, &bind_params),
        )
        .await;

        match result {
            Ok(Ok(mut rows)) => {
                let has_more = rows.len() as i64 > per_page;
                if has_more {
                    rows.truncate(per_page as usize);
                }
                let next_cursor: Value = if has_more {
                    rows.last()
                        .and_then(|r| r.get(&by_field))
                        .cloned()
                        .unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                Ok(json!({
                    "ok": true,
                    "data": {
                        "data": rows,
                        "next_cursor": next_cursor,
                        "has_more": has_more
                    }
                }))
            }
            Ok(Err(e)) => {
                let (code, msg) = self.categorize_error(&e.to_string());
                Ok(json!({
                    "ok": false,
                    "err": { "code": code, "message": msg, "details": {} }
                }))
            }
            Err(_) => Ok(json!({
                "ok": false,
                "err": { "code": "TIMEOUT", "message": "Cursor page query timed out", "details": {} }
            })),
        }
    }

    /// Compare declared model fields against the live database schema and return
    /// the ALTER TABLE SQL needed to reconcile the difference.
    ///
    /// Expected `params` keys:
    /// - `table` (string, optional) — when present, the live schema is introspected from the DB
    /// - `declared` (object) — {`column`: "SQL-type-fragment", ...}
    /// - `live` (object, optional) — only used when `table` is absent
    ///
    /// Returns `{"ok": true, "data": {"sql": "ALTER TABLE..."}`; `sql` is empty when in sync.
    async fn migration_diff(&self, params: Value) -> Result<Value> {
        let table_opt = params
            .get("table")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let declared = params.get("declared").cloned().unwrap_or(json!({}));

        let live_json = if let Some(ref table) = table_opt {
            if !is_safe_identifier(table) {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "VALIDATION_ERROR", "message": "Invalid table name", "details": {} }
                }));
            }
            match self.get_driver().await {
                Ok(driver) => {
                    let cols = introspect_table_columns(&driver, table).await;
                    columns_to_json(&cols)
                }
                Err(_) => json!({}),
            }
        } else {
            params.get("live").cloned().unwrap_or(json!({}))
        };

        let sql = compute_migration_diff(table_opt.as_deref(), &declared, &live_json);
        Ok(json!({ "ok": true, "data": { "sql": sql } }))
    }

    /// Return the current status of all registered migrations.
    ///
    /// Returns `{"ok": true, "data": {"migrations": [{"name": "...", "applied_at": "..."|null}, ...]}}`.
    async fn migration_status(&self) -> Result<Value> {
        let pending = {
            let guard = self.pending_migrations.read().await;
            guard.clone()
        };

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(_) => {
                let rows: Vec<Value> = pending
                    .iter()
                    .map(|m| json!({ "name": m.name, "applied_at": Value::Null }))
                    .collect();
                return Ok(json!({ "ok": true, "data": { "migrations": rows } }));
            }
        };

        let _ = driver
			.execute(
				"CREATE TABLE IF NOT EXISTS _clean_migrations 				 (name VARCHAR(255) PRIMARY KEY, applied_at VARCHAR(64) NOT NULL)",
				&[],
			)
			.await;

        let applied_rows = driver
            .query(
                "SELECT name, applied_at FROM _clean_migrations ORDER BY name ASC",
                &[],
            )
            .await
            .unwrap_or_default();

        let mut applied_map: HashMap<String, String> = HashMap::new();
        for row in &applied_rows {
            if let (Some(name), Some(at)) = (
                row.get("name").and_then(|v| v.as_str()),
                row.get("applied_at").and_then(|v| v.as_str()),
            ) {
                applied_map.insert(name.to_string(), at.to_string());
            }
        }

        let rows: Vec<Value> = pending
            .iter()
            .map(|m| {
                let applied_at = applied_map
                    .get(&m.name)
                    .map(|s| Value::String(s.clone()))
                    .unwrap_or(Value::Null);
                json!({ "name": m.name, "applied_at": applied_at })
            })
            .collect();

        Ok(json!({ "ok": true, "data": { "migrations": rows } }))
    }

    /// Rollback a specific named migration by executing its `down_sql`.
    ///
    /// Returns `{"ok": true, "data": null}` on success or `{"ok": false, "err": {...}}`.
    /// The WASM bridge converts this to `0` / `-1`.
    async fn rollback_migration(&self, params: Value) -> Result<Value> {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "VALIDATION_ERROR", "message": "rollback_migration requires name", "details": {} }
                }));
            }
        };

        let entry = {
            let guard = self.pending_migrations.read().await;
            guard.iter().find(|m| m.name == name).cloned()
        };

        let entry = match entry {
            Some(e) => e,
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": {
                        "code": "NOT_FOUND",
                        "message": format!("Migration {} is not registered", name),
                        "details": {}
                    }
                }));
            }
        };

        if entry.down_sql.is_empty() {
            return Ok(json!({
                "ok": false,
                "err": {
                    "code": "VALIDATION_ERROR",
                    "message": format!("Migration {} has no down_sql defined", name),
                    "details": {}
                }
            }));
        }

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "CONNECTION_ERROR", "message": format!("{}", e), "details": {} }
                }));
            }
        };

        if let Err(e) = driver.execute(&entry.down_sql, &[]).await {
            let (code, msg) = self.categorize_error(&e.to_string());
            return Ok(json!({
                "ok": false,
                "err": { "code": code, "message": msg, "details": {} }
            }));
        }

        let _ = driver
            .execute(
                &driver.host_sql("DELETE FROM _clean_migrations WHERE name = ?"),
                &[Value::String(name)],
            )
            .await;

        Ok(json!({ "ok": true, "data": null }))
    }

    /// Apply all pending registered migrations and return the count newly applied.
    ///
    /// Returns `{"ok": true, "data": {"applied": N}}`.
    async fn run_migrations_call(&self, _params: Value) -> Result<Value> {
        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "CONNECTION_ERROR", "message": format!("{}", e), "details": {} }
                }));
            }
        };

        let _ = driver
			.execute(
				"CREATE TABLE IF NOT EXISTS _clean_migrations 				 (name VARCHAR(255) PRIMARY KEY, applied_at VARCHAR(64) NOT NULL)",
				&[],
			)
			.await;

        let count_before = driver
            .query("SELECT COUNT(*) AS cnt FROM _clean_migrations", &[])
            .await
            .ok()
            .and_then(|rows| {
                rows.first()
                    .and_then(|r| r.get("cnt"))
                    .and_then(|v| v.as_i64())
            })
            .unwrap_or(0);

        match self.run_pending_migrations().await {
            Ok(()) => {
                let count_after = driver
                    .query("SELECT COUNT(*) AS cnt FROM _clean_migrations", &[])
                    .await
                    .ok()
                    .and_then(|rows| {
                        rows.first()
                            .and_then(|r| r.get("cnt"))
                            .and_then(|v| v.as_i64())
                    })
                    .unwrap_or(count_before);

                Ok(json!({
                    "ok": true,
                    "data": { "applied": (count_after - count_before).max(0) }
                }))
            }
            Err(e) => {
                let (code, msg) = self.categorize_error(&e.to_string());
                Ok(json!({
                    "ok": false,
                    "err": { "code": code, "message": msg, "details": {} }
                }))
            }
        }
    }

    /// Runtime ORDER BY safety check: returns true when `candidate` is a real
    /// column name in `table`.
    ///
    /// Returns `{"ok": true, "data": {"valid": bool}}`.
    async fn valid_field(&self, params: Value) -> Result<Value> {
        let table = match params.get("table").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "VALIDATION_ERROR", "message": "valid_field requires table", "details": {} }
                }));
            }
        };
        let candidate = match params.get("field").and_then(|v| v.as_str()) {
            Some(f) => f.to_string(),
            None => {
                return Ok(json!({
                    "ok": false,
                    "err": { "code": "VALIDATION_ERROR", "message": "valid_field requires field", "details": {} }
                }));
            }
        };

        if !is_safe_identifier(&table) || !is_safe_identifier(&candidate) {
            return Ok(json!({ "ok": true, "data": { "valid": false } }));
        }

        let driver = match self.get_driver().await {
            Ok(d) => d,
            Err(_) => return Ok(json!({ "ok": true, "data": { "valid": false } })),
        };

        let columns = introspect_table_columns(&driver, &table).await;
        let valid = columns.iter().any(|c| c.eq_ignore_ascii_case(&candidate));
        Ok(json!({ "ok": true, "data": { "valid": valid } }))
    }

    /// Categorize database error and return appropriate code and message
    fn categorize_error(&self, error: &str) -> (&'static str, String) {
        let error_lower = error.to_lowercase();

        if error_lower.contains("unique") || error_lower.contains("duplicate") {
            ("VALIDATION_ERROR", self.sanitize_error(error))
        } else if error_lower.contains("foreign key")
            || error_lower.contains("constraint")
            || error_lower.contains("check constraint")
        {
            ("VALIDATION_ERROR", self.sanitize_error(error))
        } else if error_lower.contains("syntax error") || error_lower.contains("parse error") {
            ("QUERY_ERROR", self.sanitize_error(error))
        } else if error_lower.contains("connection")
            || error_lower.contains("timeout")
            || error_lower.contains("network")
        {
            ("CONNECTION_ERROR", self.sanitize_error(error))
        } else if error_lower.contains("permission") || error_lower.contains("access denied") {
            ("PERMISSION_DENIED", self.sanitize_error(error))
        } else if error_lower.contains("not found") || error_lower.contains("does not exist") {
            ("NOT_FOUND", self.sanitize_error(error))
        } else {
            ("DB_ERROR", self.sanitize_error(error))
        }
    }

    /// Sanitize error message
    fn sanitize_error(&self, error: &str) -> String {
        let sanitized = error
            .lines()
            .next()
            .unwrap_or(error)
            .chars()
            .take(200)
            .collect::<String>();

        if sanitized.len() < error.len() {
            format!("{}...", sanitized)
        } else {
            sanitized
        }
    }
}

impl Default for DbBridge {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// HELPER FUNCTIONS — used by the new frame.data bridge methods
// ============================================================================

/// Returns true when `s` contains only ASCII alphanumeric characters and underscores.
/// Used to validate table/column names before interpolating them into SQL.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Rewrite `?` placeholders as PostgreSQL's numbered `$1, $2, ...`, leaving
/// `?` inside single-quoted literals and double-quoted identifiers alone.
fn number_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0;
    let mut quote: Option<char> = None;
    for c in sql.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                out.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    out.push(c);
                }
                '?' => {
                    n += 1;
                    out.push('$');
                    out.push_str(&n.to_string());
                }
                _ => out.push(c),
            },
        }
    }
    out
}

/// Validate the `__order` key of a paginate where-object — an ORDER BY list
/// such as `"created_at DESC, id ASC"` — and return it normalised. Each term
/// is a column (optionally `table.column`) with an optional ASC/DESC.
/// Returns `Ok(None)` when no order was requested.
fn order_by_clause(where_json: &Value) -> std::result::Result<Option<String>, String> {
    let raw = match where_json.get("__order").and_then(|v| v.as_str()) {
        Some(o) if !o.trim().is_empty() => o,
        _ => return Ok(None),
    };
    let mut terms = Vec::new();
    for term in raw.split(',') {
        let mut words = term.split_whitespace();
        let column = words
            .next()
            .ok_or_else(|| format!("Invalid order clause: {:?}", raw))?;
        if !column.split('.').all(is_safe_identifier) {
            return Err(format!("Invalid order column: {:?}", column));
        }
        let direction = match words.next().map(|d| d.to_ascii_uppercase()) {
            None => None,
            Some(d) if d == "ASC" || d == "DESC" => Some(d),
            Some(d) => return Err(format!("Invalid order direction: {:?}", d)),
        };
        if words.next().is_some() {
            return Err(format!("Invalid order clause: {:?}", raw));
        }
        terms.push(match direction {
            Some(d) => format!("{} {}", column, d),
            None => column.to_string(),
        });
    }
    Ok(Some(terms.join(", ")))
}

/// The bind value for a cursor `after` value. The cursor travels through
/// Clean code as text, but must be compared with the column's own type —
/// PostgreSQL rejects `integer > text`. Numeric columns get a number;
/// everything else stays text. `numeric` is `None` when the column type is
/// unknown, in which case an integer-looking cursor is bound as an integer.
fn cursor_bind_value(after: &str, numeric: Option<bool>) -> Value {
    let trimmed = after.trim();
    let as_number = || {
        trimmed.parse::<i64>().map(Value::from).ok().or_else(|| {
            trimmed
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .map(Value::from)
        })
    };
    match numeric {
        Some(true) => as_number().unwrap_or_else(|| Value::String(after.to_string())),
        Some(false) => Value::String(after.to_string()),
        None => trimmed
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(after.to_string())),
    }
}

/// Whether the cursor column has a numeric type. Known exactly on PostgreSQL
/// (information_schema); SQLite coerces comparisons itself, so there the
/// answer is `None` (unknown).
async fn cursor_column_is_numeric(
    driver: &DatabaseDriver,
    table: &str,
    column: &str,
) -> Option<bool> {
    match driver {
        DatabaseDriver::Postgres(_) => {
            let sql = "SELECT data_type::text AS data_type FROM information_schema.columns \
                       WHERE table_name = $1 AND column_name = $2 AND table_schema = current_schema()";
            let rows = driver
                .query(
                    sql,
                    &[
                        Value::String(table.to_string()),
                        Value::String(column.to_string()),
                    ],
                )
                .await
                .ok()?;
            let data_type = rows
                .first()?
                .get("data_type")?
                .as_str()?
                .to_ascii_lowercase();
            Some(matches!(
                data_type.as_str(),
                "smallint"
                    | "integer"
                    | "bigint"
                    | "numeric"
                    | "decimal"
                    | "real"
                    | "double precision"
            ))
        }
        DatabaseDriver::Sqlite(_) => None,
    }
}

/// Build a parameterised WHERE clause from a JSON object of equality filters.
///
/// The returned tuple contains:
/// - A SQL fragment such as `col1 = ? AND col2 = ?` (empty string when `filters` is empty)
/// - A `Vec<Value>` of the corresponding bind parameters
///
/// Only object-level keys that pass `is_safe_identifier` are included; others are silently
/// skipped to prevent SQL injection through attacker-controlled key names.
///
/// Reserved protocol keys (any key starting with `__`) are handled separately and never
/// treated as column equality filters. Currently recognised:
/// - `__where` (string): appended verbatim as a raw SQL fragment without parameter binding.
///   Used by the framework's `frame.data` plugin to convey operators (`!= null`, `> x`, …)
///   that cannot be expressed as plain equality. The fragment is composed by the framework
///   from typed AST nodes, not user input, so it is trusted.
/// - `__order` (string): silently skipped — order resolution is the caller's responsibility
///   (e.g. cursor_page uses its own `by_field`).
///
/// All other `__*` keys are skipped without effect, reserving the `__` prefix as a
/// framework/bridge protocol namespace.
fn build_where_clause(filters: &Value) -> (String, Vec<Value>) {
    let obj = match filters.as_object() {
        Some(o) => o,
        None => return (String::new(), Vec::new()),
    };

    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    for (key, val) in obj {
        if let Some(rest) = key.strip_prefix("__") {
            if rest == "where" {
                if let Some(fragment) = val.as_str() {
                    if !fragment.is_empty() {
                        clauses.push(fragment.to_string());
                    }
                }
            }
            continue;
        }
        if !is_safe_identifier(key) {
            continue;
        }
        clauses.push(format!("{} = ?", key));
        params.push(val.clone());
    }

    (clauses.join(" AND "), params)
}

/// Introspect the column names of an existing table using database-specific
/// INFORMATION_SCHEMA or PRAGMA queries.
///
/// Returns an empty vector when the table does not exist or introspection fails.
async fn introspect_table_columns(driver: &DatabaseDriver, table: &str) -> Vec<String> {
    match driver {
        DatabaseDriver::Sqlite(_) => {
            let sql = format!("PRAGMA table_info({})", table);
            match driver.query(&sql, &[]).await {
                Ok(rows) => rows
                    .iter()
                    .filter_map(|r| {
                        r.get("name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
        DatabaseDriver::Postgres(_) => {
            let sql = "SELECT column_name::text AS column_name FROM information_schema.columns WHERE table_name = $1 AND table_schema = current_schema() ORDER BY ordinal_position";
            match driver.query(sql, &[Value::String(table.to_string())]).await {
                Ok(rows) => rows
                    .iter()
                    .filter_map(|r| {
                        r.get("column_name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
    }
}

/// Convert a list of column names into a JSON object mapping each name to an empty type string.
/// Used as the "live" schema representation for `compute_migration_diff`.
fn columns_to_json(columns: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    for col in columns {
        map.insert(col.clone(), Value::String(String::new()));
    }
    Value::Object(map)
}

/// Compute the ALTER TABLE SQL needed to bring a live table in sync with a declared schema.
///
/// - `table` — table name to use in the ALTER TABLE statement (None → generic output)
/// - `declared` — JSON object: { column_name: "type-string", ... }
/// - `live` — JSON object with the same shape representing the current live columns
///
/// Returns an empty string when declared and live match exactly (same column names, case-insensitive).
/// Currently only generates ADD COLUMN statements for columns present in `declared` but absent
/// from `live`.  DROP COLUMN statements are not generated because removing columns is
/// destructive and should require explicit developer action.
fn compute_migration_diff(table: Option<&str>, declared: &Value, live: &Value) -> String {
    let decl_obj = match declared.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let live_obj = match live.as_object() {
        Some(o) => o,
        None => return String::new(),
    };

    let live_keys: std::collections::HashSet<String> =
        live_obj.keys().map(|k| k.to_lowercase()).collect();

    let mut add_clauses: Vec<String> = Vec::new();

    for (col, type_val) in decl_obj {
        if live_keys.contains(&col.to_lowercase()) {
            continue;
        }
        let type_str = type_val.as_str().unwrap_or("TEXT");
        if !is_safe_identifier(col) {
            continue;
        }
        add_clauses.push(format!("ADD COLUMN {} {}", col, type_str));
    }

    if add_clauses.is_empty() {
        return String::new();
    }

    let tbl = table.unwrap_or("<table>");
    format!("ALTER TABLE {} {};", tbl, add_clauses.join(", "))
}

/// Statements `execute()` runs: the ones that change data or schema. A
/// statement that starts with a `WITH` clause is judged by the statement
/// after it, so `WITH moved AS (UPDATE ...) INSERT INTO ...` is a write
/// while `WITH t AS (...) SELECT ...` still belongs to `query()`.
fn is_write_statement(sql: &str) -> bool {
    const WRITES: [&str; 7] = [
        "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "TRUNCATE",
    ];
    let upper = sql.trim().to_uppercase();
    let main = if starts_with_keyword(&upper, "WITH") {
        statement_after_with(&upper)
    } else {
        upper.as_str()
    };
    WRITES.iter().any(|w| starts_with_keyword(main, w))
}

fn starts_with_keyword(s: &str, keyword: &str) -> bool {
    s.starts_with(keyword)
        && !s[keyword.len()..]
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric() || c == '_')
            .unwrap_or(false)
}

/// The text after a leading `WITH` clause: every common table expression is
/// `name [(columns)] AS [NOT] [MATERIALIZED] ( ... )`, separated by commas.
/// The main statement starts at the first word outside parentheses that
/// follows a closed CTE body and is not a comma. Quoted text is skipped.
fn statement_after_with(upper: &str) -> &str {
    let bytes = upper.as_bytes();
    let mut depth = 0usize;
    let mut closed_body = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != c {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    closed_body = true;
                }
            }
            b',' if depth == 0 => closed_body = false,
            c if depth == 0 && closed_body && c.is_ascii_alphabetic() => {
                let rest = &upper[i..];
                // A column list `name (a, b) AS (...)` closes a paren too;
                // the CTE body only ends at the parenthesis after AS.
                if starts_with_keyword(rest, "AS") {
                    closed_body = false;
                } else {
                    return rest;
                }
            }
            _ => {}
        }
        i += 1;
    }
    ""
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

    static TEST_DB_MUTEX: once_cell::sync::Lazy<Arc<AsyncMutex<()>>> =
        once_cell::sync::Lazy::new(|| Arc::new(AsyncMutex::new(())));

    #[test]
    fn database_urls_accept_only_postgres_and_sqlite() {
        assert_eq!(
            check_database_url("postgres://u:p@localhost/db").unwrap(),
            DatabaseKind::Postgres
        );
        assert_eq!(
            check_database_url("postgresql://localhost/db").unwrap(),
            DatabaseKind::Postgres
        );
        assert_eq!(
            check_database_url("sqlite::memory:").unwrap(),
            DatabaseKind::Sqlite
        );
        assert_eq!(
            check_database_url("sqlite://data/app.db").unwrap(),
            DatabaseKind::Sqlite
        );

        for url in ["mysql://u:p@localhost:3306/db", "mariadb://localhost/db"] {
            let msg = check_database_url(url).unwrap_err().to_string();
            assert!(msg.contains("MySQL is not supported"), "{url}: {msg}");
            assert!(
                msg.contains("PostgreSQL") && msg.contains("SQLite"),
                "{msg}"
            );
        }

        let msg = check_database_url("oracle://localhost/db")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("'oracle'"), "{msg}");
        assert!(
            msg.contains("PostgreSQL") && msg.contains("SQLite"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn configure_refuses_a_mysql_url() {
        let mut bridge = DbBridge::new();
        let err = bridge
            .configure(DbConfig {
                database_url: "mysql://u:p@localhost:3306/db".to_string(),
                max_connections: 5,
                min_connections: 1,
                connection_timeout: 1000,
                query_timeout: 1000,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("MySQL is not supported"), "{err}");
        assert!(bridge.get_driver().await.is_err());
    }

    #[test]
    fn write_statements_include_ctes_that_end_in_a_write() {
        assert!(is_write_statement("INSERT INTO t (x) VALUES (1)"));
        assert!(is_write_statement("  update t set x = 1"));
        assert!(is_write_statement(
            "WITH u AS (UPDATE products SET quantity = quantity - $2::integer WHERE id = $1::integer AND quantity >= $2::integer RETURNING id) INSERT INTO stock_movements (product_id, direction, amount) SELECT id, 'out', $2::integer FROM u"
        ));
        assert!(is_write_statement("WITH a (n) AS (SELECT 1), b AS MATERIALIZED (SELECT ')' AS p) DELETE FROM t WHERE x IN (SELECT n FROM a)"));
        assert!(is_write_statement("WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) INSERT INTO t (x) SELECT n FROM r"));
    }

    #[test]
    fn reads_are_not_write_statements() {
        assert!(!is_write_statement("SELECT * FROM t"));
        assert!(!is_write_statement(
            "WITH t AS (SELECT 1 AS x) SELECT x FROM t"
        ));
        assert!(!is_write_statement(
            "WITH u AS (UPDATE t SET x = 1 RETURNING x) SELECT x FROM u"
        ));
        assert!(!is_write_statement("WITH"));
        assert!(!is_write_statement("INSERTS"));
        assert!(!is_write_statement("WITHDRAW"));
    }

    #[tokio::test]
    async fn execute_runs_a_statement_with_a_cte() {
        let (bridge, _guard) = setup_test_db().await;
        let result = bridge
            .execute(json!({
                "sql": "WITH n AS (SELECT 'from cte' AS v) INSERT INTO users (name, email) SELECT v, 'cte@example.test' FROM n",
                "params": []
            }), None)
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["data"]["affected_rows"], 1, "{result}");
    }

    async fn setup_test_db() -> (DbBridge, tokio::sync::MutexGuard<'static, ()>) {
        let guard = TEST_DB_MUTEX.lock().await;

        let mut bridge = DbBridge::new();

        let config = DbConfig {
            database_url: "sqlite::memory:".to_string(),
            max_connections: 5,
            min_connections: 1,
            connection_timeout: 5000,
            query_timeout: 10000,
        };

        bridge.configure(config).await.unwrap();

        // Drop table if exists
        let _ = bridge
            .call(
                "execute",
                json!({
                    "sql": "DROP TABLE IF EXISTS users",
                    "params": []
                }),
            )
            .await;

        // Create test table
        let create_table = json!({
            "sql": "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, email TEXT UNIQUE NOT NULL, age INTEGER)",
            "params": []
        });

        let result = bridge.call("execute", create_table).await.unwrap();
        if result["ok"] != true {
            panic!(
                "Failed to create test table: {}",
                serde_json::to_string_pretty(&result).unwrap()
            );
        }

        (bridge, guard)
    }

    #[tokio::test]
    async fn test_db_config() {
        let mut bridge = DbBridge::new();

        let params = json!({
            "database_url": "sqlite::memory:",
            "max_connections": 5,
            "min_connections": 1,
            "connection_timeout": 5000,
            "query_timeout": 10000
        });

        let result = bridge.call("config", params).await.unwrap();
        assert_eq!(result["ok"], true);
    }

    #[tokio::test]
    async fn test_db_execute_insert() {
        let (mut bridge, _guard) = setup_test_db().await;

        let params = json!({
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Alice", "alice@example.com", 30]
        });

        let result = bridge.call("execute", params).await.unwrap();

        assert_eq!(result["ok"], true);
        assert_eq!(result["data"]["affected_rows"], 1);
    }

    #[tokio::test]
    async fn test_db_query_select() {
        let (mut bridge, _guard) = setup_test_db().await;

        // Insert test data
        let insert = json!({
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Bob", "bob@example.com", 25]
        });
        bridge.call("execute", insert).await.unwrap();

        // Query data
        let params = json!({
            "sql": "SELECT * FROM users WHERE email = $1",
            "params": ["bob@example.com"]
        });

        let result = bridge.call("query", params).await.unwrap();

        assert_eq!(result["ok"], true);
        assert_eq!(result["data"]["count"], 1);
        assert_eq!(result["data"]["rows"][0]["name"], "Bob");
        assert_eq!(result["data"]["rows"][0]["email"], "bob@example.com");
        assert_eq!(result["data"]["rows"][0]["age"], 25);
    }

    #[tokio::test]
    async fn test_db_query_select_all() {
        let (mut bridge, _guard) = setup_test_db().await;

        for i in 1..=3 {
            let insert = json!({
                "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
                "params": [format!("User{}", i), format!("user{}@example.com", i), 20 + i]
            });
            bridge.call("execute", insert).await.unwrap();
        }

        let params = json!({
            "sql": "SELECT * FROM users ORDER BY age",
            "params": []
        });

        let result = bridge.call("query", params).await.unwrap();

        assert_eq!(result["ok"], true);
        assert_eq!(result["data"]["count"], 3);
    }

    #[tokio::test]
    async fn test_db_execute_update() {
        let (mut bridge, _guard) = setup_test_db().await;

        let insert = json!({
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Charlie", "charlie@example.com", 35]
        });
        bridge.call("execute", insert).await.unwrap();

        let update = json!({
            "sql": "UPDATE users SET age = $1 WHERE email = $2",
            "params": [40, "charlie@example.com"]
        });

        let result = bridge.call("execute", update).await.unwrap();

        assert_eq!(result["ok"], true);
        assert_eq!(result["data"]["affected_rows"], 1);

        let query = json!({
            "sql": "SELECT age FROM users WHERE email = $1",
            "params": ["charlie@example.com"]
        });

        let result = bridge.call("query", query).await.unwrap();
        assert_eq!(result["data"]["rows"][0]["age"], 40);
    }

    #[tokio::test]
    async fn test_db_execute_delete() {
        let (mut bridge, _guard) = setup_test_db().await;

        let insert = json!({
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["David", "david@example.com", 28]
        });
        bridge.call("execute", insert).await.unwrap();

        let delete = json!({
            "sql": "DELETE FROM users WHERE email = $1",
            "params": ["david@example.com"]
        });

        let result = bridge.call("execute", delete).await.unwrap();

        assert_eq!(result["ok"], true);
        assert_eq!(result["data"]["affected_rows"], 1);
    }

    #[tokio::test]
    async fn test_db_transaction_commit() {
        let (mut bridge, _guard) = setup_test_db().await;

        let begin_result = bridge.call("transaction_begin", json!({})).await.unwrap();
        assert_eq!(begin_result["ok"], true);

        let tx_id = begin_result["data"]["tx_id"].as_str().unwrap();

        let execute1 = json!({
            "tx_id": tx_id,
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Eve", "eve@example.com", 29]
        });
        bridge.call("execute_in_tx", execute1).await.unwrap();

        let execute2 = json!({
            "tx_id": tx_id,
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Frank", "frank@example.com", 31]
        });
        bridge.call("execute_in_tx", execute2).await.unwrap();

        let commit = json!({"tx_id": tx_id});
        let result = bridge.call("transaction_commit", commit).await.unwrap();

        assert_eq!(result["ok"], true);

        let query = json!({
            "sql": "SELECT COUNT(*) as count FROM users",
            "params": []
        });
        let result = bridge.call("query", query).await.unwrap();
        assert_eq!(result["data"]["rows"][0]["count"], 2);
    }

    #[tokio::test]
    async fn test_db_transaction_rollback() {
        let (mut bridge, _guard) = setup_test_db().await;

        let begin_result = bridge.call("transaction_begin", json!({})).await.unwrap();
        let tx_id = begin_result["data"]["tx_id"].as_str().unwrap();

        let execute = json!({
            "tx_id": tx_id,
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Grace", "grace@example.com", 27]
        });
        bridge.call("execute_in_tx", execute).await.unwrap();

        let rollback = json!({"tx_id": tx_id});
        let result = bridge.call("transaction_rollback", rollback).await.unwrap();

        assert_eq!(result["ok"], true);

        let query = json!({
            "sql": "SELECT COUNT(*) as count FROM users",
            "params": []
        });
        let result = bridge.call("query", query).await.unwrap();
        assert_eq!(result["data"]["rows"][0]["count"], 0);
    }

    #[tokio::test]
    async fn test_db_validation_error() {
        let (mut bridge, _guard) = setup_test_db().await;

        let params = json!({
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Test", "test@example.com", 25]
        });

        let result = bridge.call("query", params).await.unwrap();

        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "VALIDATION_ERROR");
    }

    #[tokio::test]
    async fn test_db_unique_constraint_error() {
        let (mut bridge, _guard) = setup_test_db().await;

        let insert1 = json!({
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Alice", "alice@example.com", 30]
        });
        bridge.call("execute", insert1).await.unwrap();

        let insert2 = json!({
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Bob", "alice@example.com", 25]
        });

        let result = bridge.call("execute", insert2).await.unwrap();

        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "VALIDATION_ERROR");
    }

    // Regression for prompts f2c316ce / 1c2d7544 (2026-07-25):
    // Reproduces the "empty json.get result" symptom from clean-server 1.9.99
    // at the Rust boundary. If DbBridge::query returns the expected
    // {"ok":true,"data":{"rows":[{"greeting":"hello"}]}} envelope AND the
    // dotted-path traversal used by _json_get resolves "data.rows.0.greeting"
    // to "hello", then the bug does NOT live in the shape or the traversal —
    // it lives at the WASM string round-trip. If either half fails, the diff
    // points straight at the fix site.
    #[tokio::test]
    async fn test_db_query_shape_matches_json_get_dotted_path() {
        let (mut bridge, _guard) = setup_test_db().await;

        let params = json!({
            "sql": "SELECT 'hello' as greeting",
            "params": []
        });
        let result = bridge.call("query", params).await.unwrap();

        // Half 1: envelope shape as delivered by the DB bridge.
        let raw = result.to_string();
        assert!(
            raw.contains(r#""data""#) && raw.contains(r#""rows""#),
            "envelope missing data/rows keys, got: {}",
            raw
        );

        // Half 2: same dotted-path traversal that src/bridge.rs::_json_get uses
        // (lines 3260-3277). Any divergence between this and _json_get would
        // itself be a bug worth catching.
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let mut current = &parsed;
        for part in "data.rows.0.greeting".split('.') {
            let next = if let Ok(idx) = part.parse::<usize>() {
                current.get(idx)
            } else {
                current.get(part)
            };
            current = next.unwrap_or_else(|| {
                panic!(
                    "dotted-path traversal failed at segment {:?}; envelope was: {}",
                    part, raw
                )
            });
        }
        let extracted = match current {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        assert_eq!(
            extracted, "hello",
            "dotted-path resolved but value was wrong; envelope: {}",
            raw
        );
    }

    #[tokio::test]
    async fn test_db_query_in_tx() {
        let (mut bridge, _guard) = setup_test_db().await;

        let insert = json!({
            "sql": "INSERT INTO users (name, email, age) VALUES ($1, $2, $3)",
            "params": ["Test", "test@example.com", 25]
        });
        bridge.call("execute", insert).await.unwrap();

        let begin_result = bridge.call("transaction_begin", json!({})).await.unwrap();
        let tx_id = begin_result["data"]["tx_id"].as_str().unwrap();

        let query = json!({
            "tx_id": tx_id,
            "sql": "SELECT * FROM users WHERE email = $1",
            "params": ["test@example.com"]
        });

        let result = bridge.call("query_in_tx", query).await.unwrap();

        assert_eq!(result["ok"], true);
        assert_eq!(result["data"]["count"], 1);

        bridge
            .call("transaction_rollback", json!({"tx_id": tx_id}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_unknown_function() {
        let mut bridge = DbBridge::new();

        let result = bridge.call("unknown", json!({})).await.unwrap();

        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "DB_ERROR");
    }

    #[tokio::test]
    async fn test_transaction_not_found() {
        let mut bridge = DbBridge::new();

        let commit = json!({"tx_id": "invalid_tx_id"});
        let result = bridge.call("transaction_commit", commit).await.unwrap();

        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "TRANSACTION_ERROR");
    }

    #[tokio::test]
    async fn test_invalid_params() {
        let mut bridge = DbBridge::new();

        let params = json!({"invalid": "params"});

        let result = bridge.call("query", params).await.unwrap();

        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "VALIDATION_ERROR");
    }
}

// ============================================================================
// TESTS — new frame.data bridge methods
// ============================================================================

#[cfg(test)]
mod frame_data_bridge_tests {
    use super::*;

    async fn setup_test_db_new() -> DbBridge {
        let mut bridge = DbBridge::new();
        let config = DbConfig {
            database_url: "sqlite::memory:".to_string(),
            max_connections: 5,
            min_connections: 1,
            connection_timeout: 5000,
            query_timeout: 10000,
        };
        bridge.configure(config).await.unwrap();

        // Create users table for pagination tests
        let _ = bridge.call("execute", json!({
			"sql": "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, email TEXT UNIQUE NOT NULL)",
			"params": []
		})).await;

        bridge
    }

    #[tokio::test]
    async fn test_configure_from_json_valid() {
        let mut bridge = DbBridge::new();
        let result = bridge
			.configure_from_json(r#"{"database_url":"sqlite::memory:","max_connections":5,"min_connections":1,"connection_timeout":5000,"query_timeout":10000}"#)
			.await
			.unwrap();
        assert_eq!(
            result["ok"], true,
            "configure_from_json should succeed: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_configure_from_json_invalid() {
        let mut bridge = DbBridge::new();
        let result = bridge
            .configure_from_json("not valid json at all")
            .await
            .unwrap();
        assert_eq!(result["ok"], false, "Should fail on invalid JSON");
    }

    #[tokio::test]
    async fn test_paginate_basic() {
        let mut bridge = setup_test_db_new().await;

        // Insert 25 rows
        for i in 1..=25 {
            bridge
                .call(
                    "execute",
                    json!({
                        "sql": "INSERT INTO users (name, email) VALUES (?, ?)",
                        "params": [format!("User{}", i), format!("user{}@test.com", i)]
                    }),
                )
                .await
                .unwrap();
        }

        let result = bridge
            .call(
                "paginate",
                json!({
                    "table": "users",
                    "where": {},
                    "page": 1,
                    "per_page": 10
                }),
            )
            .await
            .unwrap();

        assert_eq!(result["ok"], true, "paginate should succeed: {:?}", result);
        let data = &result["data"];
        assert_eq!(data["total"], 25, "Total should be 25");
        assert_eq!(data["page"], 1);
        assert_eq!(data["per_page"], 10);
        assert_eq!(data["total_pages"], 3);
        assert_eq!(
            data["data"].as_array().unwrap().len(),
            10,
            "Should return 10 rows"
        );
    }

    #[tokio::test]
    async fn test_paginate_honours_order_clause() {
        let mut bridge = setup_test_db_new().await;
        for i in 1..=5 {
            bridge
                .call(
                    "execute",
                    json!({
                        "sql": "INSERT INTO users (name, email) VALUES (?, ?)",
                        "params": [format!("User{}", i), format!("user{}@test.com", i)]
                    }),
                )
                .await
                .unwrap();
        }
        let result = bridge
            .call(
                "paginate",
                json!({
                    "table": "users",
                    "where": {"__where": "", "__order": "name DESC"},
                    "page": 1,
                    "per_page": 2
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "paginate should succeed: {:?}", result);
        let names: Vec<&str> = result["data"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["User5", "User4"]);
        assert_eq!(result["data"]["total"], 5);
    }

    #[tokio::test]
    async fn test_paginate_rejects_unsafe_order_clause() {
        let mut bridge = setup_test_db_new().await;
        let result = bridge
            .call(
                "paginate",
                json!({
                    "table": "users",
                    "where": {"__order": "name; DROP TABLE users"},
                    "page": 1,
                    "per_page": 2
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "VALIDATION_ERROR");
    }

    #[test]
    fn test_order_by_clause_parsing() {
        assert_eq!(order_by_clause(&json!({})), Ok(None));
        assert_eq!(
            order_by_clause(&json!({"__order": "created_at desc, users.id"})),
            Ok(Some("created_at DESC, users.id".to_string()))
        );
        assert!(order_by_clause(&json!({"__order": "id SIDEWAYS"})).is_err());
        assert!(order_by_clause(&json!({"__order": "id ASC extra"})).is_err());
        assert!(order_by_clause(&json!({"__order": "(SELECT 1)"})).is_err());
    }

    #[test]
    fn test_number_placeholders_for_postgres() {
        assert_eq!(
            number_placeholders("SELECT * FROM t WHERE a = ? AND b > ? LIMIT 3"),
            "SELECT * FROM t WHERE a = $1 AND b > $2 LIMIT 3"
        );
        // `?` inside literals and quoted identifiers is not a placeholder.
        assert_eq!(
            number_placeholders("SELECT '?' AS \"q?\" FROM t WHERE a = ?"),
            "SELECT '?' AS \"q?\" FROM t WHERE a = $1"
        );
    }

    #[test]
    fn test_cursor_bind_value_matches_column_type() {
        // Numeric column: the textual cursor is bound as a number.
        assert_eq!(cursor_bind_value("42", Some(true)), json!(42));
        assert_eq!(cursor_bind_value("1.5", Some(true)), json!(1.5));
        // Text column: stays text even when it looks numeric.
        assert_eq!(cursor_bind_value("42", Some(false)), json!("42"));
        // Unknown column type: integers become numbers, the rest text.
        assert_eq!(cursor_bind_value("7", None), json!(7));
        assert_eq!(cursor_bind_value("abc", None), json!("abc"));
    }

    #[tokio::test]
    async fn test_cursor_page_after_integer_cursor() {
        let mut bridge = setup_test_db_new().await;
        for i in 1..=5 {
            bridge
                .call(
                    "execute",
                    json!({
                        "sql": "INSERT INTO users (name, email) VALUES (?, ?)",
                        "params": [format!("User{}", i), format!("user{}@test.com", i)]
                    }),
                )
                .await
                .unwrap();
        }
        let result = bridge
            .call(
                "cursor_page",
                json!({"table": "users", "where": {}, "per_page": 2, "after": "2", "by_field": "id"}),
            )
            .await
            .unwrap();
        assert_eq!(
            result["ok"], true,
            "cursor_page should succeed: {:?}",
            result
        );
        let ids: Vec<i64> = result["data"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 4]);
        assert_eq!(result["data"]["has_more"], true);
    }

    #[tokio::test]
    async fn test_paginate_last_page() {
        let mut bridge = setup_test_db_new().await;

        for i in 1..=15 {
            bridge
                .call(
                    "execute",
                    json!({
                        "sql": "INSERT INTO users (name, email) VALUES (?, ?)",
                        "params": [format!("User{}", i), format!("user{}@pglast.com", i)]
                    }),
                )
                .await
                .unwrap();
        }

        let result = bridge
            .call(
                "paginate",
                json!({
                    "table": "users",
                    "where": {},
                    "page": 2,
                    "per_page": 10
                }),
            )
            .await
            .unwrap();

        assert_eq!(result["ok"], true);
        let data = &result["data"];
        assert_eq!(data["total"], 15);
        assert_eq!(
            data["data"].as_array().unwrap().len(),
            5,
            "Last page should have 5 rows"
        );
    }

    #[tokio::test]
    async fn test_paginate_invalid_table() {
        let mut bridge = setup_test_db_new().await;
        let result = bridge
            .call(
                "paginate",
                json!({
                    "table": "users; DROP TABLE users--",
                    "where": {},
                    "page": 1,
                    "per_page": 10
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "VALIDATION_ERROR");
    }

    #[tokio::test]
    async fn test_cursor_page_basic() {
        let mut bridge = setup_test_db_new().await;

        for i in 1..=10 {
            bridge
                .call(
                    "execute",
                    json!({
                        "sql": "INSERT INTO users (name, email) VALUES (?, ?)",
                        "params": [format!("User{}", i), format!("user{}@cursor.com", i)]
                    }),
                )
                .await
                .unwrap();
        }

        // First page — no after cursor
        let result = bridge
            .call(
                "cursor_page",
                json!({
                    "table": "users",
                    "where": {},
                    "per_page": 5,
                    "after": "",
                    "by_field": "id"
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            result["ok"], true,
            "cursor_page should succeed: {:?}",
            result
        );
        let data = &result["data"];
        assert_eq!(
            data["data"].as_array().unwrap().len(),
            5,
            "Should return 5 rows"
        );
        assert_eq!(data["has_more"], true);
        assert!(
            data["next_cursor"] != serde_json::Value::Null,
            "next_cursor should be set"
        );
    }

    #[tokio::test]
    async fn test_cursor_page_no_more() {
        let mut bridge = setup_test_db_new().await;

        for i in 1..=3 {
            bridge
                .call(
                    "execute",
                    json!({
                        "sql": "INSERT INTO users (name, email) VALUES (?, ?)",
                        "params": [format!("User{}", i), format!("user{}@cursor2.com", i)]
                    }),
                )
                .await
                .unwrap();
        }

        let result = bridge
            .call(
                "cursor_page",
                json!({
                    "table": "users",
                    "where": {},
                    "per_page": 10,
                    "after": "",
                    "by_field": "id"
                }),
            )
            .await
            .unwrap();

        assert_eq!(result["ok"], true);
        let data = &result["data"];
        assert_eq!(data["has_more"], false);
        assert_eq!(data["next_cursor"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_cursor_page_invalid_field() {
        let mut bridge = setup_test_db_new().await;
        let result = bridge
            .call(
                "cursor_page",
                json!({
                    "table": "users",
                    "where": {},
                    "per_page": 10,
                    "after": "",
                    "by_field": "id; DROP TABLE users--"
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "VALIDATION_ERROR");
    }

    #[tokio::test]
    async fn test_migration_diff_no_difference() {
        let mut bridge = setup_test_db_new().await;
        // The declared columns match the live columns (id, name, email)
        let result = bridge
            .call(
                "migration_diff",
                json!({
                    "declared": {"id": "INTEGER", "name": "TEXT", "email": "TEXT"},
                    "live": {"id": "", "name": "", "email": ""}
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        let sql = result["data"]["sql"].as_str().unwrap_or("");
        assert!(
            sql.is_empty(),
            "Should return empty SQL when in sync, got: {}",
            sql
        );
    }

    #[tokio::test]
    async fn test_migration_diff_missing_column() {
        let mut bridge = setup_test_db_new().await;
        // Declare a new column "age" that does not exist in live schema
        let result = bridge.call("migration_diff", json!({
			"declared": {"id": "INTEGER", "name": "TEXT", "email": "TEXT", "age": "INTEGER"},
			"live": {"id": "", "name": "", "email": ""}
		})).await.unwrap();
        assert_eq!(result["ok"], true);
        let sql = result["data"]["sql"].as_str().unwrap_or("");
        assert!(!sql.is_empty(), "Should generate ALTER TABLE SQL");
        assert!(
            sql.contains("ADD COLUMN age"),
            "SQL should add the age column: {}",
            sql
        );
    }

    #[tokio::test]
    async fn test_migration_status_no_db() {
        // Without DB configured, all migrations report as pending
        let mut bridge = DbBridge::new();
        bridge
            .call(
                "register_migration",
                json!({
                    "name": "001_create_users",
                    "up_sql": "CREATE TABLE users (id INT)",
                    "down_sql": "DROP TABLE users"
                }),
            )
            .await
            .unwrap();

        let result = bridge.migration_status().await.unwrap();
        assert_eq!(result["ok"], true);
        let migrations = result["data"]["migrations"].as_array().unwrap();
        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0]["name"], "001_create_users");
        assert_eq!(migrations[0]["applied_at"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_migration_status_after_run() {
        let mut bridge = setup_test_db_new().await;
        bridge
            .call(
                "register_migration",
                json!({
                    "name": "001_add_age",
                    "up_sql": "ALTER TABLE users ADD COLUMN age INTEGER",
                    "down_sql": "SELECT 1"
                }),
            )
            .await
            .unwrap();

        // Run migrations
        let run_result = bridge.run_migrations_call(json!({})).await.unwrap();
        assert_eq!(run_result["ok"], true);
        assert_eq!(run_result["data"]["applied"], 1);

        // Now check status
        let result = bridge.migration_status().await.unwrap();
        assert_eq!(result["ok"], true);
        let migrations = result["data"]["migrations"].as_array().unwrap();
        assert_eq!(migrations.len(), 1);
        assert_ne!(migrations[0]["applied_at"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_rollback_migration_success() {
        let mut bridge = setup_test_db_new().await;
        bridge
            .call(
                "register_migration",
                json!({
                    "name": "001_add_age",
                    "up_sql": "ALTER TABLE users ADD COLUMN age INTEGER",
                    "down_sql": "SELECT 1"
                }),
            )
            .await
            .unwrap();

        // Apply first
        bridge.run_migrations_call(json!({})).await.unwrap();

        // Rollback
        let result = bridge
            .rollback_migration(json!({"name": "001_add_age"}))
            .await
            .unwrap();
        assert_eq!(
            result["ok"], true,
            "rollback_migration should succeed: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_rollback_migration_not_found() {
        let mut bridge = setup_test_db_new().await;
        let result = bridge
            .rollback_migration(json!({"name": "nonexistent_migration"}))
            .await
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn test_rollback_migration_no_down_sql() {
        let mut bridge = setup_test_db_new().await;
        bridge
            .call(
                "register_migration",
                json!({
                    "name": "001_no_down",
                    "up_sql": "ALTER TABLE users ADD COLUMN phone TEXT",
                    "down_sql": ""
                }),
            )
            .await
            .unwrap();

        let result = bridge
            .rollback_migration(json!({"name": "001_no_down"}))
            .await
            .unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["err"]["code"], "VALIDATION_ERROR");
    }

    #[tokio::test]
    async fn test_run_migrations_count() {
        let mut bridge = setup_test_db_new().await;
        bridge
            .call(
                "register_migration",
                json!({
                    "name": "001_m1",
                    "up_sql": "CREATE TABLE IF NOT EXISTS m1_test (id INTEGER PRIMARY KEY)",
                    "down_sql": "DROP TABLE IF EXISTS m1_test"
                }),
            )
            .await
            .unwrap();
        bridge
            .call(
                "register_migration",
                json!({
                    "name": "002_m2",
                    "up_sql": "CREATE TABLE IF NOT EXISTS m2_test (id INTEGER PRIMARY KEY)",
                    "down_sql": "DROP TABLE IF EXISTS m2_test"
                }),
            )
            .await
            .unwrap();

        let result = bridge.run_migrations_call(json!({})).await.unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["data"]["applied"], 2, "Should apply 2 migrations");

        // Running again applies 0 (already applied)
        let result2 = bridge.run_migrations_call(json!({})).await.unwrap();
        assert_eq!(result2["ok"], true);
        assert_eq!(result2["data"]["applied"], 0, "Re-running applies 0");
    }

    #[tokio::test]
    async fn test_valid_field_true() {
        let mut bridge = setup_test_db_new().await;
        let result = bridge
            .call("valid_field", json!({"table": "users", "field": "email"}))
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["data"]["valid"], true,
            "email should be a valid field"
        );
    }

    #[tokio::test]
    async fn test_valid_field_false() {
        let mut bridge = setup_test_db_new().await;
        let result = bridge
            .call(
                "valid_field",
                json!({"table": "users", "field": "nonexistent_col"}),
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["data"]["valid"], false,
            "nonexistent_col should be invalid"
        );
    }

    #[tokio::test]
    async fn test_valid_field_sql_injection_attempt() {
        let mut bridge = setup_test_db_new().await;
        // A field name containing SQL metacharacters must be rejected
        let result = bridge
            .call(
                "valid_field",
                json!({
                    "table": "users",
                    "field": "id; DROP TABLE users--"
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["data"]["valid"], false,
            "SQL injection attempt should be rejected"
        );
    }

    #[test]
    fn test_is_safe_identifier_valid() {
        assert!(is_safe_identifier("users"));
        assert!(is_safe_identifier("user_name"));
        assert!(is_safe_identifier("Column1"));
    }

    #[test]
    fn test_is_safe_identifier_invalid() {
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("user-name"));
        assert!(!is_safe_identifier("table name"));
        assert!(!is_safe_identifier("id; DROP"));
    }

    #[test]
    fn test_build_where_clause_empty() {
        let (clause, params) = build_where_clause(&json!({}));
        assert!(clause.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn test_build_where_clause_single() {
        let (clause, params) = build_where_clause(&json!({"name": "Alice"}));
        assert_eq!(clause, "name = ?");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn test_build_where_clause_rejects_unsafe_keys() {
        // Keys with SQL metacharacters must be skipped
        let (clause, params) = build_where_clause(&json!({
            "safe_col": "value",
            "unsafe; DROP--": "evil"
        }));
        assert!(
            clause.contains("safe_col"),
            "Safe column should be included"
        );
        assert!(!clause.contains("unsafe"), "Unsafe key should be excluded");
        assert_eq!(params.len(), 1, "Only 1 param for the safe column");
    }

    #[test]
    fn test_build_where_clause_dunder_where_appended_as_raw_fragment() {
        // Bug DB-BUILD-WHERE-IGNORES-DUNDER-WHERE: the framework's frame.data plugin emits
        // {"__where": "<sql_fragment>"} for operators like `!= null` / `> x`. The bridge
        // must inject the fragment verbatim, NOT generate `WHERE __where = ?`.
        let (clause, params) = build_where_clause(&json!({
            "__where": "published_at IS NOT NULL"
        }));
        assert_eq!(clause, "published_at IS NOT NULL");
        assert!(
            params.is_empty(),
            "__where fragment must not produce bind params"
        );
    }

    #[test]
    fn test_build_where_clause_dunder_where_combined_with_equality() {
        let (clause, params) = build_where_clause(&json!({
            "author_id": 42,
            "__where": "published_at IS NOT NULL"
        }));
        // Both fragments must be present, joined with AND. The framework guarantees
        // the equality and raw fragments are compatible.
        assert!(clause.contains("author_id = ?"), "equality clause present");
        assert!(
            clause.contains("published_at IS NOT NULL"),
            "raw fragment present"
        );
        assert!(clause.contains(" AND "), "fragments joined with AND");
        assert_eq!(params.len(), 1, "only the equality column binds a param");
    }

    #[test]
    fn test_build_where_clause_dunder_order_is_skipped() {
        // __order is a reserved protocol key carrying ORDER BY metadata; it must not
        // produce a WHERE clause. Order is the caller's responsibility.
        let (clause, params) = build_where_clause(&json!({
            "name": "Alice",
            "__order": "created_at DESC"
        }));
        assert_eq!(clause, "name = ?");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn test_build_where_clause_unknown_dunder_keys_are_skipped() {
        // Any __* key not recognised must be silently ignored to keep the protocol
        // namespace forward-compatible.
        let (clause, params) = build_where_clause(&json!({
            "__future": "something",
            "name": "Alice"
        }));
        assert_eq!(clause, "name = ?");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn test_build_where_clause_empty_dunder_where_produces_no_clause() {
        let (clause, params) = build_where_clause(&json!({"__where": ""}));
        assert!(clause.is_empty());
        assert!(params.is_empty());
    }

    // ────────────────────────────────────────────────────────────────────────
    // Typed bind tag tests (FRAME-DATA-ORM-NOW-NOT-CONVERTED-FOR-DATETIME-BIND)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn decode_typed_bind_epoch_s() {
        let v = json!({"__type": "epoch_s", "value": 1782437320_i64});
        let dt = DatabaseDriver::decode_typed_bind(&v).expect("epoch_s tag should decode");
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-06-26 01:28:40"
        );
    }

    #[test]
    fn decode_typed_bind_epoch_ms() {
        let v = json!({"__type": "epoch_ms", "value": 1782437320_500_i64});
        let dt = DatabaseDriver::decode_typed_bind(&v).expect("epoch_ms tag should decode");
        assert_eq!(dt.and_utc().timestamp_millis(), 1782437320500);
    }

    #[test]
    fn decode_typed_bind_datetime_iso() {
        let v = json!({"__type": "datetime_iso", "value": "2026-06-26T01:28:40Z"});
        let dt = DatabaseDriver::decode_typed_bind(&v).expect("datetime_iso tag should decode");
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-06-26 01:28:40"
        );
    }

    #[test]
    fn decode_typed_bind_returns_none_for_plain_values() {
        // Plain values must fall through so existing binds keep working.
        assert!(DatabaseDriver::decode_typed_bind(&json!(42_i64)).is_none());
        assert!(DatabaseDriver::decode_typed_bind(&json!("hello")).is_none());
        assert!(DatabaseDriver::decode_typed_bind(&json!(null)).is_none());
        assert!(DatabaseDriver::decode_typed_bind(&json!(true)).is_none());
        assert!(DatabaseDriver::decode_typed_bind(&json!([1, 2, 3])).is_none());
    }

    #[test]
    fn decode_typed_bind_returns_none_for_unknown_tag() {
        // Forward compatibility: unknown tags fall through to JSON object bind.
        let v = json!({"__type": "future_tag", "value": 123});
        assert!(DatabaseDriver::decode_typed_bind(&v).is_none());
    }

    #[test]
    fn decode_typed_bind_returns_none_for_object_without_tag() {
        // Plain JSON objects (used elsewhere in the protocol) must not be hijacked.
        let v = json!({"name": "Alice", "age": 30});
        assert!(DatabaseDriver::decode_typed_bind(&v).is_none());
    }

    #[test]
    fn decode_typed_bind_returns_none_for_invalid_epoch_value() {
        // String where i64 is required.
        let v = json!({"__type": "epoch_s", "value": "not-a-number"});
        assert!(DatabaseDriver::decode_typed_bind(&v).is_none());
    }

    #[tokio::test]
    async fn execute_with_epoch_s_tag_binds_as_datetime() {
        // End-to-end: framework passes {"__type":"epoch_s","value":N} for now(),
        // SQLite stores it as an ISO-8601 string in a DATETIME column.
        // Use a unique in-memory database per test to avoid cross-test contention.
        let mut bridge = DbBridge::new();
        let config = DbConfig {
            database_url: "sqlite::memory:".to_string(),
            max_connections: 1,
            min_connections: 1,
            connection_timeout: 5000,
            query_timeout: 10000,
        };
        bridge.configure(config).await.unwrap();

        bridge
            .call(
                "execute",
                json!({
                    "sql": "CREATE TABLE docs (id INTEGER PRIMARY KEY, updated_at DATETIME)",
                    "params": []
                }),
            )
            .await
            .unwrap();

        let insert = bridge
            .call(
                "execute",
                json!({
                    "sql": "INSERT INTO docs (id, updated_at) VALUES (?, ?)",
                    "params": [1, {"__type": "epoch_s", "value": 1782437320_i64}]
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            insert["ok"], true,
            "insert with epoch_s tag must succeed: {:?}",
            insert
        );

        let rows = bridge
            .call(
                "query",
                json!({
                    "sql": "SELECT updated_at FROM docs WHERE id = ?",
                    "params": [1]
                }),
            )
            .await
            .unwrap();
        assert_eq!(rows["ok"], true);
        let updated_at = rows["data"]["rows"][0]["updated_at"].as_str().unwrap();
        assert!(
            updated_at.starts_with("2026-06-26 01:28:40"),
            "updated_at should be an ISO-8601 datetime string, got {:?}",
            updated_at
        );
    }
}

/// Integration tests for PostgreSQL
/// Run with: INTEGRATION_TESTS=1 cargo test integration --no-fail-fast -- --test-threads=1
#[cfg(test)]
mod integration_tests {
    use super::*;

    fn skip_if_no_integration() -> bool {
        std::env::var("INTEGRATION_TESTS").is_err()
    }

    async fn setup_postgres() -> Option<DbBridge> {
        if skip_if_no_integration() {
            return None;
        }

        let mut bridge = DbBridge::new();
        let config = DbConfig {
            database_url: "postgres://clean:cleanpass@localhost:5432/cleantest".to_string(),
            max_connections: 5,
            min_connections: 1,
            connection_timeout: 10000,
            query_timeout: 30000,
        };

        match bridge.configure(config).await {
            Ok(()) => Some(bridge),
            Err(e) => {
                eprintln!("Failed to connect to PostgreSQL: {}", e);
                None
            }
        }
    }

    #[tokio::test]
    async fn integration_test_postgres_query() {
        let Some(mut bridge) = setup_postgres().await else {
            println!("Skipping PostgreSQL integration test (set INTEGRATION_TESTS=1 to run)");
            return;
        };

        // Query users table with all columns - native driver supports all types
        let query = json!({
            "sql": "SELECT * FROM users ORDER BY id",
            "params": []
        });

        let result = bridge.call("query", query).await.unwrap();

        assert_eq!(result["ok"], true, "PostgreSQL query failed: {:?}", result);
        assert!(
            result["data"]["count"].as_i64().unwrap() >= 4,
            "Expected at least 4 users"
        );

        let first_user = &result["data"]["rows"][0];
        assert_eq!(first_user["name"], "Alice Johnson");
        assert_eq!(first_user["email"], "alice@example.com");
        assert_eq!(first_user["role"], "admin");
        // Native driver supports booleans!
        assert_eq!(first_user["active"], true);

        println!("PostgreSQL query test passed!");
    }

    #[tokio::test]
    async fn integration_test_postgres_crud() {
        let Some(mut bridge) = setup_postgres().await else {
            println!("Skipping PostgreSQL CRUD test (set INTEGRATION_TESTS=1 to run)");
            return;
        };

        // Cleanup
        let cleanup = json!({
            "sql": "DELETE FROM users WHERE email = $1",
            "params": ["testuser@integration.com"]
        });
        let _ = bridge.call("execute", cleanup).await;

        // INSERT
        let insert = json!({
            "sql": "INSERT INTO users (name, email, role) VALUES ($1, $2, $3)",
            "params": ["Test User", "testuser@integration.com", "tester"]
        });
        let result = bridge.call("execute", insert).await.unwrap();
        assert_eq!(result["ok"], true, "PostgreSQL INSERT failed: {:?}", result);
        assert_eq!(result["data"]["affected_rows"], 1);

        // SELECT with all columns
        let query = json!({
            "sql": "SELECT * FROM users WHERE email = $1",
            "params": ["testuser@integration.com"]
        });
        let result = bridge.call("query", query).await.unwrap();
        assert_eq!(result["ok"], true, "PostgreSQL SELECT failed: {:?}", result);
        assert_eq!(result["data"]["count"], 1);
        assert_eq!(result["data"]["rows"][0]["name"], "Test User");

        // UPDATE
        let update = json!({
            "sql": "UPDATE users SET role = $1 WHERE email = $2",
            "params": ["admin", "testuser@integration.com"]
        });
        let result = bridge.call("execute", update).await.unwrap();
        assert_eq!(result["ok"], true, "PostgreSQL UPDATE failed: {:?}", result);
        assert_eq!(result["data"]["affected_rows"], 1);

        // DELETE
        let delete = json!({
            "sql": "DELETE FROM users WHERE email = $1",
            "params": ["testuser@integration.com"]
        });
        let result = bridge.call("execute", delete).await.unwrap();
        assert_eq!(result["ok"], true, "PostgreSQL DELETE failed: {:?}", result);
        assert_eq!(result["data"]["affected_rows"], 1);

        println!("PostgreSQL CRUD test passed!");
    }

    #[tokio::test]
    async fn integration_test_postgres_join() {
        let Some(mut bridge) = setup_postgres().await else {
            println!("Skipping PostgreSQL JOIN test (set INTEGRATION_TESTS=1 to run)");
            return;
        };

        let query = json!({
            "sql": "SELECT p.title, u.name as author, u.active FROM posts p JOIN users u ON p.author_id = u.id WHERE u.active = true ORDER BY p.id",
            "params": []
        });

        let result = bridge.call("query", query).await.unwrap();

        assert_eq!(result["ok"], true, "PostgreSQL JOIN failed: {:?}", result);
        assert!(
            result["data"]["count"].as_i64().unwrap() >= 3,
            "Expected at least 3 posts"
        );
        // Verify boolean is properly returned
        assert_eq!(result["data"]["rows"][0]["active"], true);

        println!("PostgreSQL JOIN test passed!");
    }

    #[tokio::test]
    async fn integration_test_postgres_transaction() {
        let Some(mut bridge) = setup_postgres().await else {
            println!("Skipping PostgreSQL transaction test (set INTEGRATION_TESTS=1 to run)");
            return;
        };

        let begin_result = bridge.call("transaction_begin", json!({})).await.unwrap();
        assert_eq!(begin_result["ok"], true);
        let tx_id = begin_result["data"]["tx_id"].as_str().unwrap();

        let execute1 = json!({
            "tx_id": tx_id,
            "sql": "INSERT INTO users (name, email, role) VALUES ($1, $2, $3)",
            "params": ["TxUser1", "txuser1@integration.com", "tester"]
        });
        bridge.call("execute_in_tx", execute1).await.unwrap();

        let execute2 = json!({
            "tx_id": tx_id,
            "sql": "INSERT INTO users (name, email, role) VALUES ($1, $2, $3)",
            "params": ["TxUser2", "txuser2@integration.com", "tester"]
        });
        bridge.call("execute_in_tx", execute2).await.unwrap();

        let commit = json!({"tx_id": tx_id});
        let result = bridge.call("transaction_commit", commit).await.unwrap();
        assert_eq!(
            result["ok"], true,
            "PostgreSQL transaction commit failed: {:?}",
            result
        );

        let query = json!({
            "sql": "SELECT COUNT(*) as count FROM users WHERE email LIKE $1",
            "params": ["%@integration.com"]
        });
        let result = bridge.call("query", query).await.unwrap();
        assert!(result["data"]["rows"][0]["count"].as_i64().unwrap() >= 2);

        // Cleanup
        let cleanup = json!({
            "sql": "DELETE FROM users WHERE email LIKE $1",
            "params": ["%@integration.com"]
        });
        bridge.call("execute", cleanup).await.unwrap();

        println!("PostgreSQL transaction test passed!");
    }
}
