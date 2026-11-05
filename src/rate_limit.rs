//! Fixed-window rate limiter and axum middleware.
//!
//! Configured by `_rate_limit_configure` during WASM init. Strategy is either
//! `ip` (key by `X-Forwarded-For`/`X-Real-IP`, falling back to a constant) or
//! `user` (key by the application's session cookie, falling back to ip).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::Response,
};
use parking_lot::Mutex;

use crate::runtime_config::{RateLimitConfig, RateLimitStrategy};

#[derive(Debug)]
struct BucketState {
    remaining: u32,
    window_start: Instant,
}

#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimitConfig,
    /// Session cookie name used to key the `user` strategy.
    session_cookie: String,
    buckets: Mutex<HashMap<String, BucketState>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            session_cookie: crate::session::SessionConfig::default().cookie_name,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Key the `user` strategy by the application's session cookie.
    pub fn with_session_cookie(mut self, name: impl Into<String>) -> Self {
        self.session_cookie = name.into();
        self
    }

    pub fn config(&self) -> &RateLimitConfig {
        &self.config
    }

    /// Consume one token for `key`. Returns `true` when the request is allowed.
    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let window = self.config.window_secs as u64;
        let mut buckets = self.buckets.lock();
        let entry = buckets.entry(key.to_string()).or_insert(BucketState {
            remaining: self.config.per_window,
            window_start: now,
        });
        if now.duration_since(entry.window_start).as_secs() >= window {
            entry.remaining = self.config.per_window;
            entry.window_start = now;
        }
        if entry.remaining == 0 {
            return false;
        }
        entry.remaining -= 1;
        true
    }
}

pub type SharedRateLimiter = Arc<RateLimiter>;

/// Derive the rate-limit key from request headers using the configured strategy.
fn extract_key(strategy: RateLimitStrategy, session_cookie: &str, headers: &HeaderMap) -> String {
    fn header(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn session_from_cookie(headers: &HeaderMap, session_cookie: &str) -> Option<String> {
        let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
        crate::session::parse_cookies(cookie_header)
            .remove(session_cookie)
            .filter(|value| !value.is_empty())
            .map(|value| format!("user:{}", value))
    }

    let ip_key = || {
        let ip = header(headers, "x-forwarded-for")
            .or_else(|| header(headers, "x-real-ip"))
            .unwrap_or_else(|| "unknown".to_string());
        format!("ip:{}", ip)
    };

    match strategy {
        RateLimitStrategy::Ip => ip_key(),
        RateLimitStrategy::User => {
            session_from_cookie(headers, session_cookie).unwrap_or_else(ip_key)
        }
    }
}

/// axum middleware that enforces the rate limit configured in `SharedRateLimiter`.
pub async fn rate_limit_middleware(
    State(limiter): State<SharedRateLimiter>,
    req: Request,
    next: Next,
) -> Response {
    let key = extract_key(
        limiter.config().strategy,
        &limiter.session_cookie,
        req.headers(),
    );
    if limiter.allow(&key) {
        return next.run(req).await;
    }
    let retry_after = limiter.config().window_secs.to_string();
    let body = format!(
        r#"{{"ok":false,"error":"rate_limit_exceeded","retry_after":{}}}"#,
        limiter.config().window_secs
    );
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(header::CONTENT_TYPE, "application/json")
        .header("Retry-After", retry_after)
        .body(Body::from(body))
        .expect("rate-limit response builder")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn user_strategy_keys_by_configured_session_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("frame.sid=abc; sid=zzz"),
        );
        assert_eq!(
            extract_key(RateLimitStrategy::User, "frame.sid", &headers),
            "user:abc"
        );
        // Without the configured cookie the key falls back to the client ip.
        assert_eq!(
            extract_key(RateLimitStrategy::User, "app.sid", &headers),
            "ip:unknown"
        );
    }
}
