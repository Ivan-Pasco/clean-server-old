# Clean Server

High-performance HTTP server runtime for executing Clean Language WebAssembly applications.

## Overview

Clean Server is a production-ready HTTP server that executes compiled Clean Language (`.wasm`) applications. It provides a complete runtime environment with:

- **WebAssembly Execution**: Fast, secure sandboxed execution using Wasmtime
- **HTTP Server**: High-performance async HTTP server built on Axum
- **Host Bridge**: Comprehensive system integration (HTTP, DB, FS, Crypto, etc.)
- **Multi-Platform**: Runs on Linux, macOS, and Windows (x64 and ARM64)

## Installation

### Pre-built Binaries

Download the latest release for your platform:

```bash
# Linux x86_64
curl -L https://github.com/Ivan-Pasco/clean-server/releases/latest/download/clean-server-linux-x86_64.tar.gz | tar xz

# Linux ARM64
curl -L https://github.com/Ivan-Pasco/clean-server/releases/latest/download/clean-server-linux-arm64.tar.gz | tar xz

# macOS x86_64 (Intel)
curl -L https://github.com/Ivan-Pasco/clean-server/releases/latest/download/clean-server-macos-x86_64.tar.gz | tar xz

# macOS ARM64 (Apple Silicon)
curl -L https://github.com/Ivan-Pasco/clean-server/releases/latest/download/clean-server-macos-arm64.tar.gz | tar xz

# Windows x86_64
# Download from: https://github.com/Ivan-Pasco/clean-server/releases/latest/download/clean-server-windows-x86_64.exe.zip
```

### Build from Source

```bash
git clone https://github.com/Ivan-Pasco/clean-server.git
cd clean-server
cargo build --release
```

The binary will be available at `target/release/clean-server`.

## Quick Start

### Basic Usage

```bash
# Run a Clean Language WASM application
clean-server run app.wasm

# Specify port
clean-server run app.wasm --port 8080

# Enable debug logging
clean-server run app.wasm --log-level debug
```

### Command Line Options

```
clean-server run [OPTIONS] <WASM_FILE>

Arguments:
  <WASM_FILE>  Path to the compiled WASM file

Options:
  -p, --port <PORT>              Server port [default: 3000]
  -H, --host <HOST>              Server host [default: 0.0.0.0]
      --log-level <LEVEL>        Log level [default: info]
                                 [possible values: trace, debug, info, warn, error]
      --max-memory <MB>          Maximum WASM memory in MB [default: 512]
      --max-request-size <MB>    Maximum request body size in MB [default: 10]
      --cors                     Enable CORS
      --cors-origin <ORIGIN>     CORS allowed origin [default: *]
  -h, --help                     Print help
  -V, --version                  Print version
```

### Example Application

Create a simple Clean Language application:

```clean
// hello.cln
endpoints:
	GET "/":
		returns: json({
			message: "Hello from Clean Server!",
			timestamp: time.now()
		})

	GET "/health":
		returns: json({ status: "ok" })
```

Compile and run:

```bash
# Compile with Clean Language compiler
clean-compile hello.cln -o hello.wasm

# Run with Clean Server
clean-server run hello.wasm
```

Test the endpoints:

```bash
curl http://localhost:3000/
curl http://localhost:3000/health
```

## Host Bridge Capabilities

Clean Server registers **476 host bridge functions** against the WASM `env`
linker, split across two layers:

- **Layer 2 — portable (`host-bridge/src/wasm_linker/`)** — 308 functions
  shared with every other Clean host (Node runtime, browser runtime,
  future WASI hosts).
- **Layer 3 — server-only (`src/bridge.rs`)** — 168 functions for HTTP
  routing, request/response context, sessions, and server-side
  frameworks. Not portable.

Grouped by canonical prefix (function counts from `grep func_wrap "env",`
and `register_bridge_fn!` on the current source; treat as rough):

### Layer 2 — portable

| Namespace | ~Count | What it does |
|-----------|-------:|--------------|
| `math.*` | 96 | IEEE-754 math + full Clean stdlib coverage |
| `string.*` | 49 | UTF-8-aware ops, formatting, parsing |
| `http.*` | 27 | HTTP client (GET/POST/PUT/…/response inspection) |
| `array.*` + `list.*` | 23 | Heap collection ops |
| `db.*` | 19 | SQL query/exec, transactions, connection state |
| `time.*` | 16 | Epoch, ISO, timezone offsets, sleep |
| `crypto.*` | 16 | Hashing, HMAC, UUID, random, JWT helpers |
| `file.*` / `fs.*` | 16 | File I/O, incl. opaque byte-handle bridges |
| `print*` / `console.*` | 11 | Stdout formatting for primitives |
| `env.*` | 7 | Environment variables, `NODE_ENV` |
| `input.*` | 5 | Stdin readers (for CLI/test entry points) |
| `arena.*` | 4 | Bump-allocator control |
| `state.*` | 4 | Cross-request state slots |
| `jwt.*` | 3 | JWT encode/decode |
| Scalar coercions | ~5 | `int`, `boolean`, `float`, `number`, `integer` bridges |

