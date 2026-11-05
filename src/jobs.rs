//! Background job queue runtime for frame.jobs.
//!
//! Implements a persistent job queue backed by SQLite (`__clean_jobs` table)
//! with a write-through in-memory cache for hot-path lookups.
//!
//! # Persistence model
//!
//! - Every mutation to a `JobRecord` is written through to SQLite immediately.
//! - On startup, `recover_from_disk` loads all `pending` and `running` rows
//!   from SQLite.  Any row left in `running` (orphaned by a hard kill) is
//!   reset to `pending` so the worker retries it.
//! - `succeeded`, `failed`, and `cancelled` rows remain in the DB but are NOT
//!   loaded into the in-memory cache — `_job_status` / `_job_result` fall back
//!   to a DB query when the id is absent from the cache.
//! - A startup cleanup sweep deletes finished rows older than
//!   `JOBS_RETENTION_DAYS` (default 7).
//!
//! # Architecture
//!
//! ```text
//! _job_register / _job_enqueue
//!        │
//!        ▼
//! ┌──────────────────────────┐        ┌─────────────────┐
//! │  SharedJobsState         │ ──────▶│  __clean_jobs   │
//! │  (Arc<Mutex<JobsStore>>) │ write- │  SQLite table   │
//! │  + HashMap<id,JobRecord> │ through│  (via SqlitePool│
//! └──────────┬───────────────┘        └─────────────────┘
//!            │ polled every second
//!            ▼
//! ┌──────────────────────────┐
//! │  worker_loop             │  tokio task
//! │  - pick pending+due jobs │
//! │  - set task-locals       │
//! │  - call WASM handler     │
//! │  - apply retry policy    │
//! └──────────────────────────┘
//! ```

use crate::wasm::{RequestContext, WasmInstance};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Task-local job execution context
// ---------------------------------------------------------------------------

tokio::task_local! {
    /// The job ID of the job currently being executed. Empty string outside handler.
    pub static JOB_CURRENT_ID: String;

    /// The args JSON of the job currently being executed. Empty string outside handler.
    pub static JOB_CURRENT_ARGS: String;

    /// The attempt number (1-based) of the current job execution. 0 outside handler.
    pub static JOB_CURRENT_ATTEMPT: i32;

    /// When set by `_job_retry_after`, override the computed backoff delay (milliseconds).
    /// -1 means not set (use default backoff formula).
    pub static JOB_RETRY_OVERRIDE_MS: std::cell::Cell<i64>;

    /// Reason string when `_job_fail` was called; None if not called.
    pub static JOB_EXPLICIT_FAIL: std::cell::RefCell<Option<String>>;

    /// Result JSON when `_job_succeed` was called; None if not called.
    pub static JOB_EXPLICIT_SUCCEED: std::cell::RefCell<Option<String>>;
}

/// Retrieve the current job ID from task-local storage.
/// Returns an empty string when called outside a job handler.
pub fn current_job_id() -> String {
    JOB_CURRENT_ID.try_with(|s| s.clone()).unwrap_or_default()
}

/// Retrieve the current job args from task-local storage.
/// Returns an empty string when called outside a job handler.
pub fn current_job_args() -> String {
    JOB_CURRENT_ARGS.try_with(|s| s.clone()).unwrap_or_default()
}

/// Retrieve the current attempt number from task-local storage.
/// Returns 0 when called outside a job handler.
pub fn current_job_attempt() -> i32 {
    JOB_CURRENT_ATTEMPT.try_with(|n| *n).unwrap_or(0)
}

/// Convert a duration or Unix timestamp in seconds — the unit the job bridges
/// take, like `_time_now` — to the milliseconds the queue stores internally.
/// Negative, NaN and infinite inputs clamp to 0.
pub fn seconds_to_ms(seconds: f64) -> u64 {
    if seconds.is_finite() && seconds > 0.0 {
        (seconds * 1000.0).round() as u64
    } else {
        0
    }
}

/// Request a retry after the specified delay in milliseconds (called from inside a handler).
/// Overrides the computed backoff for this attempt.
pub fn request_retry_after_ms(delay_ms: i64) {
    JOB_RETRY_OVERRIDE_MS
        .try_with(|cell| cell.set(delay_ms))
        .ok();
}

/// Mark the current job as explicitly failed (called from inside a handler via `_job_fail`).
pub fn mark_explicit_fail(reason: String) {
    JOB_EXPLICIT_FAIL
        .try_with(|cell| {
            *cell.borrow_mut() = Some(reason);
        })
        .ok();
}

/// Mark the current job as explicitly succeeded with a result JSON (called via `_job_succeed`).
pub fn mark_explicit_succeed(result: String) {
    JOB_EXPLICIT_SUCCEED
        .try_with(|cell| {
            *cell.borrow_mut() = Some(result);
        })
        .ok();
}

// ---------------------------------------------------------------------------
// Job configuration (registered at startup via _job_register)
// ---------------------------------------------------------------------------

/// Retry backoff strategy for a registered job type.
#[derive(Debug, Clone, PartialEq)]
pub enum BackoffStrategy {
    /// Always use the same fixed delay.
    Fixed,
    /// Delay doubles with each attempt: `delay * 2^(attempt-1)`.
    Exponential,
}

impl BackoffStrategy {
    /// Parse from the string representation used in `_job_register`.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "exponential" => BackoffStrategy::Exponential,
            _ => BackoffStrategy::Fixed,
        }
    }

    /// Compute the next delay in milliseconds given the base delay and attempt number.
    ///
    /// `attempt` is 1-based (first execution = 1, first retry = 2, …).
    pub fn compute_delay(&self, base_ms: u64, attempt: u32) -> u64 {
        match self {
            BackoffStrategy::Fixed => base_ms,
            BackoffStrategy::Exponential => {
                // delay * 2^(attempt-1), capped at 24 hours to prevent overflow
                let exponent = (attempt.saturating_sub(1)).min(30);
                base_ms.saturating_mul(1u64 << exponent).min(86_400_000)
            }
        }
    }
}

/// Configuration for a registered job handler, stored at startup.
#[derive(Debug, Clone)]
pub struct JobConfig {
    /// Job name used in `_job_enqueue` calls.
    pub name: String,
    /// The WASM export name to call when the job runs.
    pub handler: String,
    /// Maximum number of attempts before the job is marked failed.
    pub max_attempts: u32,
    /// Backoff strategy for retries.
    pub backoff: BackoffStrategy,
    /// Base retry delay in milliseconds.
    pub delay_ms: u64,
    /// Maximum handler run time in milliseconds (0 = unlimited).
    pub timeout_ms: u64,
    /// Named queue for prioritised processing.
    pub queue: String,
}

// ---------------------------------------------------------------------------
// Job record (live state stored in memory during server run)
// ---------------------------------------------------------------------------

