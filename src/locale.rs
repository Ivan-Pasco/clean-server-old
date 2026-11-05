//! Internationalization (i18n) runtime for frame.locale.
//!
//! Provides:
//! - `LocaleState` — translation maps and default locale, shared across all requests for a module.
//! - The active locale of a request lives in its own `WasmState` (`active_locale`), so
//!   concurrent requests cannot interfere with each other.
//! - Translation lookup with locale fallback and `{placeholder}` interpolation.
//! - CLDR-simplified plural form selection (`_zero`, `_one`, `_few`, `_many`, `_other`).
//! - Locale-aware number, currency, and date formatting backed by `chrono` for date/time
//!   and a hand-rolled decimal formatter for number/currency (covers the 9 most-used locales).

use chrono::{DateTime, Datelike, TimeZone, Utc, Weekday};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// RTL detection
// ---------------------------------------------------------------------------

/// Primary language tags whose script is right-to-left.
const RTL_LANGUAGES: &[&str] = &[
    "ar", "he", "fa", "ur", "yi", "dv", "ha", "khw", "ks", "ku", "ps", "sd", "ug",
];

/// Return `true` when the primary language subtag of `locale` is RTL.
pub fn is_rtl(locale: &str) -> bool {
    let primary = locale.split('-').next().unwrap_or(locale).to_lowercase();
    RTL_LANGUAGES.iter().any(|&rtl| rtl == primary)
}

// ---------------------------------------------------------------------------
// Translation store
// ---------------------------------------------------------------------------

/// Holds all loaded translation maps for a module.
#[derive(Debug, Default)]
pub struct LocaleState {
    /// The module's default locale (frame.locale `locale: default`).
    pub default_locale: String,
    /// The locale consulted when a key is missing from the requested and the
    /// default locale (frame.locale `locale: fallback`).
    pub fallback_locale: String,
    pub translations: HashMap<String, HashMap<String, String>>,
    /// Locales in the order their translations were first loaded.
    load_order: Vec<String>,
}

impl LocaleState {
    pub fn new(default_locale: impl Into<String>) -> Self {
        let default_locale = default_locale.into();
        Self {
            fallback_locale: default_locale.clone(),
            default_locale,
            translations: HashMap::new(),
            load_order: Vec::new(),
        }
    }

    /// Set the module's default locale (startup `_i18n_set_locale`).
    ///
    /// frame.locale's startup code loads the default locale's translations,
    /// then the fallback locale's (only when it differs), then selects the
    /// default. The fallback is therefore the first loaded locale other than
    /// the default; with none, the default is its own fallback.
    pub fn set_default_locale(&mut self, locale: &str) {
        self.default_locale = locale.to_string();
        self.fallback_locale = self
            .load_order
            .iter()
            .find(|l| l.as_str() != locale)
            .cloned()
            .unwrap_or_else(|| locale.to_string());
    }

    /// Load (or replace) the translation map for `locale` from a JSON string.
    ///
    /// Nested objects are flattened using dot-separated key paths.
    pub fn load_json(&mut self, locale: &str, json_str: &str) -> Result<(), String> {
        let value: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("_i18n_load: invalid JSON for locale '{}': {}", locale, e))?;