### Layer 3 — server-only

| Namespace | ~Count | What it does |
|-----------|-------:|--------------|
| `_http_*` | 26 | Routing (`_http_route`, `_http_ws_route`, `_http_sse_route`), response helpers, CORS, cache, cookies |
| `_req_*` | 24 | Request context: method, path, headers, cookies, form/body/JSON, params, queries, IP, auth token |
| `_ui_*` | 19 | Server-side UI: component HTML registration, page rendering, layouts, patches, iframe messaging |
| `_auth_*` | 16 | Authentication guards, session roles, user identity, password-reset tokens |
| `_session_*` | 15 | Session lifecycle: create/extend/destroy/claim, CSRF, key/value store |
| `_job_*` | 12 | Background jobs: enqueue, cancel, register, retry, status, results |
| `_i18n_*` | 8 | Locale, translation lookup, currency/date/number formatting |
| `_ws_*` | 8 | WebSocket lifecycle, send, broadcast, room membership |
| `_mcp_*` | 7 | Model Context Protocol transport (stdio + HTTP + SSE) |
| `_res_*` | 6 | Response mutation: status, body, JSON, redirect, download, headers |
| `_sse_*` | 5 | Server-Sent Events: emit, close, retry |
| `_email_*` | 3 | Send, configure, last-error |
| `_role_*` / `_roles_*` | 3 | Role registration, permission lookup |
| `_json_*` | 3 | Host-side JSON parse/encode (bypasses WASM memory) |
| `_test_*` | 3 | In-process HTTP request helpers for tests |
| `_async_*` | 2 | `_async_fire`, `_async_await` |
| `_schedule_*` | 2 | Cron register / cancel |
| Singletons | 6 | `_dev_snapshot`, `_island_register`, `_jwt_refresh_and_rotate`, `_cors_configure`, `_rate_limit_configure`, `_server_sleep` |

### Authoritative sources

- **`foundation/spec/platform/function-registry.toml`** — the machine-readable
  signature registry for every Layer 2 + Layer 3 function (source of truth).
- **`foundation/spec/platform/HOST_BRIDGE.md`** — Layer 2 contract details:
  string-passing convention `(ptr, len)`, length-prefixed returns, dual
  naming rules (`_ns_fn` and `ns.fn`).
- **`foundation/spec/platform/SERVER_EXTENSIONS.md`** — Layer 3 contract for
  the routing, request-context, and session APIs.
- **`foundation/spec/platform/EXECUTION_LAYERS.md`** — which layer executes
  which function, and why.

Drift between this server's implementation and `function-registry.toml` is
verified in CI via
`foundation/management/scripts/check_host_parity.py --host server --strict`
plus the in-repo compliance tests (`test_spec_compliance` for Layer 2,
`test_layer3_spec_compliance` for Layer 3).

## Architecture

Clean Server consists of three main components:

1. **WASM Runtime**: Executes compiled Clean Language applications using Wasmtime
2. **HTTP Server**: Handles incoming HTTP requests and routes them to WASM handlers
3. **Host Bridge**: Provides secure system integration for WASM modules

```
┌─────────────────────────────────────────┐
│         Clean Server                    │
├─────────────────────────────────────────┤
│  HTTP Server (Axum)                     │
│    ↓                                    │
│  Router & Middleware                    │
│    ↓                                    │
│  WASM Runtime (Wasmtime)                │
│    ↓                                    │
│  Host Bridge                            │
│    ↓                                    │
│  System Resources                       │
│  (HTTP, DB, FS, Crypto, etc.)          │
└─────────────────────────────────────────┘
```

## Configuration

### Environment Variables

```bash
# Server configuration
CLEAN_SERVER_PORT=3000
CLEAN_SERVER_HOST=0.0.0.0
CLEAN_SERVER_LOG_LEVEL=info

# Database connection
DATABASE_URL=postgresql://user:pass@localhost/db

# Security
CLEAN_SERVER_CORS_ENABLED=true
CLEAN_SERVER_CORS_ORIGIN=https://example.com
CLEAN_SERVER_MAX_REQUEST_SIZE=10
```

