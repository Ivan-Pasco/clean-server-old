//! Clean Server CLI
//!
//! Command-line interface for running compiled Clean Language applications
//! and managing the local diagnostic reports that the runtime emits when
//! WASM module loading fails.
//!
//! # Usage
//!
//! ```bash
//! # Run a WASM application (backward-compatible positional form)
//! clean-server app.wasm
//!
//! # Run with custom port
//! clean-server app.wasm --port 8080
//!
//! # Inspect local RUNTIME_WASM_PARSE diagnostics
//! clean-server errors list
//! clean-server errors show <SHA>
//! clean-server errors publish <SHA>
//! clean-server errors resolve <SHA>
//!
//! # Show version
//! clean-server --version
//! ```

use clap::{Parser, Subcommand};
use clean_server::error_reporting::{self, ReportStatus, ReportSummary, WasmParseReport};
use clean_server::server::MemoryTier;
use clean_server::{ServerConfig, start_server};
use std::path::PathBuf;
use tracing::{Level, error, info};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

/// Clean Server - HTTP Server for Clean Language Applications
#[derive(Parser, Debug)]
#[command(name = "clean-server")]
#[command(author = "Ivan Pasco <ivan@cleanframework.com>")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "Run compiled Clean Language WASM applications", long_about = None)]
#[command(subcommand_negates_reqs = true)]
struct Args {
    /// Path to the WASM file to execute
    #[arg(value_name = "WASM_FILE", required = true)]
    wasm_path: Option<PathBuf>,

    /// Port to listen on [default: the module's `server: port:`, else 3000].
    /// An explicit port overrides the module's declaration.
    #[arg(short, long)]
    port: Option<u16>,

    /// Host address to bind to [default: the module's `server: host:`, else 0.0.0.0].
    /// An explicit host overrides the module's declaration.
    #[arg(long)]
    host: Option<String>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Disable CORS
    #[arg(long)]
    no_cors: bool,

    /// Request body size limit in MB
    #[arg(long, default_value = "10")]
    body_limit: usize,

    /// Database URL (e.g., sqlite://app.db, postgres://user:pass@host/db)
    /// Can also be set via DATABASE_URL environment variable
    #[arg(long, env = "DATABASE_URL")]
    database: Option<String>,

    /// Database connection pool size
    #[arg(long, default_value = "10")]
    db_pool_size: u32,

    /// Max WASM memory per instance in MB (overrides --memory-tier)
    #[arg(long, env = "CLEAN_MEMORY_LIMIT_MB")]
    memory_limit: Option<usize>,

    /// Memory budget tier: minimal (8MB), standard (32MB), large (128MB), xlarge (512MB)
    #[arg(long, env = "CLEAN_MEMORY_TIER", default_value = "standard")]
    memory_tier: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect and publish local RUNTIME_WASM_PARSE diagnostics.
    #[command(subcommand)]
    Errors(ErrorsCommand),
}