        let mut flat: HashMap<String, String> = HashMap::new();
        flatten_json(&value, String::new(), &mut flat);
        self.translations.insert(locale.to_string(), flat);
        if !self.load_order.iter().any(|l| l == locale) {
            self.load_order.push(locale.to_string());
        }
        Ok(())
    }

    fn lookup_raw(&self, key: &str, locale: &str) -> Option<&str> {
        if let Some(map) = self.translations.get(locale)
            && let Some(val) = map.get(key)
        {
            return Some(val.as_str());
        }
        if let Some(dash) = locale.find('-') {
            let primary = &locale[..dash];
            if primary != locale
                && let Some(map) = self.translations.get(primary)
                && let Some(val) = map.get(key)
            {
                return Some(val.as_str());
            }
        }
        for candidate in [&self.default_locale, &self.fallback_locale] {
            if locale != candidate.as_str()
                && let Some(map) = self.translations.get(candidate)
                && let Some(val) = map.get(key)
            {
                return Some(val.as_str());
            }
        }
        None
    }

    pub fn translate(&self, key: &str, locale: &str, params_json: &str) -> String {
        let template = match self.lookup_raw(key, locale) {
            Some(t) => t.to_owned(),
            None => return key.to_owned(),
        };
        interpolate(&template, params_json)
    }

    /// Serialize all loaded translation bundles as a compact JSON object suitable
    /// for SSR injection as `window.__CLEAN_I18N__`.
    ///
    /// The shape is `{ "en": { "key": "value", ... }, "fr": { ... } }`,
    /// matching the flat-key lookup pattern expected by the browser-side loader.
    ///
    /// Returns `None` when no translations have been loaded yet (fresh state or
    /// all-empty maps), so callers can skip injection rather than emit an empty object.
    ///
    /// The returned string is HTML-safe: `</` is escaped to `<\/` and the
    /// Unicode line separator (U+2028) and paragraph separator (U+2029) are
    /// replaced with their `\uXXXX` JSON escape sequences so the payload is
    /// safe to embed directly inside a `<script>` element.
    pub fn bundle_as_json(&self) -> Option<String> {
        if self.translations.is_empty() {
            return None;
        }

        // Build a deterministic JSON value from the flat translation maps.
        // Using serde_json::Map preserves insertion order; sort keys for
        // reproducibility and easier inspection in DevTools.
        let mut outer = serde_json::Map::new();
        let mut locales: Vec<&String> = self.translations.keys().collect();
        locales.sort_unstable();

        for locale in locales {
            let flat = &self.translations[locale];
            let mut inner = serde_json::Map::new();
            let mut keys: Vec<&String> = flat.keys().collect();
            keys.sort_unstable();
            for k in keys {
                inner.insert(k.clone(), serde_json::Value::String(flat[k].clone()));
            }
            outer.insert(locale.clone(), serde_json::Value::Object(inner));
        }

        let raw = serde_json::to_string(&serde_json::Value::Object(outer))
            .expect("serde_json::to_string of a Value::Object cannot fail");

        // HTML-safety escaping for embedding inside a <script> element:
        //   1. `</`  → `<\/`  prevents a `</script>` from closing the tag early
        //   2. U+2028 (line separator) and U+2029 (paragraph separator) are
        //      technically valid in JSON strings but break JS parsers when
        //      embedded in a <script> block because they are treated as
        //      line-terminators in ECMAScript.
        let safe = raw
            .replace("</", r"<\/")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");

        Some(safe)
    }

    pub fn translate_count(
        &self,
        key: &str,
        count: i32,
        locale: &str,
        params_json: &str,
    ) -> String {
        let merged = inject_count(params_json, count);

        // Per CLDR spec: if count == 0 and a `{key}_zero` form exists, use it
        // regardless of the locale's plural rules (most locales have no "zero" category
        // in CLDR, but apps often define a zero form explicitly).
        if count == 0 {
            let zero_key = format!("{}_zero", key);
            if self.lookup_raw(&zero_key, locale).is_some() {
                return self.translate(&zero_key, locale, &merged);
            }
        }

        let category = plural_category(count, locale, key, self);
        let suffixed = format!("{}_{}", key, category);
        if self.lookup_raw(&suffixed, locale).is_some() {
            return self.translate(&suffixed, locale, &merged);
        }
        let other_key = format!("{}_other", key);
        if self.lookup_raw(&other_key, locale).is_some() {
            return self.translate(&other_key, locale, &merged);
        }
        key.to_owned()
    }
}

pub type SharedLocaleState = Arc<RwLock<LocaleState>>;

pub fn create_shared_locale_state() -> SharedLocaleState {
    Arc::new(RwLock::new(LocaleState::new("en")))
}

// ---------------------------------------------------------------------------
// JSON flattening
// ---------------------------------------------------------------------------

fn flatten_json(value: &serde_json::Value, prefix: String, out: &mut HashMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let new_prefix = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                flatten_json(v, new_prefix, out);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix, s.clone());
        }
        serde_json::Value::Number(n) => {
            out.insert(prefix, n.to_string());
        }
        serde_json::Value::Bool(b) => {
            out.insert(prefix, b.to_string());
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Interpolation helpers
// ---------------------------------------------------------------------------

fn interpolate(template: &str, params_json: &str) -> String {
    if !template.contains('{') {
        return template.to_owned();
    }
    let params: serde_json::Value = match serde_json::from_str(params_json) {
        Ok(v) => v,
        Err(_) => return template.to_owned(),
    };
    let obj = match params.as_object() {
        Some(o) => o,
        None => return template.to_owned(),
    };
    let mut result = template.to_owned();
    for (k, v) in obj {
        let placeholder = format!("{{{}}}", k);
        let replacement = match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        result = result.replace(&placeholder, &replacement);
    }
    result
}

fn inject_count(params_json: &str, count: i32) -> String {
    let mut obj: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(params_json).unwrap_or_default();
    obj.entry("count".to_string())
        .or_insert_with(|| serde_json::Value::Number(count.into()));
    serde_json::to_string(&obj).unwrap_or_else(|_| "{}".to_string())
}

// ---------------------------------------------------------------------------
// CLDR plural rules (simplified)
// ---------------------------------------------------------------------------

pub fn plural_category(count: i32, locale: &str, _key: &str, _state: &LocaleState) -> &'static str {
    let primary = locale.split('-').next().unwrap_or(locale).to_lowercase();
    let abs = count.unsigned_abs() as u64;

    match primary.as_str() {
        "ar" => match abs {
            0 => "zero",
            1 => "one",
            2 => "two",
            3..=10 => "few",
            11..=99 => "many",
            _ => {
                let r100 = abs % 100;
                match r100 {
                    3..=10 => "few",
                    11..=99 => "many",
                    _ => "other",
                }
            }
        },
        "he" => match abs {
            0 => "zero",
            1 => "one",
            2 => "two",
            _ if abs.is_multiple_of(10) => "many",
            _ => "other",
        },
        "ru" | "uk" | "be" => slavic_ru(abs),
        "pl" => {
            if abs == 1 {
                "one"
            } else {
                let r10 = abs % 10;
                let r100 = abs % 100;
                if (2..=4).contains(&r10) && !(12..=14).contains(&r100) {
                    "few"
                } else if r10 == 0 || r10 >= 5 || (10..=20).contains(&r100) {
                    "many"
                } else {
                    "other"
                }
            }
        }
        "cs" | "sk" => match abs {
            1 => "one",
            2..=4 => "few",
            _ => "other",
        },
        "sl" => {
            let r100 = abs % 100;
            match r100 {
                1 => "one",
                2 => "two",
                3..=4 => "few",
                _ => "other",
            }
        }
        "fr" | "pt" => {
            if abs <= 1 {
                "one"
            } else {
                "other"
            }
        }
        "ja" | "zh" | "ko" | "th" | "vi" | "id" | "ms" => "other",
        _ => {
            if abs == 1 {
                "one"
            } else {
                "other"
            }
        }
    }
}