### Production Deployment

#### Systemd Service (Linux)

Create `/etc/systemd/system/clean-server.service`:

```ini
[Unit]
Description=Clean Server
After=network.target

[Service]
Type=simple
User=www-data
WorkingDirectory=/var/www/app
ExecStart=/usr/local/bin/clean-server run app.wasm --port 3000
Restart=always
Environment="DATABASE_URL=postgresql://user:pass@localhost/db"

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl enable clean-server
sudo systemctl start clean-server
```

#### Docker

```dockerfile
FROM debian:bookworm-slim

# Install dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy binary
COPY clean-server /usr/local/bin/clean-server
RUN chmod +x /usr/local/bin/clean-server

# Copy application
COPY app.wasm /app/app.wasm

WORKDIR /app
EXPOSE 3000

CMD ["clean-server", "run", "app.wasm", "--port", "3000", "--host", "0.0.0.0"]
```

Build and run:

```bash
docker build -t my-clean-app .
docker run -p 3000:3000 my-clean-app
```

## Performance

Clean Server is designed for high performance:

- **Fast startup**: < 100ms cold start
- **Low latency**: < 1ms p50 for simple endpoints
- **High throughput**: > 10,000 req/sec on modern hardware
- **Efficient memory**: < 50MB baseline memory usage

### Benchmarks

Benchmarked on MacBook Pro M1 Max (10 cores, 32GB RAM):

```
Endpoint Type          | p50    | p95    | p99    | Throughput
-----------------------|--------|--------|--------|------------
Static JSON            | 0.2ms  | 0.4ms  | 0.8ms  | 50,000 rps
Database Query         | 2.1ms  | 4.5ms  | 8.2ms  | 8,000 rps
HTTP Proxy             | 15.3ms | 28.1ms | 45.7ms | 1,200 rps
```

## Security

Clean Server implements defense-in-depth security:

- **WASM Sandboxing**: All application code runs in isolated WASM context
- **Host Bridge Allowlisting**: Explicit permissions for system access
- **SQL Injection Prevention**: Parameterized queries only
- **XSS Prevention**: Automatic HTML escaping
- **CSRF Protection**: Built-in token validation
- **Secure Cookies**: HTTP-only, SameSite, Secure flags
- **Rate Limiting**: Per-IP and per-endpoint limits
- **Input Validation**: Both compile-time and runtime checks

## Troubleshooting

### Common Issues

**Server won't start**
```bash
# Check if port is already in use
lsof -i :3000

# Try different port
clean-server run app.wasm --port 8080
```

**WASM module fails to load**
```bash
# Verify WASM file
file app.wasm

# Enable debug logging
clean-server run app.wasm --log-level debug
```

**Database connection errors**
```bash
# Test database connectivity
psql $DATABASE_URL -c "SELECT 1"

# Check environment variable
echo $DATABASE_URL
```

### Debug Mode

Enable verbose logging for troubleshooting:

```bash
RUST_LOG=clean_server=debug,host_bridge=debug clean-server run app.wasm
```

## Development

### Building from Source

```bash
# Clone repository (JSONTestSuite corpus is a submodule)
git clone --recurse-submodules https://github.com/Ivan-Pasco/clean-server.git
cd clean-server
# Or, for an existing clone:
git submodule update --init --recursive

# Build
cargo build --release

# Run tests
cargo test

# Run with example
cargo run --release -- run examples/hello.wasm
```

### Project Structure

Two crates live in this repo: the server binary (`src/`) and the portable
Layer 2 host bridge (`host-bridge/`, published as its own crate so other
runtimes can consume it).

