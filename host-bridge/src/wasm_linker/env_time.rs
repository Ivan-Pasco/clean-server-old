//! Environment & Time Host Functions (Layer 2)
//!
//! Provides environment and time operations for WASM modules:
//! - _env_get: Get environment variable value
//! - _time_now: Get current Unix timestamp in seconds
//!
//! All functions are generic over `WasmStateCore` to work with any runtime.

use super::helpers::{read_raw_string, write_string_to_caller};
use super::state::WasmStateCore;
use crate::error::BridgeResult;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike, Utc};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error};
use wasmtime::{Caller, Linker};

const SENSITIVE_SUBSTRINGS: &[&str] = &[
    "SECRET",
    "PASSWORD",
    "TOKEN",
    "PRIVATE",
    "AWS_SECRET",
    "API_KEY",
];

fn is_sensitive_env_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    SENSITIVE_SUBSTRINGS.iter().any(|p| upper.contains(p))
}

/// Register environment and time functions with the linker
pub fn register_functions<S: WasmStateCore>(linker: &mut Linker<S>) -> BridgeResult<()> {
    // =========================================
    // ENVIRONMENT
    // =========================================

    // _env_get - Get environment variable value
    // Args: name_ptr, name_len
    // Returns: pointer to value string (length-prefixed), empty if not found
    linker.func_wrap(
        "env",
        "_env_get",
        |mut caller: Caller<'_, S>, name_ptr: i32, name_len: i32| -> i32 {
            let name = match read_raw_string(&mut caller, name_ptr, name_len) {
                Some(s) => s,
                None => {
                    error!("_env_get: Failed to read variable name");
                    return write_string_to_caller(&mut caller, "");
                }
            };

            // Validate variable name (alphanumeric + underscore only)
            if !name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
            {
                error!("_env_get: Invalid variable name '{}'", name);
                return write_string_to_caller(&mut caller, "");
            }

            // Security: deny access to sensitive variables
            let denied = [
                "AWS_SECRET_ACCESS_KEY",
                "PRIVATE_KEY",
                "ENCRYPTION_KEY",
                "SSH_AUTH_SOCK",
                "GPG_PASSPHRASE",
            ];
            let upper = name.to_uppercase();
            if denied.iter().any(|d| upper.contains(d)) {
                error!("_env_get: Access denied for '{}'", name);
                return write_string_to_caller(&mut caller, "");
            }

            debug!("_env_get: reading '{}'", name);

            match std::env::var(&name) {
                Ok(value) => write_string_to_caller(&mut caller, &value),
                Err(_) => write_string_to_caller(&mut caller, ""),
            }
        },
    )?;

    // =========================================
    // TIME
    // =========================================

    // _time_now - Get current Unix timestamp in seconds
    // Args: none
    // Returns: i64 timestamp (seconds since epoch)
    linker.func_wrap("env", "_time_now", |_caller: Caller<'_, S>| -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    })?;

    // =========================================
    // ENV EXTRAS (Phase 2)
    // =========================================

    // _env_has(string) -> boolean
    linker.func_wrap(
        "env",
        "_env_has",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> i32 {
            let name = match read_raw_string(&mut caller, p, l) {
                Some(s) => s,
                None => return 0,
            };
            if std::env::var(&name).is_ok() {
                1
            } else {
                0
            }
        },
    )?;

    // _env_all() -> ptr (JSON map, sensitive keys filtered out)
    linker.func_wrap("env", "_env_all", |mut caller: Caller<'_, S>| -> i32 {
        let mut map = serde_json::Map::new();
        for (k, v) in std::env::vars() {
            if !is_sensitive_env_name(&k) {
                map.insert(k, serde_json::Value::String(v));
            }
        }
        write_string_to_caller(&mut caller, &serde_json::Value::Object(map).to_string())
    })?;

    // _env_node_env() -> ptr (NODE_ENV or "development")
    linker.func_wrap("env", "_env_node_env", |mut caller: Caller<'_, S>| -> i32 {
        let v = std::env::var("NODE_ENV").unwrap_or_else(|_| "development".to_string());
        write_string_to_caller(&mut caller, &v)
    })?;

    // _env_is_production() -> boolean
    linker.func_wrap("env", "_env_is_production", |_: Caller<'_, S>| -> i32 {
        if std::env::var("NODE_ENV").as_deref() == Ok("production") {
            1
        } else {
            0
        }
    })?;

    // _env_is_development() -> boolean (NODE_ENV != production)
    linker.func_wrap("env", "_env_is_development", |_: Caller<'_, S>| -> i32 {
        if std::env::var("NODE_ENV").as_deref() == Ok("production") {
            0
        } else {
            1
        }
    })?;

    // =========================================
    // TIME EXTRAS (Phase 2)
    // =========================================

    // _time_epoch_ms() -> i64
    linker.func_wrap("env", "_time_epoch_ms", |_: Caller<'_, S>| -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })?;

    // _time_epoch_sec() -> i64
    linker.func_wrap("env", "_time_epoch_sec", |_: Caller<'_, S>| -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    })?;

    // _time_iso() -> ptr — current time as ISO 8601 UTC
    linker.func_wrap("env", "_time_iso", |mut caller: Caller<'_, S>| -> i32 {
        let s = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        write_string_to_caller(&mut caller, &s)
    })?;

    // Points in time are Unix timestamps in SECONDS, like `_time_now`
    // (stdlib-reference §12). Durations below a second stay in milliseconds
    // (`_time_epoch_ms`, `_time_sleep`).

    // _time_format_iso(i64 epoch_seconds) -> ptr
    linker.func_wrap(
        "env",
        "_time_format_iso",
        |mut caller: Caller<'_, S>, epoch_seconds: i64| -> i32 {
            let out = format_iso(epoch_seconds);
            write_string_to_caller(&mut caller, &out)
        },
    )?;

    // _time_parse_iso(string) -> i64 epoch_seconds (-1 on invalid)
    linker.func_wrap(
        "env",
        "_time_parse_iso",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> i64 {
            let s = read_raw_string(&mut caller, p, l).unwrap_or_default();
            parse_iso(&s)
        },
    )?;

    // _time_components(i64 epoch_seconds) -> ptr (JSON of local-time components)
    linker.func_wrap(
        "env",
        "_time_components",
        |mut caller: Caller<'_, S>, epoch_seconds: i64| -> i32 {
            let payload = components_json(epoch_seconds);
            write_string_to_caller(&mut caller, &payload)
        },
    )?;

    // _time_from_components(y, m, d, h, min, s) -> i64 epoch_seconds (local time)
    linker.func_wrap(
        "env",
        "_time_from_components",
        |_: Caller<'_, S>, y: i32, m: i32, d: i32, h: i32, mi: i32, s: i32| -> i64 {
            from_components(y, m, d, h, mi, s)
        },
    )?;

    // =========================================
    // time.* (stdlib-reference §12): timestamps are f64 Unix seconds
    // =========================================

    // _time_format(ts, pattern, locale, tz) -> ptr
    linker.func_wrap(
        "env",
        "_time_format",
        |mut caller: Caller<'_, S>,
         ts: f64,
         pattern_ptr: i32,
         pattern_len: i32,
         locale_ptr: i32,
         locale_len: i32,
         tz_ptr: i32,
         tz_len: i32|
         -> i32 {
            let pattern =
                read_raw_string(&mut caller, pattern_ptr, pattern_len).unwrap_or_default();
            let locale = read_raw_string(&mut caller, locale_ptr, locale_len).unwrap_or_default();
            let tz = read_raw_string(&mut caller, tz_ptr, tz_len).unwrap_or_default();
            let out = time_format(ts, &pattern, &locale, &tz);
            write_string_to_caller(&mut caller, &out)
        },
    )?;

    // _time_relative(ts, locale) -> ptr
    linker.func_wrap(
        "env",
        "_time_relative",
        |mut caller: Caller<'_, S>, ts: f64, locale_ptr: i32, locale_len: i32| -> i32 {
            let locale = read_raw_string(&mut caller, locale_ptr, locale_len).unwrap_or_default();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let out = time_relative_from(ts, now, &locale);
            write_string_to_caller(&mut caller, &out)
        },
    )?;

    // _time_parse(iso) -> f64 Unix seconds (-1.0 on failure)
    linker.func_wrap(
        "env",
        "_time_parse",
        |mut caller: Caller<'_, S>, p: i32, l: i32| -> f64 {
            let s = read_raw_string(&mut caller, p, l).unwrap_or_default();
            time_parse(&s)
        },
    )?;

    // _time_parts(ts, tz) -> ptr (JSON)
    linker.func_wrap(
        "env",
        "_time_parts",
        |mut caller: Caller<'_, S>, ts: f64, tz_ptr: i32, tz_len: i32| -> i32 {
            let tz = read_raw_string(&mut caller, tz_ptr, tz_len).unwrap_or_default();
            let out = time_parts(ts, &tz);
            write_string_to_caller(&mut caller, &out)
        },
    )?;

    // _time_add(ts, amount, unit) -> f64 Unix seconds
    linker.func_wrap(
        "env",
        "_time_add",
        |mut caller: Caller<'_, S>, ts: f64, amount: i32, unit_ptr: i32, unit_len: i32| -> f64 {
            let unit = read_raw_string(&mut caller, unit_ptr, unit_len).unwrap_or_default();
            time_add(ts, amount, &unit)
        },
    )?;

    // _time_diff(from, to, unit) -> f64 (to - from in `unit`, fractional)
    linker.func_wrap(
        "env",
        "_time_diff",
        |mut caller: Caller<'_, S>, from: f64, to: f64, unit_ptr: i32, unit_len: i32| -> f64 {
            let unit = read_raw_string(&mut caller, unit_ptr, unit_len).unwrap_or_default();
            time_diff(from, to, &unit)
        },
    )?;

    // _time_format_locale(epoch_seconds, locale) -> ptr
    // Formats as local `YYYY-MM-DD HH:MM:SS`; the locale is accepted for
    // signature compatibility.
    linker.func_wrap(
        "env",
        "_time_format_locale",
        |mut caller: Caller<'_, S>, epoch_seconds: i64, p: i32, l: i32| -> i32 {
            let _locale = read_raw_string(&mut caller, p, l).unwrap_or_default();
            let formatted = Local
                .timestamp_opt(epoch_seconds, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            write_string_to_caller(&mut caller, &formatted)
        },
    )?;

    // _time_timezone_offset() -> i32 minutes (positive = west of UTC, matches JS Date#getTimezoneOffset)
    linker.func_wrap("env", "_time_timezone_offset", |_: Caller<'_, S>| -> i32 {
        let local_off_secs = Local::now().offset().local_minus_utc();
        -(local_off_secs / 60)
    })?;

    // _time_is_past(epoch_seconds) -> boolean
    linker.func_wrap("env", "_time_is_past", |_: Caller<'_, S>, e: i64| -> i32 {
        i32::from(is_past(e))
    })?;

    // _time_is_future(epoch_seconds) -> boolean
    linker.func_wrap(
        "env",
        "_time_is_future",
        |_: Caller<'_, S>, e: i64| -> i32 { i32::from(is_future(e)) },
    )?;

    // _time_sleep(ms) -> void — a duration, in milliseconds (sync sleep per spec; docs note divergence in HOST_BRIDGE.md)
    linker.func_wrap("env", "_time_sleep", |_: Caller<'_, S>, ms: i32| {
        let ms = ms.max(0) as u64;
        std::thread::sleep(std::time::Duration::from_millis(ms));
    })?;

    Ok(())
}