fn slavic_ru(abs: u64) -> &'static str {
    let r10 = abs % 10;
    let r100 = abs % 100;
    if r10 == 1 && r100 != 11 {
        "one"
    } else if (2..=4).contains(&r10) && !(12..=14).contains(&r100) {
        "few"
    } else {
        "many"
    }
}

// ---------------------------------------------------------------------------
// Number formatting
// ---------------------------------------------------------------------------

struct LocaleNumberFormat {
    group: char,
    decimal: char,
}

fn number_format_for(locale: &str) -> LocaleNumberFormat {
    let primary = locale.split('-').next().unwrap_or(locale).to_lowercase();
    match primary.as_str() {
        "de" | "nl" | "it" | "pl" | "cs" | "sk" | "hu" | "hr" | "bg" | "ro" | "tr" | "el"
        | "ru" | "uk" | "be" | "sl" | "sr" | "no" | "fi" | "da" | "sv" | "nb" => {
            LocaleNumberFormat {
                group: '.',
                decimal: ',',
            }
        }
        "fr" => LocaleNumberFormat {
            group: '\u{202F}',
            decimal: ',',
        },
        _ => LocaleNumberFormat {
            group: ',',
            decimal: '.',
        },
    }
}

pub fn format_number(value: f64, locale: &str, decimals: i32, use_grouping: bool) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "\u{221E}".to_string()
        } else {
            "-\u{221E}".to_string()
        };
    }
    let fmt = number_format_for(locale);
    let d = if decimals < 0 { 2 } else { decimals as usize };
    let negative = value < 0.0;
    let abs_val = value.abs();
    let raw = format!("{:.prec$}", abs_val, prec = d);
    let (int_part, frac_part) = if let Some(dot_pos) = raw.find('.') {
        (&raw[..dot_pos], &raw[dot_pos + 1..])
    } else {
        (raw.as_str(), "")
    };
    let int_formatted = if use_grouping && int_part.len() > 3 {
        let mut out = String::with_capacity(int_part.len() + int_part.len() / 3);
        for (i, ch) in int_part.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(fmt.group);
            }
            out.push(ch);
        }
        out.chars().rev().collect()
    } else {
        int_part.to_string()
    };
    let mut result = String::new();
    if negative {
        result.push('-');
    }
    result.push_str(&int_formatted);
    if d > 0 && !frac_part.is_empty() {
        result.push(fmt.decimal);
        result.push_str(frac_part);
    }
    result
}

pub fn parse_number_options(options_json: &str) -> (i32, bool) {
    let obj: serde_json::Value = serde_json::from_str(options_json).unwrap_or_default();
    let decimals = obj
        .get("maximumFractionDigits")
        .or_else(|| obj.get("minimumFractionDigits"))
        .and_then(|v| v.as_i64())
        .map(|v| v.clamp(0, 20) as i32)
        .unwrap_or(-1);
    let use_grouping = obj
        .get("useGrouping")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    (decimals, use_grouping)
}

// ---------------------------------------------------------------------------
// Currency formatting
// ---------------------------------------------------------------------------