#[derive(Subcommand, Debug)]
enum ErrorsCommand {
    /// Show where diagnostics are stored.
    Path,
    /// List all local diagnostic reports.
    List {
        /// Filter by lifecycle stage: pending, published, resolved.
        #[arg(long)]
        status: Option<String>,
    },
    /// Print a full diagnostic report.
    Show {
        /// SHA-256 prefix (≥ 4 chars) identifying the report.
        sha: String,
        /// Emit the raw JSON payload instead of the human-readable view.
        #[arg(long)]
        json: bool,
    },
    /// Print the publish payload and move the report to `published/`.
    ///
    /// The payload is the same JSON you would paste into the error
    /// dashboard or feed to the MCP `report_error` tool.
    Publish {
        /// SHA-256 prefix (≥ 4 chars) identifying the report.
        sha: Option<String>,
        /// Publish every pending report.
        #[arg(long)]
        all: bool,
    },
    /// Mark a diagnostic as resolved (strips heavy fields, keeps fingerprint).
    Resolve {
        /// SHA-256 prefix (≥ 4 chars) identifying the report.
        sha: String,
    },
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let log_level = if args.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };

    // Compose the console formatter with the dev-mode capture layer. The
    // capture layer is a no-op unless CLEAN_DEV=1, but installing it
    // unconditionally means an operator can toggle CLEAN_DEV without
    // restarting the server. See `dev_capture::DevCaptureTracingLayer`.
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        // Logs go to stderr: stdout belongs to the application (its print
        // output, and the JSON-RPC stream of a stdio MCP server).
        .with_writer(std::io::stderr)
        .compact()
        .with_filter(tracing_subscriber::filter::LevelFilter::from(log_level));
    Registry::default()
        .with(fmt_layer)
        .with(clean_server::dev_capture::DevCaptureTracingLayer)
        .init();

    match args.command {
        Some(Command::Errors(cmd)) => {
            if let Err(e) = run_errors_command(cmd) {
                error!("{}", e);
                std::process::exit(1);
            }
            return;
        }
        None => {
            if let Err(code) = run_server_command(args).await {
                std::process::exit(code);
            }
        }
    }
}

async fn run_server_command(args: Args) -> Result<(), i32> {
    eprintln!();
    eprintln!("  ╔═══════════════════════════════════════════╗");
    eprintln!(
        "  ║         Clean Server v{}            ║",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("  ║   HTTP Server for Clean Language Apps     ║");
    eprintln!("  ╚═══════════════════════════════════════════╝");
    eprintln!();

    let wasm_path = match args.wasm_path {
        Some(path) => path,
        None => {
            error!("A WASM file path is required when no subcommand is given.");
            return Err(1);
        }
    };

    if !wasm_path.exists() {
        error!("WASM file not found: {:?}", wasm_path);
        return Err(1);
    }

    if wasm_path.extension().is_none_or(|ext| ext != "wasm") {
        error!("File must have .wasm extension: {:?}", wasm_path);
        return Err(1);
    }

    let memory_tier: MemoryTier = args.memory_tier.parse().unwrap_or_else(|e| {
        error!("{}", e);
        std::process::exit(1);
    });

    let mut config = ServerConfig::default()
        .with_database_pool_size(args.db_pool_size)
        .with_memory_tier(memory_tier);

    if let Some(host) = args.host {
        config = config.with_host(host);
    }
    if let Some(port) = args.port {
        config = config.with_port(port);
    }
    if let Some(mb) = args.memory_limit {
        config = config.with_memory_limit_mb(mb);
    }

    config.cors_enabled = !args.no_cors;
    config.body_limit = args.body_limit * 1024 * 1024;

    if args.database.is_some() {
        config.database_url = args.database;
    }

    info!("Configuration:");
    info!("  WASM file: {:?}", wasm_path);
    info!(
        "  Listen: {}:{}{}",
        config.host,
        config.port,
        if config.port_explicit {
            ""
        } else {
            " (unless the module declares server: port:)"
        }
    );
    info!(
        "  CORS: {}",
        if config.cors_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    info!("  Body limit: {} MB", args.body_limit);
    info!(
        "  Memory: {} tier ({} MB limit)",
        config.memory_tier,
        config.effective_memory_limit() / (1024 * 1024)
    );
    if config.database_url.is_some() {
        info!("  Database: configured");
    } else {
        info!("  Database: not configured");
    }
    eprintln!();

    match start_server(wasm_path, config).await {
        Ok(()) => {
            info!("Server stopped");
            Ok(())
        }
        Err(e) => {
            error!("Server error: {}", e);
            Err(1)
        }
    }
}

// ---------------------------------------------------------------------
// `errors` subcommand
// ---------------------------------------------------------------------

fn run_errors_command(cmd: ErrorsCommand) -> Result<(), String> {
    let diag_root = error_reporting::diag_dir();

    match cmd {
        ErrorsCommand::Path => {
            println!("{}", diag_root.display());
            Ok(())
        }
        ErrorsCommand::List { status } => cmd_list(&diag_root, status.as_deref()),
        ErrorsCommand::Show { sha, json } => cmd_show(&diag_root, &sha, json),
        ErrorsCommand::Publish { sha, all } => cmd_publish(&diag_root, sha.as_deref(), all),
        ErrorsCommand::Resolve { sha } => cmd_resolve(&diag_root, &sha),
    }
}

fn cmd_list(diag_root: &std::path::Path, status_filter: Option<&str>) -> Result<(), String> {
    if !diag_root.exists() {
        println!("No diagnostics directory at {}", diag_root.display());
        println!("(Nothing to show — the runtime has not reported any WASM parse failures yet.)");
        return Ok(());
    }

    let filter = match status_filter {
        Some("pending") => Some(ReportStatus::Pending),
        Some("published") => Some(ReportStatus::Published),
        Some("resolved") => Some(ReportStatus::Resolved),
        Some(other) => {
            return Err(format!(
                "Unknown status '{}'. Use one of: pending, published, resolved.",
                other
            ));
        }
        None => None,
    };

    let mut reports = error_reporting::list_reports(diag_root)
        .map_err(|e| format!("Failed to read diagnostics: {}", e))?;
    if let Some(s) = filter {
        reports.retain(|r| r.status == s);
    }

    if reports.is_empty() {
        println!("No diagnostic reports found.");
        return Ok(());
    }

    let header = format!(
        "{:<12} {:<10} {:<25} {:<8} {}",
        "SHA", "STATUS", "REPORTED", "COUNT", "ERROR"
    );
    println!("{header}");
    println!("{}", "-".repeat(100));
    for r in &reports {
        print_summary_row(r);
    }
    println!();
    println!(
        "{} report(s). Inspect one: `clean-server errors show <SHA>`",
        reports.len()
    );
    Ok(())
}

fn print_summary_row(r: &ReportSummary) {
    let status_label = match r.status {
        ReportStatus::Pending => "pending",
        ReportStatus::Published => "published",
        ReportStatus::Resolved => "resolved",
    };
    let error_preview = truncate(&r.wasmtime_error_first_line, 60);
    println!(
        "{:<12} {:<10} {:<25} {:<8} {}",
        r.short, status_label, r.reported_at, r.occurrences, error_preview
    );
}

fn cmd_show(diag_root: &std::path::Path, sha: &str, as_json: bool) -> Result<(), String> {
    let report = error_reporting::load_report(diag_root, sha)
        .map_err(|e| format!("Lookup failed: {}", e))?
        .ok_or_else(|| format!("No diagnostic matches '{}'", sha))?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|e| format!("Failed to serialize report: {}", e))?
        );
        return Ok(());
    }

    print_report_human(&report);
    Ok(())
}

