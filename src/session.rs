//! Session Storage for Clean Server
//!
//! Provides in-memory session storage with support for:
//! - Session creation with claims
//! - Session retrieval and validation
//! - Session deletion (logout)
//! - Automatic expiration
//!
//! Future: Redis/database-backed sessions for horizontal scaling

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tracing::{debug, info};
use uuid::Uuid;

/// Session data stored in the session store
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    /// Session ID
    pub session_id: String,
    /// User ID (Clean Language integer = i32)
    pub user_id: i32,
    /// User role
    pub role: String,
    /// Additional claims (JSON string)
    pub claims: String,
    /// Last accessed timestamp
    #[serde(skip)]
    last_accessed: Option<Instant>,
}

impl SessionData {
    pub fn new(user_id: i32, role: String, claims: String) -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            user_id,
            role,
            claims,
            last_accessed: Some(Instant::now()),
        }
    }

    /// Check if session is expired
    pub fn is_expired(&self, timeout: Duration) -> bool {
        if let Some(last_accessed) = self.last_accessed {
            last_accessed.elapsed() > timeout
        } else {
            true
        }
    }

    /// Update last accessed time
    pub fn touch(&mut self) {
        self.last_accessed = Some(Instant::now());
    }
}

/// Session store configuration
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Session timeout in seconds (default: 3600 = 1 hour)
    pub timeout_seconds: u64,
    /// Cookie name for session ID
    pub cookie_name: String,
    /// Cookie path
    pub cookie_path: String,
    /// Cookie SameSite attribute
    pub same_site: String,
    /// Cookie secure flag
    pub secure: bool,
    /// Cookie httpOnly flag
    pub http_only: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 3600,
            cookie_name: "session".to_string(),
            cookie_path: "/".to_string(),
            same_site: "Lax".to_string(),
            secure: true,
            http_only: true,
        }
    }
}

impl SessionConfig {
    /// Build a session configuration from a `Set-Cookie` value such as
    /// `app.sid=abc; Path=/; SameSite=Lax; Secure; HttpOnly; Max-Age=300`.
    ///
    /// This is how the host learns an application's `auth: session:` settings:
    /// frame.auth bakes them into the cookie header its generated
    /// `__auth_build_cookie_header` helper returns. Attributes that are absent
    /// take their `SessionConfig::default()` value, except the boolean flags,
    /// whose absence means `false`. Returns `None` when there is no cookie name.
    pub fn from_set_cookie(header: &str) -> Option<Self> {
        let mut parts = header.split(';');
        let name_value = parts.next()?.trim();
        let name = name_value.split('=').next()?.trim();
        if name.is_empty() {
            return None;
        }
        let mut config = SessionConfig {
            cookie_name: name.to_string(),
            secure: false,
            http_only: false,
            ..SessionConfig::default()
        };
        for attr in parts {
            let attr = attr.trim();
            let (key, value) = match attr.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (attr, ""),
            };
            match key.to_ascii_lowercase().as_str() {
                "path" if !value.is_empty() => config.cookie_path = value.to_string(),
                "samesite" if !value.is_empty() => config.same_site = value.to_string(),
                "secure" => config.secure = true,
                "httponly" => config.http_only = true,
                "max-age" => {
                    if let Ok(secs) = value.parse::<u64>() {
                        config.timeout_seconds = secs;
                    }
                }
                _ => {}
            }
        }
        Some(config)
    }
}

/// The identity a session carries: who the user is and which role they hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    pub user_id: i32,
    pub role: String,
}

/// Extract the identity from a raw session blob as stored by frame.auth's
/// `session_create` helper: `{"userId":7,"claims":{"role":"user"},"expiry":...}`.
///
/// The user id is `userId` (or `user_id`); the role is `claims.role`, falling
/// back to a top-level `role`. `claims` may be an object or a JSON string.
/// Returns `None` for blobs without a user id or whose `expiry` has passed.
/// `expiry` is a Unix timestamp; values above 10^11 are read as milliseconds.
pub fn identity_from_session_json(data: &str) -> Option<SessionIdentity> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let user_id = value
        .get("userId")
        .or_else(|| value.get("user_id"))
        .and_then(json_to_i64)?;
    if let Some(expiry) = value.get("expiry").and_then(json_to_i64)
        && expiry > 0
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now = if expiry > 100_000_000_000 {
            now.as_millis() as i64
        } else {
            now.as_secs() as i64
        };
        if now > expiry {
            return None;
        }
    }
    let claims = match value.get("claims") {
        Some(serde_json::Value::String(s)) => serde_json::from_str(s).ok(),
        Some(v) => Some(v.clone()),
        None => None,
    };
    let role = claims
        .as_ref()
        .and_then(|c| c.get("role"))
        .or_else(|| value.get("role"))
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    Some(SessionIdentity {
        user_id: i32::try_from(user_id).ok()?,
        role,
    })
}