fn currency_symbol(code: &str, locale: &str) -> String {
    let upper = code.to_uppercase();
    let primary = locale.split('-').next().unwrap_or(locale).to_lowercase();
    match upper.as_str() {
        "USD" => {
            if primary.starts_with("en") {
                "$".to_string()
            } else {
                "US$".to_string()
            }
        }
        "EUR" => "\u{20AC}".to_string(),
        "GBP" => "\u{00A3}".to_string(),
        "JPY" => "\u{00A5}".to_string(),
        "CNY" | "RMB" => "\u{00A5}".to_string(),
        "CAD" => "CA$".to_string(),
        "AUD" => "A$".to_string(),
        "CHF" => "Fr.".to_string(),
        "INR" => "\u{20B9}".to_string(),
        "KRW" => "\u{20A9}".to_string(),
        "BRL" => "R$".to_string(),
        "MXN" => "MX$".to_string(),
        "SEK" | "NOK" => "kr".to_string(),
        "DKK" => "kr.".to_string(),
        "PLN" => "z\u{0142}".to_string(),
        "CZK" => "K\u{010D}".to_string(),
        "HUF" => "Ft".to_string(),
        "RUB" => "\u{20BD}".to_string(),
        "TRY" => "\u{20BA}".to_string(),
        "SAR" => "\u{FDFC}".to_string(),
        "AED" => "\u{062F}.\u{0625}".to_string(),
        "SGD" => "S$".to_string(),
        "HKD" => "HK$".to_string(),
        "NZD" => "NZ$".to_string(),
        "ZAR" => "R".to_string(),
        "THB" => "\u{0E3F}".to_string(),
        "IDR" => "Rp".to_string(),
        "MYR" => "RM".to_string(),
        "PHP" => "\u{20B1}".to_string(),
        "VND" => "\u{20AB}".to_string(),
        "EGP" => "E\u{00A3}".to_string(),
        "UAH" => "\u{20B4}".to_string(),
        "RON" => "lei".to_string(),
        _ => upper,
    }
}

pub fn format_currency(value: f64, currency_code: &str, locale: &str) -> String {
    let upper = currency_code.to_uppercase();
    let symbol = currency_symbol(&upper, locale);
    let decimals: i32 = match upper.as_str() {
        "JPY" | "KRW" | "VND" | "IDR" | "HUF" | "CLP" | "ISK" | "PYG" | "UGX" | "RWF" => 0,
        _ => 2,
    };
    let number_str = format_number(value, locale, decimals, true);
    let primary = locale.split('-').next().unwrap_or(locale).to_lowercase();
    match primary.as_str() {
        "sv" | "nb" | "da" | "pl" | "cs" | "sk" | "hu" | "ro" => {
            format!("{}\u{00A0}{}", number_str, symbol)
        }
        "fr" => format!("{}\u{202F}{}", number_str, symbol),
        _ => format!("{}{}", symbol, number_str),
    }
}

// ---------------------------------------------------------------------------
// Date formatting
// ---------------------------------------------------------------------------

/// Format a Unix timestamp in seconds (the platform's time unit, see
/// `_i18n_format_date` in function-registry.toml) as a locale-aware date.
pub fn format_date(timestamp_seconds: f64, style: &str, locale: &str) -> String {
    if timestamp_seconds.is_nan() || timestamp_seconds.is_infinite() {
        return "Invalid Date".to_string();
    }
    let secs = timestamp_seconds as i64;
    let dt: DateTime<Utc> = match Utc.timestamp_opt(secs, 0).single() {
        Some(dt) => dt,
        None => return "Invalid Date".to_string(),
    };
    let primary = locale.split('-').next().unwrap_or(locale).to_lowercase();
    let region = locale.split('-').nth(1).unwrap_or("").to_uppercase();
    match style {
        "short" => format_date_short(&dt, &primary, &region),
        "medium" => format_date_medium(&dt, &primary),
        "long" => format_date_long(&dt, &primary),
        "full" => format_date_full(&dt, &primary),
        pattern => dt.format(pattern).to_string(),
    }
}

fn format_date_short(dt: &DateTime<Utc>, primary: &str, _region: &str) -> String {
    let y = dt.year() % 100;
    let m = dt.month();
    let d = dt.day();
    match primary {
        "en" => format!("{}/{}/{:02}", m, d, y),
        "de" | "nl" | "pl" | "cs" | "sk" | "hu" | "hr" | "sl" | "sr" | "bg" | "ru" | "uk"
        | "be" | "ro" | "tr" | "el" => format!("{:02}.{:02}.{:02}", d, m, y),
        "fr" | "pt" | "es" | "it" | "nb" | "da" | "sv" | "fi" | "et" | "lv" | "lt" => {
            format!("{:02}/{:02}/{:02}", d, m, y)
        }
        "ja" | "zh" | "ko" => format!("{:02}/{:02}/{:02}", y, m, d),
        _ => format!("{}/{}/{:02}", m, d, y),
    }
}