fn print_report_human(report: &WasmParseReport) {
    println!();
    println!("{}", "=".repeat(70));
    println!("  RUNTIME_WASM_PARSE diagnostic");
    println!("{}", "=".repeat(70));
    println!();
    println!("  Fingerprint (SHA-256)  {}", report.wasm_sha256);
    println!("  Reported at            {}", report.reported_at);
    println!("  Server version         {}", report.server_version);
    println!(
        "  Compiler version       {} (source: {})",
        report
            .compiler_version
            .as_deref()
            .unwrap_or("unknown — compiler has not stamped clean:build yet"),
        report.compiler_version_source
    );
    println!("  WASM size              {} bytes", report.wasm_bytes_len);
    println!(
        "  wasmparser validates   {}",
        if report.wasmparser_validates {
            "yes — encoder bug is subtle (validator-vs-runtime mismatch)"
        } else {
            "no — the compiler emitted invalid WASM"
        }
    );
    if let Some(err) = &report.wasmparser_error {
        println!("  wasmparser error       {}", truncate(err, 200));
    }
    if let Some(path) = &report.module_path {
        println!("  Originating WASM file  {}", path);
    }
    println!("  Status                 {:?}", report.status);
    println!();
    println!("  Wasmtime error (full):");
    for line in report.wasmtime_error.lines() {
        println!("    {}", line);
    }
    println!();
    println!(
        "  WASM header (first {} bytes as hex):",
        report.wasm_header_hex.len() / 2
    );
    for chunk in report.wasm_header_hex.as_bytes().chunks(64) {
        println!("    {}", std::str::from_utf8(chunk).unwrap_or(""));
    }
    println!();
    if report.plugin_manifest.is_empty() {
        println!("  Plugin manifest: (none captured)");
    } else {
        println!(
            "  Plugin manifest ({} plugin(s) installed at snapshot time):",
            report.plugin_manifest.len()
        );
        for p in &report.plugin_manifest {
            println!(
                "    - {} v{} ({} bridge functions)",
                p.name,
                p.version,
                p.bridge_functions.len()
            );
        }
    }
    println!();
    println!("  Next steps:");
    println!(
        "    clean-server errors publish {}   # print payload + move to published/",
        &report.wasm_sha256[..12]
    );
    println!(
        "    clean-server errors resolve {}   # mark resolved after fix ships",
        &report.wasm_sha256[..12]
    );
    println!();
}