```
clean-server/
├── src/                              # Layer 3 — server-only
│   ├── main.rs                       # CLI entry point
│   ├── lib.rs                        # Library exports
│   ├── server.rs                     # HTTP server (Axum)
│   ├── router.rs                     # Request routing
│   ├── wasm.rs                       # WASM runtime (Wasmtime)
│   ├── bridge.rs                     # Layer 3 host bridge (~168 fns:
│   │                                 #   _http_*, _req_*, _res_*, _auth_*,
│   │                                 #   _session_*, _ws_*, _sse_*, _ui_*,
│   │                                 #   _job_*, _i18n_*, _email_*, _mcp_*,
│   │                                 #   _dev_snapshot, _schedule_*, …)
│   ├── bridge_browser_stubs.rs       # Stubs for browser-only functions
│   ├── bridge_canvas_stubs.rs        # Stubs for canvas-only functions
│   ├── bridge_ui_stubs.rs            # Stubs for UI-only functions
│   ├── session.rs                    # Session store + CSRF
│   ├── websocket.rs                  # WebSocket transport
│   ├── jobs.rs                       # Background job queue
│   ├── locale.rs                     # i18n backend
│   ├── permissions.rs                # Role/permission model
│   ├── rate_limit.rs                 # Per-IP + per-route rate limiting
│   ├── dev_capture.rs                # /_debug/capture snapshot backend
│   ├── build_manifest.rs             # Build metadata
│   ├── runtime_config.rs             # Config file loader
│   ├── error_reporting.rs            # errors.cleanlanguage.dev shipper
│   ├── memory.rs                     # WASM memory helpers
│   ├── error.rs                      # Error types
│   └── bin/                          # Auxiliary binaries
├── host-bridge/                      # Layer 2 — portable across all hosts
│   └── src/
│       ├── lib.rs                    # Public crate exports
│       ├── error.rs                  # Bridge error types
│       ├── http.rs                   # HTTP client (reqwest-backed)
│       ├── db.rs                     # SQL adapters (PG / MySQL / SQLite)
│       ├── crypto.rs                 # Hashing, HMAC, UUID, JWT
│       ├── env.rs                    # Environment variable access
│       ├── time.rs                   # Epoch + ISO + timezone
│       ├── fs.rs                     # Filesystem I/O + byte handles
│       ├── log.rs                    # Structured logging
│       ├── sys.rs                    # System info
│       └── wasm_linker/              # Registers ~308 functions on the
│           │                         # WASM `env` linker
│           ├── mod.rs                # Entry point + dual-naming aliases
│           ├── math.rs               # ~96 math.* functions
│           ├── string_ops.rs         # ~59 string ops
│           ├── http_client.rs        # ~27 http.* client fns
│           ├── env_time.rs           # env/time bridge registrations
│           ├── crypto_funcs.rs       # crypto.* + jwt.* bindings
│           ├── database.rs           # db.* bindings
│           ├── file_io.rs            # file.* / fs.* incl. byte handles
│           ├── array_funcs.rs        # array.* ops
│           ├── list_funcs.rs         # list.* ops
│           ├── console.rs            # print/input primitives
│           ├── memory.rs             # arena.* + WASM memory helpers
│           ├── state.rs              # state.* cross-request slots
│           └── helpers.rs            # Shared string-encoding helpers
├── Cargo.toml
├── CHANGELOG.md
└── README.md
```

Signatures for every function in both layers are declared in
`foundation/spec/platform/function-registry.toml` and enforced by
`test_spec_compliance` (Layer 2) and `test_layer3_spec_compliance`
(Layer 3).

### JSON conformance (JSONTestSuite)

The full [JSONTestSuite](https://github.com/nst/JSONTestSuite) corpus is
vendored as a git submodule at `tests/fixtures/JSONTestSuite/` and driven
through `_json_decode_v2` by `tests/json_conformance.rs`. Every push runs
the harness in CI (job `T2 · JSON conformance`) and gates against
`tests/json_conformance_baseline.txt`.

Current baseline (318 fixtures):

| Category | Result | Meaning |
|----------|--------|---------|
| `y_*`    | 95/95 pass    | Every well-formed JSON parses successfully |
| `n_*`    | 188/188 reject | Every malformed JSON returns the 0 sentinel |
| `i_*`    | 5 pass / 30 reject / 0 skip | Implementation-defined; logged only |

A commit that lowers the `y_*` pass count or the `n_*` reject count fails
CI. `i_*` deltas print warnings but do not gate.

To refresh the baseline after an intentional bridge change:

```bash
rm tests/json_conformance_baseline.txt
cargo test --test json_conformance  # regenerates the file, then fails once
git add tests/json_conformance_baseline.txt
```

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Links

- **Repository**: https://github.com/Ivan-Pasco/clean-server
- **Clean Language**: https://github.com/Ivan-Pasco/clean-language-compiler
- **Documentation**: https://docs.cleanlang.dev
- **Discord**: https://discord.gg/cleanlang

## Changelog

### v1.0.0 (2024-12-07)

- Initial release
- WebAssembly runtime using Wasmtime
- HTTP server with Axum
- Complete Host Bridge implementation
- Multi-platform support (Linux, macOS, Windows)
- Production-ready performance and security