/// Current Unix time in seconds.
fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// RFC 3339 UTC rendering of a Unix timestamp in seconds ("" when out of range).
fn format_iso(epoch_seconds: i64) -> String {
    Utc.timestamp_opt(epoch_seconds, 0)
        .single()
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// Parse an RFC 3339 timestamp to Unix seconds; -1 when invalid.
fn parse_iso(s: &str) -> i64 {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|d| d.timestamp())
        .unwrap_or(-1)
}

/// Local-time components of a Unix timestamp in seconds, as JSON.
fn components_json(epoch_seconds: i64) -> String {
    match Local.timestamp_opt(epoch_seconds, 0).single() {
        Some(dt) => json!({
            "year": dt.year(),
            "month": dt.month(),
            "day": dt.day(),
            "hour": dt.hour(),
            "minute": dt.minute(),
            "second": dt.second(),
            "dayOfWeek": dt.weekday().num_days_from_sunday(),
        })
        .to_string(),
        None => "{}".to_string(),
    }
}

/// Unix seconds for local-time components; -1 when they name no single instant.
fn from_components(y: i32, m: i32, d: i32, h: i32, mi: i32, s: i32) -> i64 {
    match Local.with_ymd_and_hms(y, m as u32, d as u32, h as u32, mi as u32, s as u32) {
        chrono::offset::LocalResult::Single(dt) => dt.timestamp(),
        _ => -1,
    }
}