fn cmd_publish(diag_root: &std::path::Path, sha: Option<&str>, all: bool) -> Result<(), String> {
    match (sha, all) {
        (Some(s), false) => publish_one(diag_root, s),
        (None, true) => publish_all(diag_root),
        (Some(_), true) => Err("Pass either <SHA> or --all, not both.".to_string()),
        (None, false) => Err(
            "Specify a SHA prefix or pass --all. Try `clean-server errors list` first.".to_string(),
        ),
    }
}

fn publish_one(diag_root: &std::path::Path, sha: &str) -> Result<(), String> {
    let report = error_reporting::load_report(diag_root, sha)
        .map_err(|e| format!("Lookup failed: {}", e))?
        .ok_or_else(|| format!("No diagnostic matches '{}'", sha))?;

    let json = serde_json::to_string_pretty(&report)
        .map_err(|e| format!("Failed to serialize report: {}", e))?;

    println!();
    println!(
        "=== RUNTIME_WASM_PARSE publish payload ({}) ===",
        report.short_fingerprint()
    );
    println!();
    println!("Copy the JSON below into the dashboard's error form, or pipe to");
    println!("the Clean MCP server's `report_error` tool:");
    println!();
    println!("--- BEGIN PAYLOAD ---");
    println!("{}", json);
    println!("--- END PAYLOAD ---");
    println!();

    let new_dir =
        error_reporting::transition(diag_root, &report.wasm_sha256, ReportStatus::Published)
            .map_err(|e| format!("Failed to mark as published: {}", e))?;

    println!("Moved to {}", new_dir.display());
    println!(
        "When the fix ships, run: clean-server errors resolve {}",
        &report.wasm_sha256[..12]
    );
    Ok(())
}

fn publish_all(diag_root: &std::path::Path) -> Result<(), String> {
    if !diag_root.exists() {
        println!("No diagnostics directory at {}", diag_root.display());
        return Ok(());
    }
    let summaries = error_reporting::list_reports(diag_root)
        .map_err(|e| format!("Failed to read diagnostics: {}", e))?;
    let pending: Vec<_> = summaries
        .into_iter()
        .filter(|s| s.status == ReportStatus::Pending)
        .collect();

    if pending.is_empty() {
        println!("No pending reports to publish.");
        return Ok(());
    }

    println!("Publishing {} pending report(s)...", pending.len());
    for s in &pending {
        println!();
        publish_one(diag_root, &s.sha)?;
    }
    Ok(())
}

fn cmd_resolve(diag_root: &std::path::Path, sha: &str) -> Result<(), String> {
    let new_dir = error_reporting::transition(diag_root, sha, ReportStatus::Resolved)
        .map_err(|e| format!("Failed to resolve: {}", e))?;
    println!("Marked resolved: {}", new_dir.display());
    println!("(Heavy fields stripped; fingerprint retained for regression detection.)");
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
