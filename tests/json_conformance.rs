//! JSONTestSuite conformance harness — Delivery-2 [P4a].
//!
//! Runs every `.json` file in `tests/fixtures/JSONTestSuite/test_parsing/`
//! through the v2 bridge (`_json_decode_v2`) and gates future commits against
//! `tests/json_conformance_baseline.txt`.
//!
//! Categories per Nicolas Seriot's JSONTestSuite naming convention:
//!   `y_*` — MUST decode (non-zero pointer)
//!   `n_*` — MUST reject (0 sentinel)
//!   `i_*` — implementation-defined; logged but not gated
//!
//! Baseline file format is one line per file:
//!   `<filename> <outcome>`
//! where outcome is one of `pass`, `reject`, `skip`. A commit that regresses
//! any y_* pass or n_* reject compared to the baseline fails the test.
//! i_* deltas print a warning only.
//!
//! The corpus is imported as a git submodule. If the submodule is absent
//! (e.g. a checkout without `submodules: recursive`), the test fails, so it
//! never masquerades as a green run on a workflow that forgot the checkout
//! flag.

use clean_server::bridge::create_linker;
use clean_server::router::Router;
use clean_server::wasm::WasmState;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasmtime::{Engine, Instance, Module, Store, TypedFunc};

const WAT: &str = r#"
(module
  (import "env" "_json_decode_v2" (func $decode (param i32 i32) (result i32)))
  (memory (export "memory") 32)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "malloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "reset_heap")
    (global.set $heap (i32.const 1024)))
  (func (export "call_decode") (param $ptr i32) (param $len i32) (result i32)
    (call $decode (local.get $ptr) (local.get $len))))
"#;

struct Harness {
    store: Store<WasmState>,
    instance: Instance,
    decode: TypedFunc<(i32, i32), i32>,
    malloc: TypedFunc<i32, i32>,
    reset_heap: TypedFunc<(), ()>,
}

impl Harness {
    fn new() -> Self {
        let engine = Engine::default();
        let linker = create_linker(&engine).expect("linker");
        let module = Module::new(&engine, WAT).expect("WAT");
        let router = Arc::new(Router::new());
        let mut store = Store::new(&engine, WasmState::new(router));
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let decode = instance.get_typed_func(&mut store, "call_decode").unwrap();
        let malloc = instance.get_typed_func(&mut store, "malloc").unwrap();
        let reset_heap = instance.get_typed_func(&mut store, "reset_heap").unwrap();
        Self {
            store,
            instance,
            decode,
            malloc,
            reset_heap,
        }
    }

    fn decode_bytes(&mut self, bytes: &[u8]) -> i32 {
        self.reset_heap.call(&mut self.store, ()).unwrap();
        let len = bytes.len() as i32;
        let ptr = self.malloc.call(&mut self.store, len).unwrap();
        let memory = self.instance.get_memory(&mut self.store, "memory").unwrap();
        memory.data_mut(&mut self.store)[ptr as usize..ptr as usize + bytes.len()]
            .copy_from_slice(bytes);
        self.decode.call(&mut self.store, (ptr, len)).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,   // decode returned non-zero
    Reject, // decode returned 0 sentinel
    Skip,   // file could not be read or is non-UTF-8 and we chose to skip
}

impl Outcome {
    fn as_str(&self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Reject => "reject",
            Outcome::Skip => "skip",
        }
    }
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/JSONTestSuite/test_parsing")
}

fn baseline_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/json_conformance_baseline.txt")
}

fn corpus_available() -> bool {
    corpus_dir().is_dir()
        && fs::read_dir(corpus_dir())
            .map(|it| it.count() > 0)
            .unwrap_or(false)
}

fn run_corpus() -> BTreeMap<String, Outcome> {
    let mut h = Harness::new();
    let mut results = BTreeMap::new();
    let entries = fs::read_dir(corpus_dir()).expect("read corpus dir");
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let outcome = classify(&mut h, &path);
        results.insert(name, outcome);
    }
    results
}

fn classify(h: &mut Harness, path: &Path) -> Outcome {
    let Ok(bytes) = fs::read(path) else {
        return Outcome::Skip;
    };
    // Non-UTF-8 fixtures (i_string_UTF-16*.json, n_*_invalid-utf-8*.json) are
    // passed through raw so the bridge exercises its own UTF-8 validation.
    // serde_json (behind the bridge) rejects invalid UTF-8 payloads, which is
    // the correct behaviour for n_* fixtures.
    let ptr = h.decode_bytes(&bytes);
    if ptr == 0 {
        Outcome::Reject
    } else {
        Outcome::Pass
    }
}

fn read_baseline() -> BTreeMap<String, Outcome> {
    let text = match fs::read_to_string(baseline_path()) {
        Ok(t) => t,
        Err(_) => return BTreeMap::new(),
    };
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap().to_string();
        let outcome = match parts.next().map(str::trim) {
            Some("pass") => Outcome::Pass,
            Some("reject") => Outcome::Reject,
            Some("skip") => Outcome::Skip,
            _ => continue,
        };
        out.insert(name, outcome);
    }
    out
}