fn is_past(epoch_seconds: i64) -> bool {
    epoch_seconds < now_seconds()
}

fn is_future(epoch_seconds: i64) -> bool {
    epoch_seconds > now_seconds()
}

// ---------------------------------------------------------------------------
// time.* implementation (stdlib-reference §12, HOST_BRIDGE.md Date / Time)
// ---------------------------------------------------------------------------

/// A UTC instant for a fractional Unix timestamp in seconds.
fn utc_instant(ts: f64) -> Option<DateTime<Utc>> {
    if !ts.is_finite() {
        return None;
    }
    let secs = ts.floor();
    let nanos = ((ts - secs) * 1e9).round().min(999_999_999.0) as u32;
    Utc.timestamp_opt(secs as i64, nanos).single()
}

/// Fractional Unix seconds of an instant.
fn unix_seconds<Tz: TimeZone>(dt: &DateTime<Tz>) -> f64 {
    dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_nanos()) / 1e9
}

/// Calendar fields of `ts` in `tz` ("" = local timezone, otherwise an IANA
/// name; an unknown name falls back to UTC).
struct CalendarFields {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    weekday: u32,
}

fn calendar_fields(ts: f64, tz: &str) -> Option<CalendarFields> {
    fn fields<T: TimeZone>(dt: DateTime<T>) -> CalendarFields {
        CalendarFields {
            year: dt.year(),
            month: dt.month(),
            day: dt.day(),
            hour: dt.hour(),
            minute: dt.minute(),
            second: dt.second(),
            weekday: dt.weekday().num_days_from_sunday(),
        }
    }
    let utc = utc_instant(ts)?;
    let tz = tz.trim();
    if tz.is_empty() {
        return Some(fields(utc.with_timezone(&Local)));
    }
    match tz.parse::<chrono_tz::Tz>() {
        Ok(zone) => Some(fields(utc.with_timezone(&zone))),
        Err(_) => {
            debug!("time: unknown timezone {:?}, using UTC", tz);
            Some(fields(utc))
        }
    }
}