fn month_abbr(month: u32, primary: &str) -> &'static str {
    match primary {
        "fr" => match month {
            1 => "janv.",
            2 => "f\u{00E9}vr.",
            3 => "mars",
            4 => "avr.",
            5 => "mai",
            6 => "juin",
            7 => "juil.",
            8 => "ao\u{00FB}t",
            9 => "sept.",
            10 => "oct.",
            11 => "nov.",
            12 => "d\u{00E9}c.",
            _ => "???",
        },
        "de" => match month {
            1 => "Jan.",
            2 => "Feb.",
            3 => "M\u{00E4}r.",
            4 => "Apr.",
            5 => "Mai",
            6 => "Juni",
            7 => "Juli",
            8 => "Aug.",
            9 => "Sep.",
            10 => "Okt.",
            11 => "Nov.",
            12 => "Dez.",
            _ => "???",
        },
        "es" => match month {
            1 => "ene.",
            2 => "feb.",
            3 => "mar.",
            4 => "abr.",
            5 => "may.",
            6 => "jun.",
            7 => "jul.",
            8 => "ago.",
            9 => "sep.",
            10 => "oct.",
            11 => "nov.",
            12 => "dic.",
            _ => "???",
        },
        "pt" => match month {
            1 => "jan.",
            2 => "fev.",
            3 => "mar.",
            4 => "abr.",
            5 => "mai.",
            6 => "jun.",
            7 => "jul.",
            8 => "ago.",
            9 => "set.",
            10 => "out.",
            11 => "nov.",
            12 => "dez.",
            _ => "???",
        },
        "ja" => match month {
            1 => "1\u{6708}",
            2 => "2\u{6708}",
            3 => "3\u{6708}",
            4 => "4\u{6708}",
            5 => "5\u{6708}",
            6 => "6\u{6708}",
            7 => "7\u{6708}",
            8 => "8\u{6708}",
            9 => "9\u{6708}",
            10 => "10\u{6708}",
            11 => "11\u{6708}",
            12 => "12\u{6708}",
            _ => "???",
        },
        "zh" => match month {
            1 => "1\u{6708}",
            2 => "2\u{6708}",
            3 => "3\u{6708}",
            4 => "4\u{6708}",
            5 => "5\u{6708}",
            6 => "6\u{6708}",
            7 => "7\u{6708}",
            8 => "8\u{6708}",
            9 => "9\u{6708}",
            10 => "10\u{6708}",
            11 => "11\u{6708}",
            12 => "12\u{6708}",
            _ => "???",
        },
        "ko" => match month {
            1 => "1\u{C6D4}",
            2 => "2\u{C6D4}",
            3 => "3\u{C6D4}",
            4 => "4\u{C6D4}",
            5 => "5\u{C6D4}",
            6 => "6\u{C6D4}",
            7 => "7\u{C6D4}",
            8 => "8\u{C6D4}",
            9 => "9\u{C6D4}",
            10 => "10\u{C6D4}",
            11 => "11\u{C6D4}",
            12 => "12\u{C6D4}",
            _ => "???",
        },
        _ => match month {
            1 => "Jan",
            2 => "Feb",
            3 => "Mar",
            4 => "Apr",
            5 => "May",
            6 => "Jun",
            7 => "Jul",
            8 => "Aug",
            9 => "Sep",
            10 => "Oct",
            11 => "Nov",
            12 => "Dec",
            _ => "???",
        },
    }
}

fn month_full(month: u32, primary: &str) -> &'static str {
    match primary {
        "fr" => match month {
            1 => "janvier",
            2 => "f\u{00E9}vrier",
            3 => "mars",
            4 => "avril",
            5 => "mai",
            6 => "juin",
            7 => "juillet",
            8 => "ao\u{00FB}t",
            9 => "septembre",
            10 => "octobre",
            11 => "novembre",
            12 => "d\u{00E9}cembre",
            _ => "???",
        },
        "de" => match month {
            1 => "Januar",
            2 => "Februar",
            3 => "M\u{00E4}rz",
            4 => "April",
            5 => "Mai",
            6 => "Juni",
            7 => "Juli",
            8 => "August",
            9 => "September",
            10 => "Oktober",
            11 => "November",
            12 => "Dezember",
            _ => "???",
        },
        "es" => match month {
            1 => "enero",
            2 => "febrero",
            3 => "marzo",
            4 => "abril",
            5 => "mayo",
            6 => "junio",
            7 => "julio",
            8 => "agosto",
            9 => "septiembre",
            10 => "octubre",
            11 => "noviembre",
            12 => "diciembre",
            _ => "???",
        },
        "pt" => match month {
            1 => "janeiro",
            2 => "fevereiro",
            3 => "mar\u{00E7}o",
            4 => "abril",
            5 => "maio",
            6 => "junho",
            7 => "julho",
            8 => "agosto",
            9 => "setembro",
            10 => "outubro",
            11 => "novembro",
            12 => "dezembro",
            _ => "???",
        },
        "ja" => month_abbr(month, "ja"),
        "zh" => month_abbr(month, "zh"),
        "ko" => month_abbr(month, "ko"),
        _ => match month {
            1 => "January",
            2 => "February",
            3 => "March",
            4 => "April",
            5 => "May",
            6 => "June",
            7 => "July",
            8 => "August",
            9 => "September",
            10 => "October",
            11 => "November",
            12 => "December",
            _ => "???",
        },
    }
}