/// Extract the identity from a verified JWT payload: the user id is `sub`
/// (or `userId` / `user_id`), the role is `role`. frame.auth's `jwt.sign`
/// nests the application claims under a `claims` object next to the
/// registered `iat` / `exp` / `jti`; those nested claims are read first.
pub fn identity_from_jwt_claims(claims_json: &str) -> Option<SessionIdentity> {
    let payload: serde_json::Value = serde_json::from_str(claims_json).ok()?;
    let value = match payload.get("claims") {
        Some(serde_json::Value::Object(_)) => payload.get("claims")?.clone(),
        Some(serde_json::Value::String(s)) => serde_json::from_str(s).ok()?,
        _ => payload,
    };
    let user_id = value
        .get("sub")
        .or_else(|| value.get("userId"))
        .or_else(|| value.get("user_id"))
        .and_then(json_to_i64)?;
    let role = value
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    Some(SessionIdentity {
        user_id: i32::try_from(user_id).ok()?,
        role,
    })
}

/// Read an integer that may be encoded as a JSON number or numeric string.
fn json_to_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// In-memory session store
pub struct SessionStore {
    /// Sessions indexed by session ID (typed sessions for auth flow)
    sessions: HashMap<String, SessionData>,
    /// Raw key-value session storage (for plugin API: _session_store/_session_get)
    raw_data: HashMap<String, (String, Instant)>,
    /// CSRF tokens indexed by session ID
    csrf_tokens: HashMap<String, String>,
    /// Consumed JWT ids (jti) with the instant at which they expire from the
    /// revocation list. Used by `_jwt_refresh_and_rotate` to enforce single-use
    /// refresh tokens (spec AUTH-J007, AUTH-J009).
    consumed_jti: HashMap<String, Instant>,
    /// Active password reset tokens, keyed by the *hex sha256* of the plaintext
    /// token so a store dump does not leak usable tokens. Value is
    /// `(user_id, expires_at)`. Consumed atomically via `consume_reset_token()`
    /// under the SessionStore write lock.
    password_resets: HashMap<String, (i32, Instant)>,
    /// Configuration
    config: SessionConfig,
}