/// `time.format`: tokens YYYY MM DD HH mm ss h A a; other characters are
/// copied as-is. Returns "" for a non-finite timestamp.
fn time_format(ts: f64, pattern: &str, _locale: &str, tz: &str) -> String {
    let f = match calendar_fields(ts, tz) {
        Some(f) => f,
        None => return String::new(),
    };
    let hour12 = match f.hour % 12 {
        0 => 12,
        h => h,
    };
    let pm = f.hour >= 12;
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut rest = pattern;
    while !rest.is_empty() {
        let (piece, len) = if rest.starts_with("YYYY") {
            (format!("{:04}", f.year), 4)
        } else if rest.starts_with("MM") {
            (format!("{:02}", f.month), 2)
        } else if rest.starts_with("DD") {
            (format!("{:02}", f.day), 2)
        } else if rest.starts_with("HH") {
            (format!("{:02}", f.hour), 2)
        } else if rest.starts_with("mm") {
            (format!("{:02}", f.minute), 2)
        } else if rest.starts_with("ss") {
            (format!("{:02}", f.second), 2)
        } else if rest.starts_with('h') {
            (hour12.to_string(), 1)
        } else if rest.starts_with('A') {
            ((if pm { "PM" } else { "AM" }).to_string(), 1)
        } else if rest.starts_with('a') {
            ((if pm { "pm" } else { "am" }).to_string(), 1)
        } else {
            let c = rest.chars().next().unwrap_or_default();
            (c.to_string(), c.len_utf8())
        };
        out.push_str(&piece);
        rest = &rest[len..];
    }
    out
}