fn weekday_full(weekday: Weekday, primary: &str) -> &'static str {
    match primary {
        "fr" => match weekday {
            Weekday::Mon => "lundi",
            Weekday::Tue => "mardi",
            Weekday::Wed => "mercredi",
            Weekday::Thu => "jeudi",
            Weekday::Fri => "vendredi",
            Weekday::Sat => "samedi",
            Weekday::Sun => "dimanche",
        },
        "de" => match weekday {
            Weekday::Mon => "Montag",
            Weekday::Tue => "Dienstag",
            Weekday::Wed => "Mittwoch",
            Weekday::Thu => "Donnerstag",
            Weekday::Fri => "Freitag",
            Weekday::Sat => "Samstag",
            Weekday::Sun => "Sonntag",
        },
        "es" => match weekday {
            Weekday::Mon => "lunes",
            Weekday::Tue => "martes",
            Weekday::Wed => "mi\u{00E9}rcoles",
            Weekday::Thu => "jueves",
            Weekday::Fri => "viernes",
            Weekday::Sat => "s\u{00E1}bado",
            Weekday::Sun => "domingo",
        },
        "pt" => match weekday {
            Weekday::Mon => "segunda-feira",
            Weekday::Tue => "ter\u{00E7}a-feira",
            Weekday::Wed => "quarta-feira",
            Weekday::Thu => "quinta-feira",
            Weekday::Fri => "sexta-feira",
            Weekday::Sat => "s\u{00E1}bado",
            Weekday::Sun => "domingo",
        },
        "ja" => match weekday {
            Weekday::Mon => "\u{6708}\u{66DC}\u{65E5}",
            Weekday::Tue => "\u{706B}\u{66DC}\u{65E5}",
            Weekday::Wed => "\u{6C34}\u{66DC}\u{65E5}",
            Weekday::Thu => "\u{6728}\u{66DC}\u{65E5}",
            Weekday::Fri => "\u{91D1}\u{66DC}\u{65E5}",
            Weekday::Sat => "\u{571F}\u{66DC}\u{65E5}",
            Weekday::Sun => "\u{65E5}\u{66DC}\u{65E5}",
        },
        "zh" => match weekday {
            Weekday::Mon => "\u{661F}\u{671F}\u{4E00}",
            Weekday::Tue => "\u{661F}\u{671F}\u{4E8C}",
            Weekday::Wed => "\u{661F}\u{671F}\u{4E09}",
            Weekday::Thu => "\u{661F}\u{671F}\u{56DB}",
            Weekday::Fri => "\u{661F}\u{671F}\u{4E94}",
            Weekday::Sat => "\u{661F}\u{671F}\u{516D}",
            Weekday::Sun => "\u{661F}\u{671F}\u{65E5}",
        },
        "ko" => match weekday {
            Weekday::Mon => "\u{C6D4}\u{C694}\u{C77C}",
            Weekday::Tue => "\u{D654}\u{C694}\u{C77C}",
            Weekday::Wed => "\u{C218}\u{C694}\u{C77C}",
            Weekday::Thu => "\u{BAA9}\u{C694}\u{C77C}",
            Weekday::Fri => "\u{AE08}\u{C694}\u{C77C}",
            Weekday::Sat => "\u{D1A0}\u{C694}\u{C77C}",
            Weekday::Sun => "\u{C77C}\u{C694}\u{C77C}",
        },
        _ => match weekday {
            Weekday::Mon => "Monday",
            Weekday::Tue => "Tuesday",
            Weekday::Wed => "Wednesday",
            Weekday::Thu => "Thursday",
            Weekday::Fri => "Friday",
            Weekday::Sat => "Saturday",
            Weekday::Sun => "Sunday",
        },
    }
}

fn format_date_medium(dt: &DateTime<Utc>, primary: &str) -> String {
    let abbr = month_abbr(dt.month(), primary);
    let d = dt.day();
    let y = dt.year();
    match primary {
        "fr" | "de" | "es" | "pt" | "it" | "pl" | "cs" | "sk" | "hu" | "ro" | "nl" | "sv"
        | "nb" | "da" | "fi" | "tr" => format!("{} {} {}", d, abbr, y),
        "ja" => format!("{}年{}日", y, abbr),
        "zh" => format!("{}年{}日", y, abbr),
        "ko" => format!("{}년 {} {}일", y, abbr, d),
        _ => format!("{} {}, {}", abbr, d, y),
    }
}