fn category(name: &str) -> Option<char> {
    let mut chars = name.chars();
    let first = chars.next()?;
    let second = chars.next()?;
    if second != '_' {
        return None;
    }
    if matches!(first, 'y' | 'n' | 'i') {
        Some(first)
    } else {
        None
    }
}

#[test]
fn json_conformance_baseline_matches() {
    // A missing corpus is a failure, not a skip: without it this test would
    // pass having checked nothing. CI checks the submodule out.
    assert!(
        corpus_available(),
        "JSONTestSuite corpus not found at {}. Run \
         `git submodule update --init tests/fixtures/JSONTestSuite`.",
        corpus_dir().display()
    );

    let results = run_corpus();
    let baseline = read_baseline();

    // If no baseline exists yet, print a suggested one and fail so the
    // developer commits it deliberately.
    if baseline.is_empty() {
        let (y_pass, y_total, n_reject, n_total, i_pass, i_reject, i_skip) = tally(&results);
        eprintln!(
            "No baseline at {}. Snapshot to lock in:\n\
             y_* pass:   {}/{}\n\
             n_* reject: {}/{}\n\
             i_* breakdown: pass={} reject={} skip={}",
            baseline_path().display(),
            y_pass,
            y_total,
            n_reject,
            n_total,
            i_pass,
            i_reject,
            i_skip
        );
        write_baseline(&results);
        panic!(
            "wrote fresh baseline to {} — inspect and commit",
            baseline_path().display()
        );
    }

    // Compare per-file. Gate on y_* and n_*; warn on i_*.
    let mut y_regressions = Vec::new();
    let mut n_regressions = Vec::new();
    let mut i_warnings = Vec::new();
    let mut new_files = Vec::new();

    for (name, actual) in &results {
        let Some(cat) = category(name) else { continue };
        match baseline.get(name) {
            None => new_files.push((name.clone(), *actual)),
            Some(expected) if expected == actual => {}
            Some(expected) => {
                let msg = format!(
                    "{}: baseline={} actual={}",
                    name,
                    expected.as_str(),
                    actual.as_str()
                );
                match cat {
                    'y' => y_regressions.push(msg),
                    'n' => n_regressions.push(msg),
                    'i' => i_warnings.push(msg),
                    _ => {}
                }
            }
        }
    }

    for w in &i_warnings {
        eprintln!("::warning::i_* delta: {}", w);
    }
    for (name, actual) in &new_files {
        eprintln!(
            "::warning::new fixture not in baseline: {} → {}",
            name,
            actual.as_str()
        );
    }

    if !y_regressions.is_empty() || !n_regressions.is_empty() {
        let mut msg = String::from("JSONTestSuite conformance regression:\n");
        for r in &y_regressions {
            msg.push_str(&format!("  [y_] {}\n", r));
        }
        for r in &n_regressions {
            msg.push_str(&format!("  [n_] {}\n", r));
        }
        panic!("{}", msg);
    }
}

fn tally(results: &BTreeMap<String, Outcome>) -> (usize, usize, usize, usize, usize, usize, usize) {
    let (mut y_pass, mut y_total) = (0, 0);
    let (mut n_reject, mut n_total) = (0, 0);
    let (mut i_pass, mut i_reject, mut i_skip) = (0, 0, 0);
    for (name, o) in results {
        match category(name) {
            Some('y') => {
                y_total += 1;
                if *o == Outcome::Pass {
                    y_pass += 1;
                }
            }
            Some('n') => {
                n_total += 1;
                if *o == Outcome::Reject {
                    n_reject += 1;
                }
            }
            Some('i') => match o {
                Outcome::Pass => i_pass += 1,
                Outcome::Reject => i_reject += 1,
                Outcome::Skip => i_skip += 1,
            },
            _ => {}
        }
    }
    (y_pass, y_total, n_reject, n_total, i_pass, i_reject, i_skip)
}

fn write_baseline(results: &BTreeMap<String, Outcome>) {
    let (y_pass, y_total, n_reject, n_total, i_pass, i_reject, i_skip) = tally(results);
    let mut out = String::new();
    out.push_str("# JSONTestSuite conformance baseline for _json_decode_v2.\n");
    out.push_str("# Generated by tests/json_conformance.rs — do not edit by hand.\n");
    out.push_str("# Regenerate: `cargo test --test json_conformance` after deleting this file.\n");
    out.push_str(&format!(
        "# Summary: y={}/{} pass, n={}/{} reject, i pass={} reject={} skip={}\n",
        y_pass, y_total, n_reject, n_total, i_pass, i_reject, i_skip
    ));
    for (name, outcome) in results {
        out.push_str(&format!("{} {}\n", name, outcome.as_str()));
    }
    fs::write(baseline_path(), out).expect("write baseline");
}