/// `time.parse`: ISO 8601 to fractional Unix seconds; -1.0 when invalid.
/// A date-only string is UTC midnight; a date-time without an offset is
/// local time (the ECMAScript `Date.parse` rules).
fn time_parse(s: &str) -> f64 {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return unix_seconds(&dt);
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d
            .and_hms_opt(0, 0, 0)
            .map(|n| unix_seconds(&n.and_utc()))
            .unwrap_or(-1.0);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return match Local.from_local_datetime(&n) {
                chrono::offset::LocalResult::Single(dt) => unix_seconds(&dt),
                chrono::offset::LocalResult::Ambiguous(dt, _) => unix_seconds(&dt),
                chrono::offset::LocalResult::None => -1.0,
            };
        }
    }
    -1.0
}

/// `time.parts`: calendar components of `ts` in `tz` as JSON.
fn time_parts(ts: f64, tz: &str) -> String {
    match calendar_fields(ts, tz) {
        Some(f) => json!({
            "year": f.year,
            "month": f.month,
            "day": f.day,
            "hour": f.hour,
            "minute": f.minute,
            "second": f.second,
            "dayOfWeek": f.weekday,
            "timestamp": if ts.fract() == 0.0 { json!(ts as i64) } else { json!(ts) },
        })
        .to_string(),
        None => "{}".to_string(),
    }
}

/// Seconds in a fixed-length unit; `None` for calendar units and unknown names.
fn fixed_unit_seconds(unit: &str) -> Option<f64> {
    match unit {
        "seconds" => Some(1.0),
        "minutes" => Some(60.0),
        "hours" => Some(3_600.0),
        "days" => Some(86_400.0),
        "weeks" => Some(604_800.0),
        _ => None,
    }
}

/// Add `months` calendar months (UTC), clamping the day to the target
/// month's last day (2026-01-31 + 1 month = 2026-02-28).
fn add_months(ts: f64, months: i64) -> f64 {
    let dt = match utc_instant(ts) {
        Some(dt) => dt,
        None => return f64::NAN,
    };
    let moved = if months >= 0 {
        dt.checked_add_months(chrono::Months::new(months as u32))
    } else {
        dt.checked_sub_months(chrono::Months::new(months.unsigned_abs() as u32))
    };
    moved.map(|d| unix_seconds(&d)).unwrap_or(f64::NAN)
}

/// `time.add`: `ts` plus `amount` units. Unknown units yield NaN.
fn time_add(ts: f64, amount: i32, unit: &str) -> f64 {
    let unit = unit.trim().to_ascii_lowercase();
    if let Some(secs) = fixed_unit_seconds(&unit) {
        return ts + f64::from(amount) * secs;
    }
    match unit.as_str() {
        "months" => add_months(ts, i64::from(amount)),
        "years" => add_months(ts, i64::from(amount) * 12),
        _ => f64::NAN,
    }
}