fn format_date_long(dt: &DateTime<Utc>, primary: &str) -> String {
    let full = month_full(dt.month(), primary);
    let d = dt.day();
    let y = dt.year();
    match primary {
        "fr" => format!("{} {} {}", d, full, y),
        "de" => format!("{}. {} {}", d, full, y),
        "es" | "pt" | "it" | "nl" => format!("{} de {} de {}", d, full, y),
        "ja" => format!("{}年{}{}日", y, full, d),
        "zh" => format!("{}年{}{}日", y, full, d),
        "ko" => format!("{}년 {} {}일", y, full, d),
        _ => format!("{} {}, {}", full, d, y),
    }
}

fn format_date_full(dt: &DateTime<Utc>, primary: &str) -> String {
    let day_name = weekday_full(dt.weekday(), primary);
    let long = format_date_long(dt, primary);
    match primary {
        "ja" | "zh" | "ko" => format!("{}({})", long, day_name),
        _ => format!("{}, {}", day_name, long),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_translate_key_found() {
        let mut state = LocaleState::new("en");
        state
            .load_json("en", r#"{"common": {"save": "Save"}}"#)
            .unwrap();
        assert_eq!(state.translate("common.save", "en", "{}"), "Save");
    }

    #[test]
    fn test_default_locale_and_fallback_from_startup() {
        // frame.locale startup: load default (fr), load fallback (en), select fr.
        let mut state = LocaleState::new("en");
        state.load_json("fr", r#"{"hello": "Bonjour"}"#).unwrap();
        state
            .load_json("en", r#"{"hello": "Hello", "bye": "Goodbye"}"#)
            .unwrap();
        state.set_default_locale("fr");
        assert_eq!(state.default_locale, "fr");
        assert_eq!(state.fallback_locale, "en");
        assert_eq!(state.translate("hello", "fr", "{}"), "Bonjour");
        // Missing from fr -> served from the fallback locale.
        assert_eq!(state.translate("bye", "fr", "{}"), "Goodbye");
        // Requested locale without a bundle -> default first.
        assert_eq!(state.translate("hello", "de", "{}"), "Bonjour");
    }

    #[test]
    fn test_translate_fallback_to_default() {
        let mut state = LocaleState::new("en");
        state
            .load_json("en", r#"{"common": {"save": "Save"}}"#)
            .unwrap();
        assert_eq!(state.translate("common.save", "fr", "{}"), "Save");
    }

    #[test]
    fn test_translate_key_missing_returns_key() {
        let state = LocaleState::new("en");
        assert_eq!(state.translate("missing.key", "en", "{}"), "missing.key");
    }

    #[test]
    fn test_translate_interpolation() {
        let mut state = LocaleState::new("en");
        state
            .load_json("en", r#"{"greeting": "Hello, {name}!"}"#)
            .unwrap();
        assert_eq!(
            state.translate("greeting", "en", r#"{"name": "Alice"}"#),
            "Hello, Alice!"
        );
    }

    #[test]
    fn test_translate_bcp47_prefix() {
        let mut state = LocaleState::new("en");
        state.load_json("fr", r#"{"save": "Sauvegarder"}"#).unwrap();
        assert_eq!(state.translate("save", "fr-CA", "{}"), "Sauvegarder");
    }

    #[test]
    fn test_translate_count_en() {
        let mut state = LocaleState::new("en");
        state
            .load_json(
                "en",
                r#"{
            "users_zero": "No users",
            "users_one": "One user",
            "users_other": "{count} users"
        }"#,
            )
            .unwrap();
        assert_eq!(state.translate_count("users", 0, "en", "{}"), "No users");
        assert_eq!(state.translate_count("users", 1, "en", "{}"), "One user");
        assert_eq!(state.translate_count("users", 5, "en", "{}"), "5 users");
    }

    #[test]
    fn test_translate_count_fallback_other() {
        let mut state = LocaleState::new("en");
        state
            .load_json(
                "en",
                r#"{"items_one": "One item", "items_other": "{count} items"}"#,
            )
            .unwrap();
        // No _zero: should fall back to _other
        assert_eq!(state.translate_count("items", 0, "en", "{}"), "0 items");
        assert_eq!(state.translate_count("items", 5, "en", "{}"), "5 items");
    }

    #[test]
    fn test_format_number_en() {
        assert_eq!(format_number(1299.99, "en-US", 2, true), "1,299.99");
    }

    #[test]
    fn test_format_number_de() {
        assert_eq!(format_number(1299.99, "de", 2, true), "1.299,99");
    }

    #[test]
    fn test_format_number_no_grouping() {
        assert_eq!(format_number(1299.99, "en", 2, false), "1299.99");
    }

    #[test]
    fn test_format_currency_usd_en() {
        assert_eq!(format_currency(1299.99, "USD", "en-US"), "$1,299.99");
    }

    #[test]
    fn test_format_currency_jpy_no_decimals() {
        assert_eq!(format_currency(1299.0, "JPY", "ja"), "\u{00A5}1,299");
    }

    #[test]
    fn test_format_date_medium_en() {
        // 2026-01-01 00:00:00 UTC = 1735689600
        assert_eq!(format_date(1767225600.0, "medium", "en-US"), "Jan 1, 2026");
    }

    #[test]
    fn test_format_date_short_en_us() {
        assert_eq!(format_date(1767225600.0, "short", "en-US"), "1/1/26");
    }

    #[test]
    fn test_format_date_long_en() {
        assert_eq!(format_date(1767225600.0, "long", "en"), "January 1, 2026");
    }

    #[test]
    fn test_format_date_full_en() {
        assert_eq!(
            format_date(1767225600.0, "full", "en"),
            "Thursday, January 1, 2026"
        );
    }

    #[test]
    fn test_format_date_invalid() {
        assert_eq!(format_date(f64::NAN, "short", "en"), "Invalid Date");
    }

    #[test]
    fn test_is_rtl() {
        assert!(is_rtl("ar"));
        assert!(is_rtl("ar-SA"));
        assert!(!is_rtl("en"));
        assert!(!is_rtl("fr-CA"));
    }

    #[test]
    fn test_flatten_nested() {
        let mut state = LocaleState::new("en");
        state
            .load_json(
                "en",
                r#"{"users": {"greeting": "Hello", "bye": "Goodbye"}}"#,
            )
            .unwrap();
        assert_eq!(state.translate("users.greeting", "en", "{}"), "Hello");
        assert_eq!(state.translate("users.bye", "en", "{}"), "Goodbye");
    }

    #[test]
    fn test_plural_en() {
        let state = LocaleState::new("en");
        assert_eq!(plural_category(1, "en", "k", &state), "one");
        assert_eq!(plural_category(0, "en", "k", &state), "other");
        assert_eq!(plural_category(5, "en", "k", &state), "other");
    }

    #[test]
    fn test_plural_ru() {
        let state = LocaleState::new("en");
        assert_eq!(plural_category(1, "ru", "k", &state), "one");
        assert_eq!(plural_category(11, "ru", "k", &state), "many");
        assert_eq!(plural_category(2, "ru", "k", &state), "few");
        assert_eq!(plural_category(5, "ru", "k", &state), "many");
    }

    // -----------------------------------------------------------------------
    // bundle_as_json tests
    // -----------------------------------------------------------------------

    #[test]
    fn bundle_as_json_empty_returns_none() {
        let state = LocaleState::new("en");
        assert!(
            state.bundle_as_json().is_none(),
            "fresh LocaleState with no loaded translations must return None"
        );
    }

    #[test]
    fn bundle_as_json_contains_loaded_translations() {
        let mut state = LocaleState::new("en");
        state
            .load_json("en", r#"{"greeting": "Hello", "farewell": "Goodbye"}"#)
            .unwrap();
        state
            .load_json("fr", r#"{"greeting": "Bonjour", "farewell": "Au revoir"}"#)
            .unwrap();

        let json_str = state
            .bundle_as_json()
            .expect("bundle_as_json must return Some when translations are loaded");

        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("bundle_as_json output must be valid JSON");

        // Both locales present
        assert!(parsed.get("en").is_some(), "JSON must contain 'en' locale");
        assert!(parsed.get("fr").is_some(), "JSON must contain 'fr' locale");

        // Key lookup for English
        assert_eq!(
            parsed["en"]["greeting"],
            serde_json::json!("Hello"),
            "en.greeting must be 'Hello'"
        );
        assert_eq!(
            parsed["en"]["farewell"],
            serde_json::json!("Goodbye"),
            "en.farewell must be 'Goodbye'"
        );

        // Key lookup for French
        assert_eq!(
            parsed["fr"]["greeting"],
            serde_json::json!("Bonjour"),
            "fr.greeting must be 'Bonjour'"
        );
        assert_eq!(
            parsed["fr"]["farewell"],
            serde_json::json!("Au revoir"),
            "fr.farewell must be 'Au revoir'"
        );
    }

    #[test]
    fn bundle_as_json_html_safe() {
        let mut state = LocaleState::new("en");
        // Value contains `</script>` (script-injection attempt) and U+2028 (line separator)
        state
            .load_json(
                "en",
                "{ \"xss\": \"</script><script>alert(1)</script>\", \"ls\": \"\u{2028}\" }",
            )
            .unwrap();

        let json_str = state
            .bundle_as_json()
            .expect("bundle_as_json must return Some");

        // The raw `</script>` sequence must not appear in the output
        assert!(
            !json_str.contains("</script>"),
            "bundle_as_json must not emit raw </script> — got: {}",
            json_str
        );

        // U+2028 must not appear as a raw character
        assert!(
            !json_str.contains('\u{2028}'),
            "bundle_as_json must not emit a raw U+2028 line separator — got: {}",
            json_str
        );

        // The output must still be valid JSON after the escaping
        let _parsed: serde_json::Value = serde_json::from_str(&json_str)
            .expect("bundle_as_json output must remain valid JSON after HTML-safety escaping");
    }
}