impl SessionStore {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            sessions: HashMap::new(),
            raw_data: HashMap::new(),
            csrf_tokens: HashMap::new(),
            consumed_jti: HashMap::new(),
            password_resets: HashMap::new(),
            config,
        }
    }

    /// Create a new session
    pub fn create(&mut self, user_id: i32, role: &str, claims: &str) -> SessionData {
        let session = SessionData::new(user_id, role.to_string(), claims.to_string());
        let session_id = session.session_id.clone();

        info!("Creating session {} for user {}", session_id, user_id);
        self.sessions.insert(session_id, session.clone());

        session
    }

    /// Get a session by ID
    pub fn get(&mut self, session_id: &str) -> Option<SessionData> {
        let timeout = Duration::from_secs(self.config.timeout_seconds);

        if let Some(session) = self.sessions.get_mut(session_id) {
            if session.is_expired(timeout) {
                debug!("Session {} has expired, removing", session_id);
                self.sessions.remove(session_id);
                return None;
            }
            session.touch();
            return Some(session.clone());
        }
        None
    }

    /// Delete a session
    pub fn delete(&mut self, session_id: &str) -> bool {
        info!("Deleting session {}", session_id);
        self.sessions.remove(session_id).is_some()
    }

    /// Get session configuration
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Replace the session configuration (cookie name, attributes, timeout).
    /// Called once at startup with the application's `auth: session:` settings.
    pub fn set_config(&mut self, config: SessionConfig) {
        info!(
            "Session cookie '{}' (timeout {}s)",
            config.cookie_name, config.timeout_seconds
        );
        self.config = config;
    }

    /// The session id carried by a `Cookie` request header, read from the
    /// configured session cookie name.
    pub fn session_id_from_cookie_header(&self, cookie_header: &str) -> Option<String> {
        parse_cookies(cookie_header)
            .remove(&self.config.cookie_name)
            .filter(|id| !id.is_empty())
    }

    /// Resolve the identity of a live session: a typed session (created by
    /// `_auth_set_session`) or a raw session blob (stored by `_session_store`).
    /// Both are touched; expired entries are removed and yield `None`.
    pub fn identity(&mut self, session_id: &str) -> Option<SessionIdentity> {
        if let Some(session) = self.get(session_id) {
            return Some(SessionIdentity {
                user_id: session.user_id,
                role: session.role,
            });
        }
        if !self.raw_data.contains_key(session_id) {
            return None;
        }
        let data = self.get_raw(session_id)?;
        identity_from_session_json(&data)
    }

    /// Format a Set-Cookie header for the session
    pub fn format_cookie(&self, session_id: &str) -> String {
        let mut cookie = format!(
            "{}={}; Path={}",
            self.config.cookie_name, session_id, self.config.cookie_path
        );

        if self.config.http_only {
            cookie.push_str("; HttpOnly");
        }
        if self.config.secure {
            cookie.push_str("; Secure");
        }
        cookie.push_str(&format!("; SameSite={}", self.config.same_site));

        // Add max-age
        cookie.push_str(&format!("; Max-Age={}", self.config.timeout_seconds));

        cookie
    }

    /// Format a cookie header that clears the session
    pub fn format_clear_cookie(&self) -> String {
        format!(
            "{}=; Path={}; Max-Age=0; HttpOnly",
            self.config.cookie_name, self.config.cookie_path
        )
    }

    // =========================================
    // RAW KEY-VALUE SESSION STORAGE
    // =========================================

    /// Store raw data by session ID (for plugin API)
    pub fn store_raw(&mut self, session_id: &str, data: &str) -> bool {
        debug!("Storing raw session data for {}", session_id);
        self.raw_data
            .insert(session_id.to_string(), (data.to_string(), Instant::now()));
        true
    }

    /// Get raw session data by ID (with expiration check)
    pub fn get_raw(&mut self, session_id: &str) -> Option<String> {
        let timeout = Duration::from_secs(self.config.timeout_seconds);

        if let Some((data, last_accessed)) = self.raw_data.get(session_id) {
            if last_accessed.elapsed() > timeout {
                debug!("Raw session {} expired, removing", session_id);
                self.raw_data.remove(session_id);
                return None;
            }
            let result = data.clone();
            // Touch - update timestamp
            if let Some(entry) = self.raw_data.get_mut(session_id) {
                entry.1 = Instant::now();
            }
            return Some(result);
        }

        // Also check typed sessions for backward compatibility
        if let Some(session) = self.sessions.get_mut(session_id) {
            if session.is_expired(timeout) {
                self.sessions.remove(session_id);
                return None;
            }
            session.touch();
            let json = serde_json::json!({
                "userId": session.user_id,
                "role": session.role,
                "sessionId": session.session_id,
                "claims": session.claims
            })
            .to_string();
            return Some(json);
        }

        None
    }

    /// Delete raw session data by ID
    pub fn delete_raw(&mut self, session_id: &str) -> bool {
        let raw_removed = self.raw_data.remove(session_id).is_some();
        let typed_removed = self.sessions.remove(session_id).is_some();
        // Also clean up CSRF token for this session
        self.csrf_tokens.remove(session_id);
        raw_removed || typed_removed
    }

    /// Check if a session exists (raw or typed)
    pub fn exists_raw(&self, session_id: &str) -> bool {
        self.raw_data.contains_key(session_id) || self.sessions.contains_key(session_id)
    }

    // =========================================
    // CSRF TOKEN MANAGEMENT
    // =========================================

    /// Set CSRF token for a session
    pub fn set_csrf(&mut self, session_id: &str, token: &str) {
        debug!("Setting CSRF token for session {}", session_id);
        self.csrf_tokens
            .insert(session_id.to_string(), token.to_string());
    }

    /// Get CSRF token for a session
    pub fn get_csrf(&self, session_id: &str) -> Option<String> {
        self.csrf_tokens.get(session_id).cloned()
    }

    /// Mark a JWT `jti` as consumed. Returns `false` if the `jti` was already
    /// present (i.e. a replay attempt); returns `true` on first consumption.
    ///
    /// `ttl` is how long the record should live in the revocation list — should
    /// match the original token's remaining lifetime so the entry is still
    /// blocking replays right up to natural expiry.
    pub fn mark_jti_consumed(&mut self, jti: &str, ttl: Duration) -> bool {
        // Opportunistically drop any expired entries so this map does not grow
        // unbounded when tokens are refreshed frequently.
        let now = Instant::now();
        self.consumed_jti.retain(|_, expires_at| *expires_at > now);

        if self.consumed_jti.contains_key(jti) {
            return false;
        }
        self.consumed_jti.insert(jti.to_string(), now + ttl);
        true
    }

    /// Returns true if `jti` has been consumed (and is still within its TTL).
    pub fn is_jti_consumed(&self, jti: &str) -> bool {
        match self.consumed_jti.get(jti) {
            Some(expires_at) => *expires_at > Instant::now(),
            None => false,
        }
    }

    /// Store a password reset token. Callers pass the SHA-256 hex digest of
    /// the plaintext token — the plaintext is never held in the store so a
    /// memory dump does not leak usable tokens. `ttl` is measured from now.
    ///
    /// Returns `true` if the row was inserted, `false` if a row with the same
    /// hash was already present (collision on cryptographic random is
    /// effectively impossible; this only happens if the caller supplies a
    /// pre-existing hash).
    pub fn store_reset_token(&mut self, token_hash: &str, user_id: i32, ttl: Duration) -> bool {
        let now = Instant::now();
        // Opportunistic cleanup keeps this map from growing unbounded when
        // users churn reset requests without following through.
        self.password_resets
            .retain(|_, (_, expires_at)| *expires_at > now);

        if self.password_resets.contains_key(token_hash) {
            return false;
        }
        self.password_resets
            .insert(token_hash.to_string(), (user_id, now + ttl));
        true
    }

    /// Atomically validate and consume a password reset token. If the hash is
    /// present AND not expired, the row is removed and the associated user_id
    /// is returned. Returns `None` on invalid, expired, or already-consumed.
    ///
    /// Atomicity comes from the fact that both the check and the remove
    /// happen while the caller holds the `SessionStore` write lock — a second
    /// caller attempting the same hash will observe the row already gone.
    pub fn consume_reset_token(&mut self, token_hash: &str) -> Option<i32> {
        let now = Instant::now();
        match self.password_resets.remove(token_hash) {
            Some((user_id, expires_at)) if expires_at > now => Some(user_id),
            _ => None,
        }
    }

    /// Cleanup expired sessions (call periodically)
    pub fn cleanup_expired(&mut self) -> usize {
        let timeout = Duration::from_secs(self.config.timeout_seconds);
        let before = self.sessions.len() + self.raw_data.len();
        let now = Instant::now();

        self.sessions
            .retain(|_, session| !session.is_expired(timeout));
        self.raw_data
            .retain(|_, (_, last_accessed)| last_accessed.elapsed() <= timeout);
        self.consumed_jti.retain(|_, expires_at| *expires_at > now);
        self.password_resets
            .retain(|_, (_, expires_at)| *expires_at > now);

        let after = self.sessions.len() + self.raw_data.len();
        let removed = before - after;
        if removed > 0 {
            info!("Cleaned up {} expired sessions", removed);
        }
        removed
    }

    /// Get count of active sessions
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Check if store is empty
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// Shared session store type
pub type SharedSessionStore = Arc<RwLock<SessionStore>>;

