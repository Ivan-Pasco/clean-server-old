use anyhow::Result;
use serde::{Deserialize, Serialize};

mod crypto;
mod db;
mod env;
pub mod error;
mod fs;
mod http;
mod log;
mod mysql_sql;
mod sys;
mod time;
pub mod wasm_linker;

pub use crypto::CryptoBridge;
pub use db::{DbBridge, DbConfig, DbQuery, DbResult, OpenTransaction};
pub use env::EnvBridge;
pub use error::{BridgeError as WasmBridgeError, BridgeResult};
pub use fs::FsBridge;
pub use http::{HttpBridge, HttpRequest, HttpResponse};
pub use log::{LogBridge, LogConfig, LogEntry, LogLevel};
pub use sys::SysBridge;
pub use time::TimeBridge;
pub use wasm_linker::{
    // Linker creation
    create_linker,
    read_length_prefixed_bytes,
    read_raw_string,
    // Helper functions
    read_string_from_caller,
    register_all_functions,
    write_bytes_to_caller,
    write_string_to_caller,
    AuthContext,
    RequestContext,
    SharedDbBridge,
    WasmMemory,
    // Core types and trait
    WasmState,
    WasmStateCore,
    STRING_LENGTH_PREFIX_SIZE,
};

/// Standard envelope for all bridge responses
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BridgeResponse<T> {
    Ok { ok: bool, data: T },
    Err { ok: bool, err: BridgeError },
}

impl<T> BridgeResponse<T> {
    pub fn success(data: T) -> Self {
        Self::Ok { ok: true, data }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Err {
            ok: false,
            err: BridgeError {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeError {
    pub code: String,
    pub message: String,
}

/// Main host bridge that provides all capabilities
pub struct HostBridge {
    http: HttpBridge,
    db: DbBridge,
    env: EnvBridge,
    time: TimeBridge,
    crypto: CryptoBridge,
    log: LogBridge,
    sys: SysBridge,
    fs: FsBridge,
}

impl HostBridge {
    pub fn new() -> Self {
        Self {
            http: HttpBridge::new(),
            db: DbBridge::new(),
            env: EnvBridge::new(),
            time: TimeBridge::new(),
            crypto: CryptoBridge::new(),
            log: LogBridge::new(),
            sys: SysBridge::new(),
            fs: FsBridge::new(),
        }
    }

    /// Call a bridge function by namespace and name
    pub async fn call(
        &mut self,
        namespace: &str,
        function: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match namespace {
            "http" => self.http.call(function, params).await,
            "db" => self.db.call(function, params).await,
            "env" => self.env.call(function, params).await,
            "time" => self.time.call(function, params).await,
            "crypto" => self.crypto.call(function, params).await,
            "log" => Ok(self.log.call(function, params)?),
            "sys" => self.sys.call(function, params).await,
            "fs" => self.fs.call(function, params).await,
            _ => anyhow::bail!("Unknown bridge namespace: {}", namespace),
        }
    }

    pub fn http(&mut self) -> &mut HttpBridge {
        &mut self.http
    }

    pub fn db(&mut self) -> &mut DbBridge {
        &mut self.db
    }

    pub fn env(&self) -> &EnvBridge {
        &self.env
    }

    pub fn time(&self) -> &TimeBridge {
        &self.time
    }

    pub fn crypto(&mut self) -> &mut CryptoBridge {
        &mut self.crypto
    }

    pub fn log(&self) -> &LogBridge {
        &self.log
    }

    pub fn sys(&self) -> &SysBridge {
        &self.sys
    }

    pub fn fs(&self) -> &FsBridge {
        &self.fs
    }
}

impl Default for HostBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bridge_response_success() {
        let response: BridgeResponse<i32> = BridgeResponse::success(42);
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("\"data\":42"));
    }

    #[test]
    fn test_bridge_response_error() {
        let response: BridgeResponse<i32> =
            BridgeResponse::error("TEST_ERROR", "Test error message");
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("TEST_ERROR"));
    }

    #[tokio::test]
    async fn test_host_bridge_creation() {
        let bridge = HostBridge::new();
        assert!(bridge.env.get("HOME").is_some() || bridge.env.get("USERPROFILE").is_some());
    }
}