/// Calendar months from `from` to `to` (to >= from), fractional: whole
/// months by calendar stepping, plus the elapsed share of the next month.
fn months_between(from: f64, to: f64) -> f64 {
    let mut whole: i64 = 0;
    while add_months(from, whole + 1) <= to {
        whole += 1;
    }
    let start = add_months(from, whole);
    let next = add_months(from, whole + 1);
    let span = next - start;
    whole as f64 + if span > 0.0 { (to - start) / span } else { 0.0 }
}

/// `time.diff`: `to - from` in `unit`, fractional. Unknown units yield NaN.
fn time_diff(from: f64, to: f64, unit: &str) -> f64 {
    let unit = unit.trim().to_ascii_lowercase();
    if let Some(secs) = fixed_unit_seconds(&unit) {
        return (to - from) / secs;
    }
    let months = |a: f64, b: f64| {
        if b >= a {
            months_between(a, b)
        } else {
            -months_between(b, a)
        }
    };
    match unit.as_str() {
        "months" => months(from, to),
        "years" => months(from, to) / 12.0,
        _ => f64::NAN,
    }
}

/// `time.relative`: wording for `ts` relative to `now` (both Unix seconds).
/// Thresholds follow the common Intl.RelativeTimeFormat breakpoints; English,
/// Spanish, French, German and Portuguese are worded natively, other locales
/// use English.
fn time_relative_from(ts: f64, now: f64, locale: &str) -> String {
    let delta = ts - now;
    let abs = delta.abs();
    let lang = locale
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if abs < 45.0 {
        return match lang.as_str() {
            "es" => "ahora mismo",
            "fr" => "à l'instant",
            "de" => "gerade eben",
            "pt" => "agora mesmo",
            _ => "just now",
        }
        .to_string();
    }
    // (unit index, count): 0 minute, 1 hour, 2 day, 3 month, 4 year
    let (unit, n) = if abs < 45.0 * 60.0 {
        (0, (abs / 60.0).round())
    } else if abs < 22.0 * 3600.0 {
        (1, (abs / 3600.0).round())
    } else if abs < 26.0 * 86_400.0 {
        (2, (abs / 86_400.0).round())
    } else if abs < 320.0 * 86_400.0 {
        (3, (abs / (30.4375 * 86_400.0)).round())
    } else {
        (4, (abs / (365.25 * 86_400.0)).round())
    };
    let n = n.max(1.0) as i64;
    let one = n == 1;
    let past = delta < 0.0;
    let (sing, plur): (&str, &str) = match (lang.as_str(), unit) {
        ("es", 0) => ("minuto", "minutos"),
        ("es", 1) => ("hora", "horas"),
        ("es", 2) => ("día", "días"),
        ("es", 3) => ("mes", "meses"),
        ("es", _) => ("año", "años"),
        ("fr", 0) => ("minute", "minutes"),
        ("fr", 1) => ("heure", "heures"),
        ("fr", 2) => ("jour", "jours"),
        ("fr", 3) => ("mois", "mois"),
        ("fr", _) => ("an", "ans"),
        ("de", 0) => ("Minute", "Minuten"),
        ("de", 1) => ("Stunde", "Stunden"),
        ("de", 2) => ("Tag", "Tagen"),
        ("de", 3) => ("Monat", "Monaten"),
        ("de", _) => ("Jahr", "Jahren"),
        ("pt", 0) => ("minuto", "minutos"),
        ("pt", 1) => ("hora", "horas"),
        ("pt", 2) => ("dia", "dias"),
        ("pt", 3) => ("mês", "meses"),
        ("pt", _) => ("ano", "anos"),
        (_, 0) => ("minute", "minutes"),
        (_, 1) => ("hour", "hours"),
        (_, 2) => ("day", "days"),
        (_, 3) => ("month", "months"),
        (_, _) => ("year", "years"),
    };
    let word = if one { sing } else { plur };
    match (lang.as_str(), past) {
        ("es", true) => format!("hace {} {}", n, word),
        ("es", false) => format!("dentro de {} {}", n, word),
        ("fr", true) => format!("il y a {} {}", n, word),
        ("fr", false) => format!("dans {} {}", n, word),
        ("de", true) => format!("vor {} {}", n, word),
        ("de", false) => format!("in {} {}", n, word),
        ("pt", true) => format!("há {} {}", n, word),
        ("pt", false) => format!("em {} {}", n, word),
        (_, true) => format!("{} {} ago", n, word),
        (_, false) => format!("in {} {}", n, word),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-01-01T00:00:00Z
    const NEW_YEAR_2026: i64 = 1_767_225_600;

    // ---- stdlib-reference §12 time.* bridges -------------------------------

    #[test]
    fn time_format_tokens() {
        // 2026-01-01T14:05:09Z
        let ts = (NEW_YEAR_2026 + 14 * 3600 + 5 * 60 + 9) as f64;
        assert_eq!(
            time_format(ts, "YYYY-MM-DD HH:mm:ss", "", "UTC"),
            "2026-01-01 14:05:09"
        );
        assert_eq!(
            time_format(ts, "h:mm A / h a", "en-US", "UTC"),
            "2:05 PM / 2 pm"
        );
        // IANA timezone: New York is UTC-5 in January.
        assert_eq!(
            time_format(ts, "YYYY-MM-DD HH:mm", "", "America/New_York"),
            "2026-01-01 09:05"
        );
        // Midnight is 12 AM on the 12-hour clock.
        assert_eq!(time_format(NEW_YEAR_2026 as f64, "h A", "", "UTC"), "12 AM");
    }

    #[test]
    fn time_parse_iso_8601_to_seconds() {
        assert_eq!(time_parse("2026-01-01T00:00:00Z"), NEW_YEAR_2026 as f64);
        assert_eq!(
            time_parse("2026-01-01T00:00:00.500Z"),
            NEW_YEAR_2026 as f64 + 0.5
        );
        assert_eq!(
            time_parse("2026-01-01T01:00:00+01:00"),
            NEW_YEAR_2026 as f64
        );
        // Date-only forms are UTC midnight.
        assert_eq!(time_parse("2026-01-01"), NEW_YEAR_2026 as f64);
        assert_eq!(time_parse("yesterday"), -1.0);
    }

    #[test]
    fn time_parts_in_timezone() {
        let parts: serde_json::Value =
            serde_json::from_str(&time_parts(NEW_YEAR_2026 as f64, "UTC")).unwrap();
        assert_eq!(parts["year"], 2026);
        assert_eq!(parts["month"], 1);
        assert_eq!(parts["day"], 1);
        assert_eq!(parts["hour"], 0);
        assert_eq!(parts["dayOfWeek"], 4); // Thursday
        assert_eq!(parts["timestamp"], NEW_YEAR_2026);
        let ny: serde_json::Value =
            serde_json::from_str(&time_parts(NEW_YEAR_2026 as f64, "America/New_York")).unwrap();
        assert_eq!(ny["year"], 2025);
        assert_eq!(ny["day"], 31);
        assert_eq!(ny["hour"], 19);
    }

    #[test]
    fn time_add_units_and_calendar_months() {
        let jan31 = time_parse("2026-01-31T10:00:00Z");
        assert_eq!(
            time_add(jan31, 1, "months"),
            time_parse("2026-02-28T10:00:00Z")
        );
        assert_eq!(
            time_add(jan31, -2, "months"),
            time_parse("2025-11-30T10:00:00Z")
        );
        let leap = time_parse("2024-02-29T00:00:00Z");
        assert_eq!(
            time_add(leap, 1, "years"),
            time_parse("2025-02-28T00:00:00Z")
        );
        assert_eq!(time_add(jan31, 90, "seconds"), jan31 + 90.0);
        assert_eq!(time_add(jan31, 2, "minutes"), jan31 + 120.0);
        assert_eq!(time_add(jan31, 3, "hours"), jan31 + 10_800.0);
        assert_eq!(time_add(jan31, 1, "days"), jan31 + 86_400.0);
        assert_eq!(time_add(jan31, 2, "weeks"), jan31 + 14.0 * 86_400.0);
        assert!(time_add(jan31, 1, "fortnights").is_nan());
    }

    #[test]
    fn time_diff_units_with_fractions() {
        let a = time_parse("2026-01-01T00:00:00Z");
        let b = time_parse("2026-01-02T12:00:00Z");
        assert_eq!(time_diff(a, b, "days"), 1.5);
        assert_eq!(time_diff(a, b, "hours"), 36.0);
        assert_eq!(time_diff(b, a, "hours"), -36.0);
        assert_eq!(time_diff(a, b, "seconds"), 129_600.0);
        let jan31 = time_parse("2026-01-31T00:00:00Z");
        let feb28 = time_parse("2026-02-28T00:00:00Z");
        assert_eq!(time_diff(jan31, feb28, "months"), 1.0);
        assert_eq!(time_diff(feb28, jan31, "months"), -1.0);
        let y = time_diff(a, time_parse("2027-07-01T00:00:00Z"), "years");
        assert!((y - 1.5).abs() < 1e-9, "got {y}");
        assert!(time_diff(a, b, "eons").is_nan());
    }

    #[test]
    fn time_relative_wording() {
        let now = now_seconds() as f64;
        assert_eq!(time_relative_from(now - 5.0, now, "en"), "just now");
        assert_eq!(
            time_relative_from(now - 3.0 * 3600.0, now, "en"),
            "3 hours ago"
        );
        assert_eq!(
            time_relative_from(now + 2.0 * 86_400.0, now, "en"),
            "in 2 days"
        );
        assert_eq!(time_relative_from(now - 60.0, now, "en-US"), "1 minute ago");
        assert_eq!(
            time_relative_from(now - 3.0 * 3600.0, now, "es"),
            "hace 3 horas"
        );
        assert_eq!(
            time_relative_from(now + 2.0 * 86_400.0, now, "fr"),
            "dans 2 jours"
        );
        assert_eq!(
            time_relative_from(now - 400.0 * 86_400.0, now, "de"),
            "vor 1 Jahr"
        );
    }

    #[test]
    fn timestamps_are_unix_seconds() {
        assert_eq!(format_iso(NEW_YEAR_2026), "2026-01-01T00:00:00Z");
        assert_eq!(parse_iso("2026-01-01T00:00:00Z"), NEW_YEAR_2026);
        assert_eq!(parse_iso("2026-01-01T01:00:00+01:00"), NEW_YEAR_2026);
        assert_eq!(parse_iso("not a date"), -1);
    }

    #[test]
    fn components_round_trip_in_seconds() {
        let parts: serde_json::Value =
            serde_json::from_str(&components_json(NEW_YEAR_2026)).unwrap();
        let back = from_components(
            parts["year"].as_i64().unwrap() as i32,
            parts["month"].as_i64().unwrap() as i32,
            parts["day"].as_i64().unwrap() as i32,
            parts["hour"].as_i64().unwrap() as i32,
            parts["minute"].as_i64().unwrap() as i32,
            parts["second"].as_i64().unwrap() as i32,
        );
        assert_eq!(back, NEW_YEAR_2026);
        assert!(parts.get("millisecond").is_none());
    }

    #[test]
    fn past_and_future_compare_against_now_in_seconds() {
        let now = now_seconds();
        assert!(is_past(now - 60));
        assert!(!is_past(now + 60));
        assert!(is_future(now + 60));
        // A seconds timestamp read as milliseconds would be in 1970.
        assert!(is_future(now + 3600));
    }
}