/// Create a shared session store
pub fn create_session_store(config: SessionConfig) -> SharedSessionStore {
    Arc::new(RwLock::new(SessionStore::new(config)))
}

/// Parse cookies from a Cookie header value
/// Returns a HashMap of cookie name -> value
pub fn parse_cookies(cookie_header: &str) -> HashMap<String, String> {
    let mut cookies = HashMap::new();

    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(eq_pos) = part.find('=') {
            let name = part[..eq_pos].trim();
            let value = part[eq_pos + 1..].trim();
            // Remove surrounding quotes if present
            let value = value.trim_matches('"');
            cookies.insert(name.to_string(), value.to_string());
        }
    }

    cookies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let config = SessionConfig::default();
        let mut store = SessionStore::new(config);

        let session = store.create(1, "user", r#"{"email":"test@example.com"}"#);
        assert_eq!(session.user_id, 1);
        assert_eq!(session.role, "user");
        assert!(!session.session_id.is_empty());
    }

    #[test]
    fn test_session_get() {
        let config = SessionConfig::default();
        let mut store = SessionStore::new(config);

        let session = store.create(1, "admin", "{}");
        let session_id = session.session_id.clone();

        let retrieved = store.get(&session_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().user_id, 1);
    }

    #[test]
    fn test_session_delete() {
        let config = SessionConfig::default();
        let mut store = SessionStore::new(config);

        let session = store.create(1, "user", "{}");
        let session_id = session.session_id.clone();

        assert!(store.delete(&session_id));
        assert!(store.get(&session_id).is_none());
    }

    #[test]
    fn test_session_expiry() {
        let config = SessionConfig {
            timeout_seconds: 0, // Immediate expiry
            ..SessionConfig::default()
        };
        let mut store = SessionStore::new(config);

        let session = store.create(1, "user", "{}");
        let session_id = session.session_id.clone();

        // Session should be expired immediately
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(store.get(&session_id).is_none());
    }

    #[test]
    fn test_parse_cookies() {
        let cookies = parse_cookies("session=abc123; theme=dark; lang=en");

        assert_eq!(cookies.get("session"), Some(&"abc123".to_string()));
        assert_eq!(cookies.get("theme"), Some(&"dark".to_string()));
        assert_eq!(cookies.get("lang"), Some(&"en".to_string()));
    }

    #[test]
    fn test_jti_first_consumption_succeeds_replay_fails() {
        let mut store = SessionStore::new(SessionConfig::default());
        let ttl = Duration::from_secs(60);

        assert!(store.mark_jti_consumed("jti-abc", ttl));
        assert!(store.is_jti_consumed("jti-abc"));
        // Second call with the same jti must return false — this is the replay
        // guard the spec (AUTH-J007, AUTH-J009) requires.
        assert!(!store.mark_jti_consumed("jti-abc", ttl));
    }

    #[test]
    fn test_jti_expires_after_ttl() {
        let mut store = SessionStore::new(SessionConfig::default());
        // Zero-duration entry is considered expired on the next `Instant::now()`.
        assert!(store.mark_jti_consumed("jti-short", Duration::from_millis(0)));
        std::thread::sleep(Duration::from_millis(5));
        // Expired entries are treated as not-consumed so the caller can reissue.
        assert!(!store.is_jti_consumed("jti-short"));
    }

    #[test]
    fn test_reset_token_store_and_consume() {
        let mut store = SessionStore::new(SessionConfig::default());
        let ttl = Duration::from_secs(900);
        // Not-a-real-hash, but store treats it as an opaque key.
        let hash = "0123456789abcdef".repeat(4);

        assert!(store.store_reset_token(&hash, 42, ttl));
        // First consume returns the user id; second consume must return None
        // — this is the atomic single-use guarantee frame.auth relies on.
        assert_eq!(store.consume_reset_token(&hash), Some(42));
        assert_eq!(store.consume_reset_token(&hash), None);
    }

    #[test]
    fn test_reset_token_expired_returns_none() {
        let mut store = SessionStore::new(SessionConfig::default());
        let hash = "abababab".repeat(8);
        // TTL of 0 -> row is expired the moment the next Instant::now() fires.
        assert!(store.store_reset_token(&hash, 7, Duration::from_millis(0)));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(store.consume_reset_token(&hash), None);
    }

    #[test]
    fn test_config_from_set_cookie() {
        let cfg = SessionConfig::from_set_cookie(
            "app.sid=probe; Path=/app; SameSite=Strict; Secure; HttpOnly; Max-Age=300",
        )
        .expect("config");
        assert_eq!(cfg.cookie_name, "app.sid");
        assert_eq!(cfg.cookie_path, "/app");
        assert_eq!(cfg.same_site, "Strict");
        assert!(cfg.secure);
        assert!(cfg.http_only);
        assert_eq!(cfg.timeout_seconds, 300);

        let plain =
            SessionConfig::from_set_cookie("frame.sid=x; Path=/; SameSite=Lax").expect("config");
        assert_eq!(plain.cookie_name, "frame.sid");
        assert!(!plain.secure);
        assert!(!plain.http_only);
        assert!(SessionConfig::from_set_cookie("=x; Path=/").is_none());
    }

    #[test]
    fn test_session_id_from_configured_cookie() {
        let mut store = SessionStore::new(SessionConfig::default());
        store.set_config(SessionConfig {
            cookie_name: "frame.sid".to_string(),
            ..SessionConfig::default()
        });
        assert_eq!(
            store.session_id_from_cookie_header("theme=dark; frame.sid=abc"),
            Some("abc".to_string())
        );
        // A cookie under another name is not the session cookie.
        assert_eq!(store.session_id_from_cookie_header("session=abc"), None);
    }

    #[test]
    fn test_identity_from_raw_session_blob() {
        let mut store = SessionStore::new(SessionConfig::default());
        store.store_raw(
            "sid-1",
            r#"{"userId":7,"claims":{"role":"user"},"expiry":0}"#,
        );
        assert_eq!(
            store.identity("sid-1"),
            Some(SessionIdentity {
                user_id: 7,
                role: "user".to_string()
            })
        );
        assert_eq!(store.identity("missing"), None);
    }

    #[test]
    fn test_identity_from_typed_session() {
        let mut store = SessionStore::new(SessionConfig::default());
        let session = store.create(3, "admin", "{}");
        assert_eq!(
            store.identity(&session.session_id),
            Some(SessionIdentity {
                user_id: 3,
                role: "admin".to_string()
            })
        );
    }

    #[test]
    fn test_identity_rejects_expired_or_anonymous_blob() {
        // expiry in seconds, in the past
        assert_eq!(
            identity_from_session_json(r#"{"userId":1,"claims":{},"expiry":1000}"#),
            None
        );
        // expiry in milliseconds, in the future
        let future_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 60_000) as i64;
        let blob = format!(r#"{{"userId":1,"claims":"{{\"role\":\"x\"}}","expiry":{future_ms}}}"#);
        assert_eq!(
            identity_from_session_json(&blob),
            Some(SessionIdentity {
                user_id: 1,
                role: "x".to_string()
            })
        );
        assert_eq!(identity_from_session_json(r#"{"claims":{}}"#), None);
    }

    #[test]
    fn test_identity_from_jwt_claims() {
        assert_eq!(
            identity_from_jwt_claims(r#"{"sub":7,"role":"user","exp":1}"#),
            Some(SessionIdentity {
                user_id: 7,
                role: "user".to_string()
            })
        );
        assert_eq!(
            identity_from_jwt_claims(r#"{"sub":"42"}"#).map(|i| i.user_id),
            Some(42)
        );
        assert_eq!(identity_from_jwt_claims(r#"{"role":"user"}"#), None);
        // frame.auth jwt.sign payload: application claims nested under `claims`.
        assert_eq!(
            identity_from_jwt_claims(
                r#"{"iat":1,"exp":2,"jti":"x","claims":{"sub":7,"role":"user"}}"#
            ),
            Some(SessionIdentity {
                user_id: 7,
                role: "user".to_string()
            })
        );
    }

    #[test]
    fn test_format_cookie() {
        let config = SessionConfig {
            cookie_name: "mysession".to_string(),
            cookie_path: "/app".to_string(),
            same_site: "Strict".to_string(),
            secure: true,
            http_only: true,
            timeout_seconds: 3600,
        };
        let store = SessionStore::new(config);

        let cookie = store.format_cookie("test123");
        assert!(cookie.contains("mysession=test123"));
        assert!(cookie.contains("Path=/app"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=3600"));
    }
}