/// Current lifecycle status of a job.
#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse_status(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(JobStatus::Pending),
            "running" => Some(JobStatus::Running),
            "succeeded" => Some(JobStatus::Succeeded),
            "failed" => Some(JobStatus::Failed),
            "cancelled" => Some(JobStatus::Cancelled),
            _ => None,
        }
    }
}

/// A single job instance tracked in the in-memory store.
#[derive(Debug, Clone)]
pub struct JobRecord {
    /// Unique job identifier (UUID v4 string).
    pub id: String,
    /// Name of the registered job type.
    pub name: String,
    /// Serialised argument payload (JSON string).
    pub args: String,
    /// Current lifecycle status.
    pub status: JobStatus,
    /// Number of attempts that have run so far.
    pub attempt: u32,
    /// Maximum attempts allowed (copied from JobConfig at enqueue time).
    pub max_attempts: u32,
    /// Backoff strategy (copied from JobConfig at enqueue time).
    pub backoff: BackoffStrategy,
    /// Base retry delay in ms (copied from JobConfig at enqueue time).
    pub delay_ms: u64,
    /// Timeout in ms (0 = unlimited).
    pub timeout_ms: u64,
    /// Queue name.
    pub queue: String,
    /// The WASM handler export name.
    pub handler: String,
    /// When the job should next run (Unix ms).
    pub scheduled_at: u64,
    /// When this record was created (Unix ms).
    pub created_at: u64,
    /// When this record was last updated (Unix ms).
    pub updated_at: u64,
    /// When the job transitioned to a terminal state (Unix ms). None while
    /// pending or running.
    pub finished_at: Option<u64>,
    /// Result payload set by `_job_succeed`, None until then.
    pub result: Option<String>,
    /// Error message from the last failed attempt, if any.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Cron schedule registration
// ---------------------------------------------------------------------------

/// A registered cron schedule entry.
#[derive(Debug, Clone)]
pub struct CronSchedule {
    /// User-visible name (key in `_schedule_cancel`).
    pub name: String,
    /// Registration ID returned by `_schedule_cron`; also accepted by
    /// `_schedule_cancel`.
    pub id: String,
    /// Cron expression string (e.g. `"*/1 * * * *"`).
    pub cron_expr: String,
    /// WASM export name to call on each tick.
    pub handler: String,
    /// Whether this schedule is still active.
    pub active: bool,
}

// ---------------------------------------------------------------------------
// SQLite persistence helpers
// ---------------------------------------------------------------------------

/// How many days of finished job records to retain in `__clean_jobs`.
/// Records with `finished_at_ms < now - JOBS_RETENTION_DAYS * 86_400_000`
/// are deleted at startup.
const JOBS_RETENTION_DAYS: u64 = 7;

/// Ensure the `__clean_jobs` table and index exist.
pub async fn ensure_jobs_table(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS __clean_jobs (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            args_json       TEXT NOT NULL,
            status          TEXT NOT NULL,
            attempt         INTEGER NOT NULL DEFAULT 0,
            scheduled_at_ms INTEGER NOT NULL,
            started_at_ms   INTEGER,
            finished_at_ms  INTEGER,
            result_json     TEXT,
            error_message   TEXT,
            queue           TEXT NOT NULL DEFAULT 'default',
            created_at_ms   INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_jobs_status_scheduled
         ON __clean_jobs(status, scheduled_at_ms)",
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Persist a freshly created `JobRecord` to SQLite (INSERT).
async fn db_insert_job(pool: &sqlx::SqlitePool, r: &JobRecord) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO __clean_jobs
            (id, name, args_json, status, attempt, scheduled_at_ms, started_at_ms,
             finished_at_ms, result_json, error_message, queue, created_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, ?, ?)",
    )
    .bind(&r.id)
    .bind(&r.name)
    .bind(&r.args)
    .bind(r.status.as_str())
    .bind(r.attempt as i64)
    .bind(r.scheduled_at as i64)
    .bind(&r.queue)
    .bind(r.created_at as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Write the mutable fields of a `JobRecord` back to SQLite (UPDATE).
///
/// Called after every state transition (Pending→Running, Running→Succeeded, etc.).
async fn db_update_job(pool: &sqlx::SqlitePool, r: &JobRecord) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE __clean_jobs
         SET status          = ?,
             attempt         = ?,
             scheduled_at_ms = ?,
             started_at_ms   = CASE WHEN ? = 'running' THEN ? ELSE started_at_ms END,
             finished_at_ms  = ?,
             result_json     = ?,
             error_message   = ?
         WHERE id = ?",
    )
    .bind(r.status.as_str())
    .bind(r.attempt as i64)
    .bind(r.scheduled_at as i64)
    // started_at_ms: set to now_ms when transitioning to running, keep existing otherwise
    .bind(r.status.as_str())
    .bind(r.updated_at as i64)
    // finished_at_ms: None while pending/running, Some when terminal
    .bind(r.finished_at.map(|v| v as i64))
    .bind(r.result.as_deref())
    .bind(r.error.as_deref())
    .bind(&r.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Query a single row from `__clean_jobs` by id without loading it into memory.
///
/// Returns `(status_str, result_json_opt, error_message_opt)` or `None` when not found.
async fn db_query_job_by_id(
    pool: &sqlx::SqlitePool,
    job_id: &str,
) -> anyhow::Result<Option<(String, Option<String>, Option<String>)>> {
    let row = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT status, result_json, error_message FROM __clean_jobs WHERE id = ? LIMIT 1",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Delete finished rows older than `retention_days`.
///
/// Targets `succeeded`, `failed`, and `cancelled` rows whose `finished_at_ms`
/// is older than the retention window.  Rows with NULL `finished_at_ms` are
/// never deleted here (they are still active).
async fn db_cleanup_old_jobs(pool: &sqlx::SqlitePool, retention_days: u64) -> anyhow::Result<u64> {
    let cutoff = now_ms().saturating_sub(retention_days * 86_400_000);
    let result = sqlx::query(
        "DELETE FROM __clean_jobs
         WHERE status IN ('succeeded', 'failed', 'cancelled')
           AND finished_at_ms IS NOT NULL
           AND finished_at_ms < ?",
    )
    .bind(cutoff as i64)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// The shared jobs store
// ---------------------------------------------------------------------------

/// All in-memory job state — one instance per server process.
pub struct JobsStore {
    /// Registered job configurations keyed by job name.
    pub configs: HashMap<String, JobConfig>,
    /// Active job records keyed by job ID (pending + running only after recovery).
    /// Finished records (succeeded / failed / cancelled) are evicted from memory
    /// after completion; the DB is queried on-demand for their status/result.
    pub records: HashMap<String, JobRecord>,
    /// Registered cron schedules keyed by schedule name.
    pub schedules: HashMap<String, CronSchedule>,
    /// Optional SQLite pool for write-through persistence.
    /// `None` when the server is running without a SQLite database.
    pub sqlite_pool: Option<sqlx::SqlitePool>,
}

impl JobsStore {
    pub fn new() -> Self {
        Self {
            configs: HashMap::new(),
            records: HashMap::new(),
            schedules: HashMap::new(),
            sqlite_pool: None,
        }
    }

    /// Wire in a SQLite pool for persistence.  Called once during server startup,
    /// before the worker loop is started.
    pub fn set_sqlite_pool(&mut self, pool: sqlx::SqlitePool) {
        self.sqlite_pool = Some(pool);
    }
}

impl Default for JobsStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe handle to the shared jobs state.
pub type SharedJobsState = Arc<Mutex<JobsStore>>;

/// Create a new empty shared jobs state.
pub fn create_shared_jobs_state() -> SharedJobsState {
    Arc::new(Mutex::new(JobsStore::new()))
}

// ---------------------------------------------------------------------------
// Startup: schema creation, recovery, and cleanup
// ---------------------------------------------------------------------------

/// Initialise persistence for the jobs runtime.
///
/// Steps executed in order:
/// 1. Create `__clean_jobs` table and index if they don't exist.
/// 2. Delete finished rows older than `retention_days`.
/// 3. Load all `pending` and `running` rows into the in-memory cache.
///    Any `running` row (orphaned by a previous hard kill) is reset to `pending`.
///
/// Safe to call when `state` has no SQLite pool configured — all steps are
/// no-ops in that case.
pub async fn init_persistence(state: &SharedJobsState, retention_days: u64) {
    let pool = {
        let store = state.lock().await;
        store.sqlite_pool.clone()
    };

    let pool = match pool {
        Some(p) => p,
        None => {
            debug!("jobs: no SQLite pool configured — persistence disabled");
            return;
        }
    };

    // 1. Create table + index.
    if let Err(e) = ensure_jobs_table(&pool).await {
        error!("jobs: failed to create __clean_jobs table: {}", e);
        return;
    }
    debug!("jobs: __clean_jobs table verified");

    // 2. Delete stale finished rows.
    match db_cleanup_old_jobs(&pool, retention_days).await {
        Ok(0) => debug!("jobs: no stale records to clean up"),
        Ok(n) => info!(
            "jobs: cleaned up {} finished job record(s) older than {}d",
            n, retention_days
        ),
        Err(e) => warn!("jobs: cleanup of old records failed: {}", e),
    }

    // 3. Recover pending + running rows.
    recover_from_disk(state, &pool).await;
}

/// Load `pending` and `running` rows from SQLite into the in-memory cache.
///
/// `running` rows are reset to `pending` (the handler that was executing
/// was cut short by the process kill and must be retried).
async fn recover_from_disk(state: &SharedJobsState, pool: &sqlx::SqlitePool) {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            i64,
            i64,
            Option<String>,
            Option<String>,
            String,
            i64,
        ),
    >(
        "SELECT id, name, args_json, status, attempt, scheduled_at_ms,
                result_json, error_message, queue, created_at_ms
         FROM __clean_jobs
         WHERE status IN ('pending', 'running')",
    )
    .fetch_all(pool)
    .await;

    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            error!("jobs: recovery query failed: {}", e);
            return;
        }
    };

    let count = rows.len();
    let mut reset_count = 0usize;

    let mut store = state.lock().await;
    for (
        id,
        name,
        args,
        status_str,
        attempt,
        scheduled_at_ms,
        result_json,
        error_msg,
        queue,
        created_at_ms,
    ) in rows
    {
        let was_running = status_str == "running";
        let status = if was_running {
            reset_count += 1;
            JobStatus::Pending
        } else {
            JobStatus::Pending
        };

        let record = JobRecord {
            id: id.clone(),
            name,
            args,
            status,
            attempt: attempt as u32,
            max_attempts: 3, // default; will be overwritten when the job config is re-registered
            backoff: BackoffStrategy::Fixed,
            delay_ms: 1000,
            timeout_ms: 0,
            queue,
            handler: String::new(), // handler is resolved from the registered config at dispatch time
            scheduled_at: scheduled_at_ms as u64,
            created_at: created_at_ms as u64,
            updated_at: now_ms(),
            finished_at: None,
            result: result_json,
            error: error_msg,
        };
        store.records.insert(id, record);
    }

    if was_running_reset_needed(reset_count) {
        // Flush the status reset to disk so if we crash again the rows
        // are still pending rather than running.
        let pool_clone = pool.clone();
        drop(store); // release lock before async work

        if let Err(e) =
            sqlx::query("UPDATE __clean_jobs SET status = 'pending' WHERE status = 'running'")
                .execute(&pool_clone)
                .await
        {
            warn!(
                "jobs: failed to reset orphaned running rows to pending: {}",
                e
            );
        }
    } else {
        drop(store);
    }

    if count > 0 {
        info!(
            "jobs: recovered {} job record(s) from disk ({} reset from running→pending)",
            count, reset_count
        );
    }
}

/// Public entry point for recovery used in integration tests.
///
/// Calls the same internal recovery logic as the startup path, but accepts the
/// pool as a direct parameter so tests can pass an in-memory pool without
/// attaching it to the `JobsStore`.
pub async fn recover_from_disk_with_pool(state: &SharedJobsState, pool: &sqlx::SqlitePool) {
    recover_from_disk(state, pool).await;
}

/// Helper to make the borrow-checker happy around the conditional pool flush.
#[inline(always)]
fn was_running_reset_needed(reset_count: usize) -> bool {
    reset_count > 0
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Current Unix timestamp in milliseconds.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Public API used by bridge functions
// ---------------------------------------------------------------------------

/// Register a job handler with its retry configuration.
///
/// Silently replaces any existing config for the same name.
#[allow(clippy::too_many_arguments)]
pub async fn register_job(
    state: &SharedJobsState,
    name: String,
    handler: String,
    max_attempts: u32,
    backoff: BackoffStrategy,
    delay_ms: u64,
    timeout_ms: u64,
    queue: String,
) {
    let config = JobConfig {
        name: name.clone(),
        handler,
        max_attempts: max_attempts.max(1),
        backoff,
        delay_ms,
        timeout_ms,
        queue,
    };

    // Re-apply the config to any recovered records for this job name so their
    // max_attempts / backoff / handler reflect the current registration.
    let mut store = state.lock().await;
    for r in store.records.values_mut() {
        if r.name == name {
            r.max_attempts = config.max_attempts;
            r.backoff = config.backoff.clone();
            r.delay_ms = config.delay_ms;
            r.timeout_ms = config.timeout_ms;
            r.handler = config.handler.clone();
        }
    }
    store.configs.insert(name.clone(), config);
    debug!("job.register: registered handler for '{}'", name);
}

/// Enqueue a job for immediate execution.
///
/// Returns the generated UUID v4 job ID, or an empty string if the job name
/// is not registered.
pub async fn enqueue_job(state: &SharedJobsState, name: String, args: String) -> String {
    enqueue_job_at(state, name, args, now_ms()).await
}

/// Enqueue a job scheduled to run at a specific future Unix millisecond timestamp.
///
/// Returns the generated UUID v4 job ID, or an empty string if the job name
/// is not registered.
pub async fn enqueue_job_at(
    state: &SharedJobsState,
    name: String,
    args: String,
    run_at_ms: u64,
) -> String {
    let (record, pool) = {
        let mut store = state.lock().await;

        let config = match store.configs.get(&name) {
            Some(c) => c.clone(),
            None => {
                warn!("job.enqueue: unknown job name '{}' — not registered", name);
                return String::new();
            }
        };

        let id = Uuid::new_v4().to_string();
        let ts = now_ms();

        let record = JobRecord {
            id: id.clone(),
            name: name.clone(),
            args,
            status: JobStatus::Pending,
            attempt: 0,
            max_attempts: config.max_attempts,
            backoff: config.backoff,
            delay_ms: config.delay_ms,
            timeout_ms: config.timeout_ms,
            queue: config.queue,
            handler: config.handler,
            scheduled_at: run_at_ms,
            created_at: ts,
            updated_at: ts,
            finished_at: None,
            result: None,
            error: None,
        };

        let pool = store.sqlite_pool.clone();
        store.records.insert(id.clone(), record.clone());
        (record, pool)
    };

    // Write-through to SQLite outside the lock.
    if let Some(ref p) = pool
        && let Err(e) = db_insert_job(p, &record).await
    {
        warn!("jobs: failed to persist enqueued job {}: {}", record.id, e);
    }

    debug!(
        "job.enqueue: enqueued '{}' as {} (scheduled_at={})",
        name, record.id, run_at_ms
    );
    record.id
}

/// Cancel a pending job.
///
/// Returns `true` if the job existed and was in `Pending` state.
/// Running, succeeded, failed, or cancelled jobs cannot be cancelled.
pub async fn cancel_job(state: &SharedJobsState, job_id: &str) -> bool {
    let (updated_opt, was_pending) = {
        let mut store = state.lock().await;
        if let Some(record) = store.records.get_mut(job_id) {
            if record.status == JobStatus::Pending {
                record.status = JobStatus::Cancelled;
                record.updated_at = now_ms();
                record.finished_at = Some(now_ms());
                let r = record.clone();
                let p = store.sqlite_pool.clone();
                (Some((r, p)), true)
            } else {
                (None, false)
            }
        } else {
            (None, false)
        }
    };

    if was_pending {
        debug!("job.cancel: {} cancelled", job_id);
        if let Some((record, Some(ref p))) = updated_opt
            && let Err(e) = db_update_job(p, &record).await
        {
            warn!("jobs: failed to persist cancellation of {}: {}", job_id, e);
        }
        // Evict the terminal record from memory to keep the cache lean.
        state.lock().await.records.remove(job_id);
        return true;
    }
    false
}

/// Return the current status string for a job ID.
///
/// Lookup order:
/// 1. In-memory cache (pending / running records are always here).
/// 2. SQLite fallback for finished records that have been evicted from memory.
///
/// Returns `"not_found"` if the ID is unknown in both locations.
pub async fn job_status(state: &SharedJobsState, job_id: &str) -> String {
    let (cached, pool) = {
        let store = state.lock().await;
        let cached = store
            .records
            .get(job_id)
            .map(|r| r.status.as_str().to_string());
        (cached, store.sqlite_pool.clone())
    };

    if let Some(status) = cached {
        return status;
    }

    // Fallback to DB.
    if let Some(ref p) = pool {
        match db_query_job_by_id(p, job_id).await {
            Ok(Some((status, _, _))) => return status,
            Ok(None) => {}
            Err(e) => warn!("jobs: DB lookup for status of {} failed: {}", job_id, e),
        }
    }

    "not_found".to_string()
}

/// Return the result or error string for a job.
///
/// - Succeeded → result JSON (or empty string for implicit success).
/// - Failed     → error message.
/// - Otherwise  → empty string.
///
/// Checks in-memory cache first, then falls back to SQLite for evicted records.
pub async fn job_result(state: &SharedJobsState, job_id: &str) -> String {
    let (cached, pool) = {
        let store = state.lock().await;
        let cached = store.records.get(job_id).map(|r| match r.status {
            JobStatus::Succeeded => r.result.clone().unwrap_or_default(),
            JobStatus::Failed => r.error.clone().unwrap_or_default(),
            _ => String::new(),
        });
        (cached, store.sqlite_pool.clone())
    };

    if let Some(result) = cached {
        return result;
    }

    // Fallback to DB for terminal records that were evicted from the cache.
    if let Some(ref p) = pool {
        match db_query_job_by_id(p, job_id).await {
            Ok(Some((status, result_json, error_msg))) => {
                return match status.as_str() {
                    "succeeded" => result_json.unwrap_or_default(),
                    "failed" => error_msg.unwrap_or_default(),
                    _ => String::new(),
                };
            }
            Ok(None) => {}
            Err(e) => warn!("jobs: DB lookup for result of {} failed: {}", job_id, e),
        }
    }

    String::new()
}

/// Register a named cron schedule.
///
/// Returns the registration ID (per the `_schedule_cron` contract in
/// function-registry.toml), or `None` if the name is empty or the cron
/// expression is unparseable. Registering an existing name replaces the
/// previous schedule and issues a new ID.
pub async fn schedule_cron(
    state: &SharedJobsState,
    name: String,
    cron_expr: String,
    handler: String,
) -> Option<String> {
    if name.is_empty() {
        warn!("schedule.cron: schedule name must not be empty");
        return None;
    }
    if !is_valid_cron(&cron_expr) {
        warn!(
            "schedule.cron: invalid cron expression '{}' for schedule '{}'",
            cron_expr, name
        );
        return None;
    }

    let id = format!("sched_{}", Uuid::new_v4().simple());
    let schedule = CronSchedule {
        name: name.clone(),
        id: id.clone(),
        cron_expr,
        handler,
        active: true,
    };
    state.lock().await.schedules.insert(name.clone(), schedule);
    debug!("schedule.cron: registered '{}' as {}", name, id);
    Some(id)
}

/// Cancel a cron schedule by marking it inactive.
///
/// `key` is either the schedule name or the registration ID returned by
/// [`schedule_cron`]. Returns `true` if the schedule existed and was active.
pub async fn schedule_cancel(state: &SharedJobsState, key: &str) -> bool {
    let mut store = state.lock().await;
    let sched = if store.schedules.contains_key(key) {
        store.schedules.get_mut(key)
    } else {
        store.schedules.values_mut().find(|s| s.id == key)
    };
    if let Some(sched) = sched
        && sched.active
    {
        sched.active = false;
        debug!("schedule.cancel: '{}' deactivated", sched.name);
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Cron expression parsing and next-tick calculation
// ---------------------------------------------------------------------------

/// Validate that a 5-field cron expression can be parsed.
fn is_valid_cron(expr: &str) -> bool {
    parse_cron_fields(expr).is_some()
}

/// A parsed single cron field.
enum CronField {
    /// `*` — matches every value.
    Star,
    /// `*/n` — matches values divisible by n.
    Step(u32),
    /// Explicit set of values (from comma lists and ranges).
    List(Vec<u32>),
}

impl CronField {
    fn matches(&self, value: u32) -> bool {
        match self {
            CronField::Star => true,
            CronField::Step(n) => *n > 0 && value.is_multiple_of(*n),
            CronField::List(vals) => vals.contains(&value),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        if s == "*" {
            return Some(CronField::Star);
        }
        if let Some(step_str) = s.strip_prefix("*/") {
            let n: u32 = step_str.parse().ok()?;
            if n == 0 {
                return None;
            }
            return Some(CronField::Step(n));
        }
        // Comma-separated list of values and/or ranges.
        let mut values: Vec<u32> = Vec::new();
        for part in s.split(',') {
            if let Some((start_s, end_s)) = part.split_once('-') {
                let a: u32 = start_s.trim().parse().ok()?;
                let b: u32 = end_s.trim().parse().ok()?;
                if a > b {
                    return None;
                }
                for v in a..=b {
                    values.push(v);
                }
            } else {
                let v: u32 = part.trim().parse().ok()?;
                values.push(v);
            }
        }
        if values.is_empty() {
            return None;
        }
        Some(CronField::List(values))
    }
}

/// Parsed 5-field cron expression.
struct CronFields {
    minute: CronField, // 0–59
    hour: CronField,   // 0–23
    dom: CronField,    // 1–31
    month: CronField,  // 1–12
    dow: CronField,    // 0–6 (Sunday=0)
}

fn parse_cron_fields(expr: &str) -> Option<CronFields> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    Some(CronFields {
        minute: CronField::parse(parts[0])?,
        hour: CronField::parse(parts[1])?,
        dom: CronField::parse(parts[2])?,
        month: CronField::parse(parts[3])?,
        dow: CronField::parse(parts[4])?,
    })
}

/// Compute how long until the next tick of a 5-field cron expression.
///
/// Scans forward minute-by-minute from now.  Returns `None` only if no match
/// is found within one year (guards against impossible expressions like
/// `0 0 31 2 *` — Feb 31st never fires).
pub fn next_cron_tick(expr: &str) -> Option<Duration> {
    use chrono::{Datelike, Timelike, Utc};

    let fields = parse_cron_fields(expr)?;
    let now = Utc::now();

    // Start from the next whole minute.
    let base = now.with_second(0).and_then(|t| t.with_nanosecond(0))?;
    let mut candidate = base + chrono::Duration::minutes(1);

    for _ in 0..(366 * 24 * 60) {
        let m = candidate.minute();
        let h = candidate.hour();
        let d = candidate.day();
        let mo = candidate.month();
        let wd = candidate.weekday().num_days_from_sunday();

        if fields.minute.matches(m)
            && fields.hour.matches(h)
            && fields.dom.matches(d)
            && fields.month.matches(mo)
            && fields.dow.matches(wd)
        {
            let delta = candidate.signed_duration_since(now);
            let secs = delta.num_seconds().max(0) as u64;
            return Some(Duration::from_secs(secs));
        }

        candidate += chrono::Duration::minutes(1);
    }

    None
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

/// Worker poll interval — check for due jobs every second.
const WORKER_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How often the worker emits a heartbeat log line when jobs are tracked.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum jobs picked up per poll cycle.
const MAX_JOBS_PER_POLL: usize = 20;

/// Start the background worker loop as a detached tokio task.
///
/// Accepts an optional `SharedDbBridge` so it can extract a `SqlitePool` and
/// wire it into the `JobsStore` before the first poll.  The bridge must be
/// fully configured (i.e. `db_bridge.configure(...)` has already been called)
/// before this function is invoked — `server.rs` guarantees this by calling
/// `configure_db_bridge` before `start_worker_loop`.
pub fn start_worker_loop(
    state: SharedJobsState,
    wasm: Arc<WasmInstance>,
    db_bridge: Option<crate::wasm::SharedDbBridge>,
) {
    tokio::spawn(async move {
        // Wire in the SQLite pool from the DbBridge (if one is configured).
        if let Some(bridge) = db_bridge {
            let bridge_read = bridge.read().await;
            if let Some(pool) = bridge_read.get_sqlite_pool().await {
                state.lock().await.set_sqlite_pool(pool);
                debug!("jobs: SQLite persistence wired in from DbBridge");
            } else {
                debug!("jobs: DbBridge is not SQLite — persistence disabled");
            }
        }

        // Initialise schema, run cleanup, and recover pending/running records.
        init_persistence(&state, JOBS_RETENTION_DAYS).await;

        info!("Job worker loop started");
        let mut last_heartbeat = Instant::now();

        loop {
            tokio::time::sleep(WORKER_POLL_INTERVAL).await;

            // Heartbeat every 60 seconds while there are tracked records.
            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                let count = state.lock().await.records.len();
                if count > 0 {
                    info!("Job worker heartbeat: {} total job records in store", count);
                }
                last_heartbeat = Instant::now();
            }

            // Collect due pending jobs (up to MAX_JOBS_PER_POLL).
            let due_jobs: Vec<JobRecord> = {
                let store = state.lock().await;
                let now = now_ms();
                store
                    .records
                    .values()
                    .filter(|r| r.status == JobStatus::Pending && r.scheduled_at <= now)
                    .take(MAX_JOBS_PER_POLL)
                    .cloned()
                    .collect()
            };

            for job in due_jobs {
                // Atomically claim the job: Pending → Running.
                let (claimed, pool) = {
                    let mut store = state.lock().await;
                    if let Some(r) = store.records.get_mut(&job.id) {
                        if r.status == JobStatus::Pending {
                            r.status = JobStatus::Running;
                            r.attempt += 1;
                            r.updated_at = now_ms();
                            (true, store.sqlite_pool.clone())
                        } else {
                            (false, None)
                        }
                    } else {
                        (false, None)
                    }
                };

                if !claimed {
                    debug!("job {}: skipped (already claimed)", job.id);
                    continue;
                }

                // Persist the Running status.
                if let Some(ref p) = pool {
                    let updated = state.lock().await.records.get(&job.id).cloned();
                    if let Some(ref r) = updated
                        && let Err(e) = db_update_job(p, r).await
                    {
                        warn!(
                            "jobs: failed to persist running status for {}: {}",
                            job.id, e
                        );
                    }
                }

                let attempt = state
                    .lock()
                    .await
                    .records
                    .get(&job.id)
                    .map(|r| r.attempt)
                    .unwrap_or(1);

                debug!(
                    "job {}: starting attempt {}/{}",
                    job.id, attempt, job.max_attempts
                );

                // Execute the WASM handler in spawn_blocking with task-local context.
                let wasm_clone = wasm.clone();
                let handler_name = job.handler.clone();
                let job_id_clone = job.id.clone();
                let job_args_clone = job.args.clone();
                let attempt_i32 = attempt as i32;

                let call_result = tokio::task::spawn_blocking(move || {
                    JOB_CURRENT_ID.sync_scope(job_id_clone, || {
                        JOB_CURRENT_ARGS.sync_scope(job_args_clone, || {
                            JOB_CURRENT_ATTEMPT.sync_scope(attempt_i32, || {
                                JOB_RETRY_OVERRIDE_MS.sync_scope(
                                    std::cell::Cell::new(-1i64),
                                    || {
                                        JOB_EXPLICIT_FAIL.sync_scope(
                                            std::cell::RefCell::new(None),
                                            || {
                                                JOB_EXPLICIT_SUCCEED.sync_scope(
                                                    std::cell::RefCell::new(None),
                                                    || {
                                                        let req = RequestContext {
                                                            method: "JOB".to_string(),
                                                            path: String::new(),
                                                            headers: Vec::new(),
                                                            body: String::new(),
                                                            body_bytes: None,
                                                            params: std::collections::HashMap::new(
                                                            ),
                                                            query: std::collections::HashMap::new(),
                                                        };
                                                        let handler_result = wasm_clone
                                                            .call_handler_job(
                                                                &handler_name,
                                                                req,
                                                                None,
                                                            );

                                                        let retry_override = JOB_RETRY_OVERRIDE_MS
                                                            .try_with(|c| c.get())
                                                            .unwrap_or(-1);
                                                        let explicit_fail = JOB_EXPLICIT_FAIL
                                                            .try_with(|r| r.borrow().clone())
                                                            .unwrap_or(None);
                                                        let explicit_succeed = JOB_EXPLICIT_SUCCEED
                                                            .try_with(|r| r.borrow().clone())
                                                            .unwrap_or(None);

                                                        JobOutcome {
                                                            handler_result,
                                                            retry_override_ms: retry_override,
                                                            explicit_fail,
                                                            explicit_succeed,
                                                        }
                                                    },
                                                )
                                            },
                                        )
                                    },
                                )
                            })
                        })
                    })
                })
                .await;

                match call_result {
                    Err(join_err) => {
                        let err_msg = format!("Job handler task panicked: {}", join_err);
                        error!("job {}: {}", job.id, err_msg);
                        apply_failure(&state, &job, attempt, err_msg).await;
                    }
                    Ok(outcome) => {
                        apply_outcome(&state, &job, attempt, outcome).await;
                    }
                }
            }
        }
    });
}

/// The bundled outcome of a single job handler invocation.
struct JobOutcome {
    /// What the WASM handler returned.
    handler_result: Result<(), crate::error::RuntimeError>,
    /// Milliseconds requested by `_job_retry_after` (-1 = not set).
    retry_override_ms: i64,
    /// Reason if `_job_fail` was called.
    explicit_fail: Option<String>,
    /// Result JSON if `_job_succeed` was called.
    explicit_succeed: Option<String>,
}

/// Apply the outcome of a job handler invocation to the job store.
async fn apply_outcome(
    state: &SharedJobsState,
    job: &JobRecord,
    attempt: u32,
    outcome: JobOutcome,
) {
    // _job_succeed takes highest priority regardless of handler error.
    if let Some(result_json) = outcome.explicit_succeed {
        let (updated, pool) = {
            let mut store = state.lock().await;
            let pool = store.sqlite_pool.clone();
            if let Some(r) = store.records.get_mut(&job.id) {
                r.status = JobStatus::Succeeded;
                r.result = Some(result_json);
                r.updated_at = now_ms();
                r.finished_at = Some(now_ms());
                (Some(r.clone()), pool)
            } else {
                (None, pool)
            }
        };
        info!("job {}: succeeded (explicit, attempt {})", job.id, attempt);
        if let (Some(r), Some(p)) = (updated, pool) {
            persist_and_evict(state, &p, r).await;
        }
        return;
    }

    // _job_fail forces immediate failure (no more retries).
    if let Some(reason) = outcome.explicit_fail {
        let (updated, pool) = {
            let mut store = state.lock().await;
            let pool = store.sqlite_pool.clone();
            if let Some(r) = store.records.get_mut(&job.id) {
                r.status = JobStatus::Failed;
                r.error = Some(reason.clone());
                r.updated_at = now_ms();
                r.finished_at = Some(now_ms());
                (Some(r.clone()), pool)
            } else {
                (None, pool)
            }
        };
        warn!(
            "job {}: explicitly failed (attempt {}): {}",
            job.id, attempt, reason
        );
        if let (Some(r), Some(p)) = (updated, pool) {
            persist_and_evict(state, &p, r).await;
        }
        return;
    }

    match outcome.handler_result {
        Ok(()) => {
            let (updated, pool) = {
                let mut store = state.lock().await;
                let pool = store.sqlite_pool.clone();
                if let Some(r) = store.records.get_mut(&job.id) {
                    r.status = JobStatus::Succeeded;
                    r.updated_at = now_ms();
                    r.finished_at = Some(now_ms());
                    (Some(r.clone()), pool)
                } else {
                    (None, pool)
                }
            };
            info!("job {}: succeeded (implicit, attempt {})", job.id, attempt);
            if let (Some(r), Some(p)) = (updated, pool) {
                persist_and_evict(state, &p, r).await;
            }
        }
        Err(e) => {
            apply_failure_with_retry_override(
                state,
                job,
                attempt,
                e.to_string(),
                outcome.retry_override_ms,
            )
            .await;
        }
    }
}

/// Write a terminal (succeeded / failed / cancelled) record to SQLite and then
/// evict it from the in-memory cache to keep memory usage bounded.
///
/// Eviction is safe because `job_status` and `job_result` fall back to the DB
/// when the id is absent from the cache.
async fn persist_and_evict(state: &SharedJobsState, pool: &sqlx::SqlitePool, record: JobRecord) {
    if let Err(e) = db_update_job(pool, &record).await {
        warn!(
            "jobs: failed to persist terminal status for {}: {}",
            record.id, e
        );
    }
    // Evict the record from the hot cache.
    state.lock().await.records.remove(&record.id);
}

/// Apply failure logic: retry if attempts remain, otherwise mark permanently failed.
async fn apply_failure(state: &SharedJobsState, job: &JobRecord, attempt: u32, err_msg: String) {
    apply_failure_with_retry_override(state, job, attempt, err_msg, -1).await;
}

async fn apply_failure_with_retry_override(
    state: &SharedJobsState,
    job: &JobRecord,
    attempt: u32,
    err_msg: String,
    retry_override_ms: i64,
) {
    if attempt < job.max_attempts {
        let next_delay = if retry_override_ms >= 0 {
            retry_override_ms as u64
        } else {
            job.backoff.compute_delay(job.delay_ms, attempt)
        };

        let next_scheduled = now_ms() + next_delay;

        let (updated, pool) = {
            let mut store = state.lock().await;
            let pool = store.sqlite_pool.clone();
            if let Some(r) = store.records.get_mut(&job.id) {
                r.status = JobStatus::Pending;
                r.scheduled_at = next_scheduled;
                r.error = Some(err_msg.clone());
                r.updated_at = now_ms();
                (Some(r.clone()), pool)
            } else {
                (None, pool)
            }
        };

        warn!(
            "job {}: attempt {} failed, retry in {}ms: {}",
            job.id, attempt, next_delay, err_msg
        );

        if let (Some(r), Some(p)) = (updated, pool)
            && let Err(e) = db_update_job(&p, &r).await
        {
            warn!(
                "jobs: failed to persist retry schedule for {}: {}",
                job.id, e
            );
        }
    } else {
        let (updated, pool) = {
            let mut store = state.lock().await;
            let pool = store.sqlite_pool.clone();
            if let Some(r) = store.records.get_mut(&job.id) {
                r.status = JobStatus::Failed;
                r.error = Some(err_msg.clone());
                r.updated_at = now_ms();
                r.finished_at = Some(now_ms());
                (Some(r.clone()), pool)
            } else {
                (None, pool)
            }
        };

        error!(
            "job {}: permanently failed after {} attempt(s): {}",
            job.id, attempt, err_msg
        );

        if let (Some(r), Some(p)) = (updated, pool) {
            persist_and_evict(state, &p, r).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Cron scheduler
// ---------------------------------------------------------------------------

/// Start the cron scheduler monitor.
///
/// A monitor task polls the schedule registry every 5 seconds.  When it
/// discovers a new active schedule it spawns a dedicated per-schedule task.
/// Each per-schedule task computes the next tick, sleeps until then, fires the
/// WASM handler, then loops.  When a schedule is cancelled (active = false)
/// the task exits cleanly after its next wake-up.
///
/// Cron schedules are declarative — they are re-registered on every server
/// start via `_schedule_cron` calls.  No persistence is needed.
pub fn start_cron_scheduler(state: SharedJobsState, wasm: Arc<WasmInstance>) {
    tokio::spawn(async move {
        info!("Cron scheduler monitor started");

        let mut running: std::collections::HashSet<String> = std::collections::HashSet::new();

        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;

            // Collect active schedules that don't have a running task yet.
            let new_schedules: Vec<CronSchedule> = {
                let store = state.lock().await;
                store
                    .schedules
                    .values()
                    .filter(|s| s.active && !running.contains(&s.name))
                    .cloned()
                    .collect()
            };

            for sched in new_schedules {
                running.insert(sched.name.clone());
                let state_clone = state.clone();
                let wasm_clone = wasm.clone();

                tokio::spawn(async move {
                    info!(
                        "cron '{}': task started (expr: '{}')",
                        sched.name, sched.cron_expr
                    );

                    loop {
                        let still_active = state_clone
                            .lock()
                            .await
                            .schedules
                            .get(&sched.name)
                            .map(|s| s.active)
                            .unwrap_or(false);

                        if !still_active {
                            info!("cron '{}': deactivated, task exiting", sched.name);
                            break;
                        }

                        let wait = match next_cron_tick(&sched.cron_expr) {
                            Some(d) => d,
                            None => {
                                warn!(
                                    "cron '{}': cannot compute next tick for '{}', deactivating",
                                    sched.name, sched.cron_expr
                                );
                                break;
                            }
                        };

                        debug!(
                            "cron '{}': next tick in {:.1}s",
                            sched.name,
                            wait.as_secs_f64()
                        );

                        tokio::time::sleep(wait).await;

                        let still_active = state_clone
                            .lock()
                            .await
                            .schedules
                            .get(&sched.name)
                            .map(|s| s.active)
                            .unwrap_or(false);

                        if !still_active {
                            info!(
                                "cron '{}': cancelled during sleep, task exiting",
                                sched.name
                            );
                            break;
                        }

                        info!("cron '{}': firing handler '{}'", sched.name, sched.handler);

                        let wasm_fire = wasm_clone.clone();
                        let h_name = sched.handler.clone();
                        let sched_name = sched.name.clone();

                        let result = tokio::task::spawn_blocking(move || {
                            let req = RequestContext {
                                method: "CRON".to_string(),
                                path: sched_name,
                                headers: Vec::new(),
                                body: String::new(),
                                body_bytes: None,
                                params: std::collections::HashMap::new(),
                                query: std::collections::HashMap::new(),
                            };
                            wasm_fire.call_handler_job(&h_name, req, None)
                        })
                        .await;

                        match result {
                            Ok(Ok(())) => {
                                debug!("cron '{}': handler completed", sched.name);
                            }
                            Ok(Err(e)) => {
                                warn!("cron '{}': handler error: {}", sched.name, e);
                            }
                            Err(join_err) => {
                                warn!("cron '{}': handler panicked: {}", sched.name, join_err);
                            }
                        }
                    }
                });
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[test]
    fn job_bridge_seconds_convert_to_internal_milliseconds() {
        assert_eq!(super::seconds_to_ms(30.0), 30_000);
        assert_eq!(super::seconds_to_ms(1.5), 1_500);
        assert_eq!(super::seconds_to_ms(1_767_225_600.0), 1_767_225_600_000);
        assert_eq!(super::seconds_to_ms(-5.0), 0);
        assert_eq!(super::seconds_to_ms(f64::NAN), 0);
    }

    use super::*;

    #[tokio::test]
    async fn test_register_and_enqueue() {
        let state = create_shared_jobs_state();

        register_job(
            &state,
            "sendWelcomeEmail".to_string(),
            "sendWelcomeEmail_handler".to_string(),
            3,
            BackoffStrategy::Exponential,
            1000,
            0,
            "default".to_string(),
        )
        .await;

        let id = enqueue_job(
            &state,
            "sendWelcomeEmail".to_string(),
            r#"{"email":"test@example.com"}"#.to_string(),
        )
        .await;

        assert!(
            !id.is_empty(),
            "enqueue should return a non-empty UUID job ID"
        );

        let status = job_status(&state, &id).await;
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn test_enqueue_unknown_job_returns_empty() {
        let state = create_shared_jobs_state();
        let id = enqueue_job(&state, "nonexistent".to_string(), "{}".to_string()).await;
        assert!(id.is_empty(), "unknown job type should return empty string");
    }

    #[tokio::test]
    async fn test_cancel_pending_job() {
        let state = create_shared_jobs_state();

        register_job(
            &state,
            "cancelMe".to_string(),
            "cancelMe_handler".to_string(),
            1,
            BackoffStrategy::Fixed,
            0,
            0,
            "default".to_string(),
        )
        .await;

        let id = enqueue_job(&state, "cancelMe".to_string(), "{}".to_string()).await;
        assert!(!id.is_empty());

        let cancelled = cancel_job(&state, &id).await;
        assert!(cancelled, "cancel should succeed for a pending job");

        // After cancellation the record is evicted from the in-memory cache; status
        // falls back to the DB.  Without a DB wired in the fallback returns not_found,
        // which is the correct behaviour for the pure in-memory test path.
        let status = job_status(&state, &id).await;
        assert!(
            status == "cancelled" || status == "not_found",
            "expected cancelled or not_found, got {}",
            status
        );

        // Second cancel should return false regardless.
        let cancelled_again = cancel_job(&state, &id).await;
        assert!(!cancelled_again, "second cancel of same job should fail");
    }

    #[tokio::test]
    async fn test_cancel_nonexistent_returns_false() {
        let state = create_shared_jobs_state();
        let cancelled = cancel_job(&state, "00000000-0000-0000-0000-000000000000").await;
        assert!(!cancelled);
    }

    #[tokio::test]
    async fn test_job_result_pending_is_empty() {
        let state = create_shared_jobs_state();

        register_job(
            &state,
            "resultTest".to_string(),
            "resultTest_handler".to_string(),
            1,
            BackoffStrategy::Fixed,
            0,
            0,
            "default".to_string(),
        )
        .await;

        let id = enqueue_job(&state, "resultTest".to_string(), "{}".to_string()).await;
        let result = job_result(&state, &id).await;
        assert_eq!(result, "", "pending job should return empty result");
    }

    #[tokio::test]
    async fn test_enqueue_at_sets_future_schedule() {
        let state = create_shared_jobs_state();

        register_job(
            &state,
            "futureTask".to_string(),
            "futureTask_handler".to_string(),
            1,
            BackoffStrategy::Fixed,
            0,
            0,
            "default".to_string(),
        )
        .await;

        let future_ms = now_ms() + 10_000;
        let id = enqueue_job_at(
            &state,
            "futureTask".to_string(),
            "{}".to_string(),
            future_ms,
        )
        .await;

        assert!(!id.is_empty());

        let store = state.lock().await;
        let record = store.records.get(&id).unwrap();
        assert_eq!(record.status, JobStatus::Pending);
        assert!(record.scheduled_at > now_ms());
    }

    #[test]
    fn test_backoff_fixed() {
        let b = BackoffStrategy::Fixed;
        assert_eq!(b.compute_delay(1000, 1), 1000);
        assert_eq!(b.compute_delay(1000, 2), 1000);
        assert_eq!(b.compute_delay(1000, 5), 1000);
    }

    #[test]
    fn test_backoff_exponential() {
        let b = BackoffStrategy::Exponential;
        assert_eq!(b.compute_delay(1000, 1), 1000); // 1000 * 2^0
        assert_eq!(b.compute_delay(1000, 2), 2000); // 1000 * 2^1
        assert_eq!(b.compute_delay(1000, 3), 4000); // 1000 * 2^2
        assert_eq!(b.compute_delay(1000, 4), 8000); // 1000 * 2^3
    }

    #[test]
    fn test_backoff_strategy_parse() {
        assert_eq!(
            BackoffStrategy::parse("exponential"),
            BackoffStrategy::Exponential
        );
        assert_eq!(
            BackoffStrategy::parse("EXPONENTIAL"),
            BackoffStrategy::Exponential
        );
        assert_eq!(BackoffStrategy::parse("fixed"), BackoffStrategy::Fixed);
        assert_eq!(BackoffStrategy::parse("linear"), BackoffStrategy::Fixed);
        assert_eq!(BackoffStrategy::parse("unknown"), BackoffStrategy::Fixed);
    }

    #[test]
    fn test_cron_validation_valid() {
        assert!(is_valid_cron("* * * * *"));
        assert!(is_valid_cron("*/5 * * * *"));
        assert!(is_valid_cron("0 0 * * *"));
        assert!(is_valid_cron("*/1 * * * *"));
        assert!(is_valid_cron("0 9 * * 1-5"));
        assert!(is_valid_cron("0,30 * * * *"));
    }

    #[test]
    fn test_cron_validation_invalid() {
        assert!(!is_valid_cron("invalid"));
        assert!(!is_valid_cron("* * * *")); // too few fields
        assert!(!is_valid_cron("* * * * * *")); // too many fields
        assert!(!is_valid_cron("*/0 * * * *")); // zero step
        assert!(!is_valid_cron(""));
    }

    #[tokio::test]
    async fn test_schedule_cron_and_cancel() {
        let state = create_shared_jobs_state();

        let id = schedule_cron(
            &state,
            "dailyDigest".to_string(),
            "0 0 * * *".to_string(),
            "daily_digest_handler".to_string(),
        )
        .await;
        assert!(
            id.as_deref().is_some_and(|id| !id.is_empty()),
            "valid cron expression should register and return an ID"
        );

        let cancelled = schedule_cancel(&state, "dailyDigest").await;
        assert!(cancelled, "should cancel an active schedule");

        let cancelled_again = schedule_cancel(&state, "dailyDigest").await;
        assert!(!cancelled_again, "second cancel should fail");
    }

    #[tokio::test]
    async fn test_schedule_cancel_by_registration_id() {
        let state = create_shared_jobs_state();
        let id = schedule_cron(
            &state,
            "hourly".to_string(),
            "0 * * * *".to_string(),
            "hourly_handler".to_string(),
        )
        .await
        .expect("valid cron expression should register");

        assert!(
            schedule_cancel(&state, &id).await,
            "cancel by ID should succeed"
        );
        assert!(
            !schedule_cancel(&state, &id).await,
            "second cancel should fail"
        );
        assert!(
            !schedule_cancel(&state, "sched_unknown").await,
            "unknown ID should not cancel anything"
        );
    }

    #[tokio::test]
    async fn test_schedule_invalid_cron_returns_false() {
        let state = create_shared_jobs_state();
        let id = schedule_cron(
            &state,
            "bad".to_string(),
            "not a cron".to_string(),
            "handler".to_string(),
        )
        .await;
        assert!(id.is_none(), "invalid cron expression should not register");
    }

    #[test]
    fn test_task_local_defaults_outside_handler() {
        assert_eq!(current_job_id(), "");
        assert_eq!(current_job_args(), "");
        assert_eq!(current_job_attempt(), 0);
    }

    #[test]
    fn test_status_strings() {
        assert_eq!(JobStatus::Pending.as_str(), "pending");
        assert_eq!(JobStatus::Running.as_str(), "running");
        assert_eq!(JobStatus::Succeeded.as_str(), "succeeded");
        assert_eq!(JobStatus::Failed.as_str(), "failed");
        assert_eq!(JobStatus::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn test_status_from_str() {
        assert_eq!(JobStatus::parse_status("pending"), Some(JobStatus::Pending));
        assert_eq!(JobStatus::parse_status("running"), Some(JobStatus::Running));
        assert_eq!(
            JobStatus::parse_status("succeeded"),
            Some(JobStatus::Succeeded)
        );
        assert_eq!(JobStatus::parse_status("failed"), Some(JobStatus::Failed));
        assert_eq!(
            JobStatus::parse_status("cancelled"),
            Some(JobStatus::Cancelled)
        );
        assert_eq!(JobStatus::parse_status("unknown"), None);
    }
}
