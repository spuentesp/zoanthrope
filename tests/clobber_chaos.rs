//! Clobber chaos: real-concurrency evidence for zoanthrope's
//! anti-clobbering promise.
//!
//! Run the full harness (CI-able, seconds-to-a-minute):
//!
//! ```text
//! cargo test --release --test clobber_chaos -- --ignored --nocapture
//! ```
//!
//! Knobs: `ZOO_CHAOS_WORKERS` (16), `ZOO_CHAOS_ITERS` (200),
//! `ZOO_CHAOS_FILES` (50), `ZOO_CHAOS_MODE` (`claim` | `stomp`),
//! `ZOO_CHAOS_SEED` (fixed default for reproducibility).
//!
//! # What is under test
//!
//! The real machinery: `tools::call_tool("claim_files", ...)` — the
//! exact surface the MCP server serves — over a shared on-disk
//! workspace (`.zoanthrope/state.json` + `.zoanthrope/locks/`), with
//! genuine multi-threaded contention and real file I/O for the "edits".
//!
//! # The invariant
//!
//! **At no point do two live claim windows cover the same file.**
//!
//! # How it is detected — and why that is honest
//!
//! Workers only emit events; they never decide pass/fail. Detection is
//! post-hoc over independent evidence, in three channels:
//!
//! 1. **Claim windows from state (the registry's own epochs).** The
//!    open bracket is the grant body's `claimed_at` and the close
//!    bracket is the `ts` of the `release` audit event — both stamped
//!    by the tool *inside* its locked read-modify-write, so the
//!    brackets order with the arbitration itself and a descheduled
//!    worker cannot hide a double-grant. Two windows on one file with
//!    overlapping `[open, close)` intervals mean the tool granted two
//!    live claims at once. A harness-side total-order sequence counter
//!    brackets the same calls from outside as a secondary channel.
//! 2. **File artifacts (physical evidence).** Each "edit" appends real
//!    lines carrying `agent`, `iter`, window id, and the write's
//!    sequence number: an `open` boundary, data lines, a `close`
//!    boundary. The file alone reconstructs each write window; two
//!    windows on one file with overlapping intervals, or a window
//!    whose data lines interleave with a foreign window's, is a
//!    physical double-write. This channel never consults the tool.
//! 3. **Tool reports (protocol channel).** At edit time the harness
//!    asks `list_claims` who holds the file. In claim mode the tool
//!    must report the editing agent as holder (a miss is registry
//!    divergence). In stomp mode the harness checks the opposite:
//!    every deliberate unclaimed edit must be flagged, and any
//!    *foreign* holder the tool reports at that moment is counted.
//!
//! Stomp mode is the positive control: workers deliberately edit
//! without claiming, so actual double-writes are guaranteed on hot
//! files. The artifact channel must detect every one (100%), which
//! calibrates the detector that then has to stay silent in claim mode.
//! An "undetected violation" is a physical write overlap the claim
//! bookkeeping says was sequential — a detector gap or a tool
//! divergence — and fails the run.
//!
//! Exit criteria (`#[test]` assertion):
//!
//! * claim mode: zero claim-window overlaps (both bracket channels),
//!   zero artifact overlaps, zero segment violations, zero protocol
//!   misses, zero undetected double-writes;
//! * stomp mode: the positive control fired (actual stomps > 0), the
//!   artifact channel detected 100% of them, and 100% of the
//!   deliberate edits were flagged as unclaimed.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zoanthrope::tools;

// ─── configuration ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Claim,
    Stomp,
}

impl Mode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claim" => Some(Self::Claim),
            "stomp" => Some(Self::Stomp),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Stomp => "stomp",
        }
    }
}

#[derive(Debug, Clone)]
struct Cfg {
    workers: usize,
    iters: usize,
    files: usize,
    mode: Mode,
    seed: u64,
}

impl Cfg {
    fn from_env() -> Self {
        Self {
            workers: env_usize("ZOO_CHAOS_WORKERS", 16).max(1),
            iters: env_usize("ZOO_CHAOS_ITERS", 200).max(1),
            files: env_usize("ZOO_CHAOS_FILES", 50).max(2),
            mode: std::env::var("ZOO_CHAOS_MODE")
                .ok()
                .as_deref()
                .and_then(Mode::parse)
                .unwrap_or(Mode::Claim),
            seed: env_usize("ZOO_CHAOS_SEED", 0x0005_2026) as u64,
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

// ─── tiny deterministic PRNG (no new deps) ──────────────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

// ─── events and evidence ────────────────────────────────────────────

/// One claim/edit window on one file.
///
/// `state_open` / `state_close` are the tool-issued epochs bracketing
/// registry liveness (`claimed_at` from the grant body; `ts` of the
/// `release` audit event). `seq_open` / `seq_close` are the harness
/// counter brackets around the same calls. `edit_open` / `edit_close`
/// bracket the write footprint. In stomp mode there is no claim and
/// all three coincide.
#[derive(Debug, Clone)]
struct Window {
    agent: String,
    iter: usize,
    file: String,
    state_open: f64,
    state_close: Option<f64>,
    seq_open: u64,
    seq_close: u64,
    edit_open: u64,
    edit_close: u64,
    /// What `list_claims` reported at edit time: who held the file.
    tool_holder: Option<String>,
}

impl Window {
    fn key(&self) -> String {
        format!("{}:{}", self.agent, self.iter)
    }
}

#[derive(Debug, Default, Clone)]
struct Stats {
    iterations: u64,
    grants: u64,
    conflicts: u64,
    backoffs: u64,
    tool_errors: u64,
    lock_timeouts: u64,
    windows: u64,
    release_calls: u64,
    release_misses: u64,
    epoch_misses: u64,
    lines: u64,
}

#[derive(Debug, Default)]
struct Totals {
    /// Ground truth (harness stamps): edit-window overlaps on a file.
    gt_overlaps: u64,
    /// Channel 1: state-epoch claim windows overlapping on a file.
    state_overlaps: u64,
    /// Channel 1b: harness sequence brackets overlapping on a file.
    seq_overlaps: u64,
    /// Channel 2: artifact edit-window overlaps.
    artifact_overlaps: u64,
    /// Channel 2 stricter: data-only interval overlaps.
    data_overlaps: u64,
    /// Channel 2: interleaved data segments in the files.
    segment_violations: u64,
    /// Channel 3 (claim): edits where the tool did not report us.
    protocol_misses: u64,
    /// Channel 3 (stomp): edits flagged as unclaimed.
    protocol_flagged: u64,
    /// Channel 3 (stomp): edits where the tool reported a foreign holder.
    tool_reported_foreign: u64,
    /// Ground-truth overlaps the artifact channel missed.
    detector_misses: u64,
    /// Physical write overlap the claim bookkeeping says was
    /// sequential — the "undetected double-write" class.
    undetected: u64,
    /// Artifact overlap with no matching ground-truth pair.
    spurious_artifact: u64,
}

// ─── worker plumbing ────────────────────────────────────────────────

fn works_dir(root: &Path) -> PathBuf {
    root.join("works")
}

fn works_path(root: &Path, name: &str) -> PathBuf {
    // `name` is the claimed path ("works/fNNN.txt") — resolve it under
    // the workspace root.
    root.join(name)
}

fn file_name(i: usize) -> String {
    format!("works/f{i:03}.txt")
}

fn append_line(path: &Path, line: &str) {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open works file");
    f.write_all(line.as_bytes()).expect("append works line");
}

fn line(kind: &str, agent: &str, iter: usize, w: &str, seq: u64) -> String {
    format!("agent={agent} iter={iter} w={w} seq={seq} kind={kind}\n")
}

/// Pick 1–3 files with intentional overlap: the first 5 names are hot
/// and drawn far more often than the rest.
fn pick_files(rng: &mut Rng, names: &[String]) -> Vec<String> {
    let hot = names.len().min(5);
    let k = 1 + rng.below(3) as usize;
    let mut out: Vec<String> = Vec::with_capacity(k);
    for _ in 0..k {
        let f = if rng.below(100) < 40 {
            names[rng.below(hot as u64) as usize].clone()
        } else {
            names[rng.below(names.len() as u64) as usize].clone()
        };
        if !out.contains(&f) {
            out.push(f);
        }
    }
    out
}

fn is_conflict(v: &Value) -> bool {
    v.get("ok").and_then(Value::as_bool) == Some(false) && v.get("holder").is_some()
}

/// Global total-order sequence counter shared by all workers.
#[derive(Clone)]
struct SeqCounter(Arc<AtomicU64>);

impl SeqCounter {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }

    fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

fn worker(
    root: PathBuf,
    agent: String,
    cfg: Cfg,
    names: Vec<String>,
    seq: SeqCounter,
    wid: usize,
) -> (Stats, Vec<Window>) {
    let mut rng = Rng::new(cfg.seed ^ (wid as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut stats = Stats::default();
    let mut windows = Vec::new();

    for it in 0..cfg.iters {
        stats.iterations += 1;
        let files = pick_files(&mut rng, &names);
        let wbase = format!("{agent}:{it}");

        match cfg.mode {
            Mode::Claim => {
                let args = json!({
                    "agent_id": agent,
                    "intent": "edit",
                    "purpose": format!("chaos it={it}"),
                    "files": files,
                });
                let res = match tools::call_tool(&root, "claim_files", &args) {
                    Ok(v) => v,
                    Err(e) => {
                        stats.tool_errors += 1;
                        if e.contains("timed out") {
                            stats.lock_timeouts += 1;
                        }
                        continue;
                    }
                };
                if is_conflict(&res) {
                    stats.conflicts += 1;
                    if rng.below(2) == 0 {
                        stats.backoffs += 1;
                        thread::sleep(Duration::from_micros(50 + rng.below(450)));
                    }
                    continue;
                }
                if res.get("ok").and_then(Value::as_bool) != Some(true) {
                    stats.tool_errors += 1;
                    continue;
                }
                stats.grants += 1;
                // Open brackets: tool-issued epoch from the grant body
                // (stamped inside the tool's locked update), plus the
                // harness counter taken as close to the return as
                // possible.
                let state_open = res
                    .get("granted")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("claimed_at"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let seq_open = seq.next();
                let holders = tool_holders(&root);
                let mut batch = Vec::new();
                for f in &files {
                    let w = wbase.clone();
                    let edit_open = seq.next();
                    append_line(
                        &works_path(&root, f),
                        &line("open", &agent, it, &w, edit_open),
                    );
                    stats.lines += 1;
                    for _sub in 0..3 {
                        let s = seq.next();
                        append_line(&works_path(&root, f), &line("data", &agent, it, &w, s));
                        stats.lines += 1;
                        thread::yield_now();
                    }
                    let edit_close = seq.next();
                    append_line(
                        &works_path(&root, f),
                        &line("close", &agent, it, &w, edit_close),
                    );
                    stats.lines += 1;
                    stats.windows += 1;
                    batch.push(Window {
                        agent: agent.clone(),
                        iter: it,
                        file: f.clone(),
                        state_open,
                        state_close: None,
                        seq_open,
                        seq_close: 0,
                        edit_open,
                        edit_close,
                        tool_holder: holders.get(f).cloned(),
                    });
                }
                let seq_close = seq.next();
                for w in &mut batch {
                    w.seq_close = seq_close;
                }
                let rel = tools::call_tool(
                    &root,
                    "claim_files",
                    &json!({"agent_id": agent, "mode": "release", "files": files}),
                );
                stats.release_calls += 1;
                match rel {
                    Ok(v) => {
                        let released: Vec<String> = v
                            .get("released")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if released.len() != files.len() {
                            stats.release_misses += 1;
                        }
                    }
                    Err(e) => {
                        stats.tool_errors += 1;
                        if e.contains("timed out") {
                            stats.lock_timeouts += 1;
                        }
                    }
                }
                // Close bracket from state: the `ts` of our release
                // audit events (stamped inside the tool's locked
                // update). Falls back to None when the ring has
                // already rolled over.
                let rel_ts = release_epoch(&root, &agent, &files);
                match rel_ts {
                    Some(ts) => {
                        for w in &mut batch {
                            w.state_close = Some(ts);
                        }
                    }
                    None => stats.epoch_misses += 1,
                }
                windows.extend(batch);
            }
            Mode::Stomp => {
                let holders = tool_holders(&root);
                for f in &files {
                    let w = wbase.clone();
                    let edit_open = seq.next();
                    append_line(
                        &works_path(&root, f),
                        &line("open", &agent, it, &w, edit_open),
                    );
                    stats.lines += 1;
                    for _sub in 0..3 {
                        let s = seq.next();
                        append_line(&works_path(&root, f), &line("data", &agent, it, &w, s));
                        stats.lines += 1;
                        thread::yield_now();
                    }
                    let edit_close = seq.next();
                    append_line(
                        &works_path(&root, f),
                        &line("close", &agent, it, &w, edit_close),
                    );
                    stats.lines += 1;
                    stats.windows += 1;
                    windows.push(Window {
                        agent: agent.clone(),
                        iter: it,
                        file: f.clone(),
                        state_open: 0.0,
                        state_close: None,
                        seq_open: edit_open,
                        seq_close: edit_close,
                        edit_open,
                        edit_close,
                        tool_holder: holders.get(f).cloned(),
                    });
                }
            }
        }

        // Brief move-away jitter so workers do not lock-step.
        if rng.below(100) < 15 {
            thread::sleep(Duration::from_micros(rng.below(200)));
        }
    }
    (stats, windows)
}

/// Map of path → holder agent from the tool's own `list_claims` view
/// (the protocol channel: what the tool reports at this instant).
fn tool_holders(root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Ok(v) = tools::call_tool(root, "list_claims", &json!({})) {
        if let Some(claims) = v.get("claims").and_then(Value::as_array) {
            for c in claims {
                if let (Some(p), Some(a)) = (
                    c.get("path").and_then(Value::as_str),
                    c.get("agent").and_then(Value::as_str),
                ) {
                    out.insert(p.to_string(), a.to_string());
                }
            }
        }
    }
    out
}

/// The `ts` of `agent`'s latest `release` audit event for any of
/// `files` — the tool-issued close bracket of the claim window.
fn release_epoch(root: &Path, agent: &str, files: &[String]) -> Option<f64> {
    let v = tools::call_tool(root, "get_audit_log", &json!({"limit": 400})).ok()?;
    let events = v.get("events").and_then(Value::as_array)?;
    let mut best: Option<f64> = None;
    for e in events {
        if e.get("agent").and_then(Value::as_str) != Some(agent) {
            continue;
        }
        if e.get("action").and_then(Value::as_str) != Some("release") {
            continue;
        }
        let path = e.get("path").and_then(Value::as_str).unwrap_or("");
        if !files.iter().any(|f| f == path) {
            continue;
        }
        if let Some(ts) = e.get("ts").and_then(Value::as_f64) {
            if best.is_none_or(|b| ts > b) {
                best = Some(ts);
            }
        }
    }
    best
}

// ─── post-hoc detectors ─────────────────────────────────────────────

fn intervals_overlap(a_open: f64, a_close: f64, b_open: f64, b_close: f64) -> bool {
    // Half-open [open, close): touching brackets are sequential.
    a_open < b_close && b_open < a_close
}

fn seq_overlap(a_open: u64, a_close: u64, b_open: u64, b_close: u64) -> bool {
    a_open < b_close && b_open < a_close
}

fn sorted_pair(a: &str, b: &str) -> (String, String) {
    if a < b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

#[derive(Debug)]
struct ArtifactWindow {
    key: String,
    open_seq: Option<u64>,
    close_seq: Option<u64>,
    data_seqs: Vec<u64>,
}

struct ArtifactReport {
    overlaps: Vec<(String, String)>,
    data_overlaps: Vec<(String, String)>,
    segment_violations: u64,
}

/// Reconstruct write windows from the file artifacts alone.
fn analyze_artifact(path: &Path) -> ArtifactReport {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut data_order: Vec<String> = Vec::new();
    let mut by_w: HashMap<String, ArtifactWindow> = HashMap::new();
    for l in raw.lines() {
        let mut w = String::new();
        let mut seq = 0u64;
        let mut kind = String::new();
        for tok in l.split_whitespace() {
            if let Some((k, v)) = tok.split_once('=') {
                match k {
                    "w" => w = v.to_string(),
                    "seq" => seq = v.parse().unwrap_or(0),
                    "kind" => kind = v.to_string(),
                    _ => {}
                }
            }
        }
        if w.is_empty() {
            continue;
        }
        let e = by_w.entry(w.clone()).or_insert_with(|| ArtifactWindow {
            key: w.clone(),
            open_seq: None,
            close_seq: None,
            data_seqs: Vec::new(),
        });
        match kind.as_str() {
            "open" => e.open_seq = Some(seq),
            "close" => e.close_seq = Some(seq),
            _ => {
                e.data_seqs.push(seq);
                data_order.push(w);
            }
        }
    }

    let list: Vec<&ArtifactWindow> = by_w.values().collect();
    let mut overlaps = Vec::new();
    let mut data_overlaps = Vec::new();
    for i in 0..list.len() {
        for j in (i + 1)..list.len() {
            let (a, b) = (list[i], list[j]);
            if let (Some(ao), Some(ac), Some(bo), Some(bc)) =
                (a.open_seq, a.close_seq, b.open_seq, b.close_seq)
            {
                if seq_overlap(ao, ac, bo, bc) {
                    overlaps.push(sorted_pair(&a.key, &b.key));
                }
            }
            let (amin, amax) = match (a.data_seqs.iter().min(), a.data_seqs.iter().max()) {
                (Some(lo), Some(hi)) => (*lo, *hi),
                _ => continue,
            };
            let (bmin, bmax) = match (b.data_seqs.iter().min(), b.data_seqs.iter().max()) {
                (Some(lo), Some(hi)) => (*lo, *hi),
                _ => continue,
            };
            if seq_overlap(amin, amax, bmin, bmax) {
                data_overlaps.push(sorted_pair(&a.key, &b.key));
            }
        }
    }

    // Segment check: each window's data lines must form one contiguous
    // run in file order — a window reappearing after a foreign one
    // means interleaved writers on the same file.
    let mut segment_violations = 0u64;
    let mut last_w = String::new();
    let mut closed: HashSet<String> = HashSet::new();
    for w in &data_order {
        if *w != last_w {
            if !last_w.is_empty() {
                closed.insert(last_w.clone());
            }
            if closed.contains(w) {
                segment_violations += 1;
            }
            last_w = w.clone();
        }
    }

    ArtifactReport {
        overlaps,
        data_overlaps,
        segment_violations,
    }
}

// ─── the run ────────────────────────────────────────────────────────

#[derive(Debug)]
struct Summary {
    cfg: Cfg,
    stats: Stats,
    totals: Totals,
    wall: Duration,
    root: PathBuf,
}

fn run(cfg: Cfg) -> Summary {
    let root = std::env::temp_dir().join(format!(
        "zoo-chaos-{}-{}-{}",
        cfg.mode.as_str(),
        std::process::id(),
        cfg.seed
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(works_dir(&root)).expect("create works dir");
    let names: Vec<String> = (0..cfg.files).map(file_name).collect();

    let seq = SeqCounter::new();
    let mut handles = Vec::new();
    let t0 = Instant::now();
    for wid in 0..cfg.workers {
        let root = root.clone();
        let agent = format!("w{wid:02}");
        let cfg = cfg.clone();
        let names = names.clone();
        let seq = seq.clone();
        handles.push(thread::spawn(move || {
            worker(root, agent, cfg, names, seq, wid)
        }));
    }
    let mut stats = Stats::default();
    let mut windows: Vec<Window> = Vec::new();
    for h in handles {
        let (s, w) = h.join().expect("worker thread");
        stats.iterations += s.iterations;
        stats.grants += s.grants;
        stats.conflicts += s.conflicts;
        stats.backoffs += s.backoffs;
        stats.tool_errors += s.tool_errors;
        stats.lock_timeouts += s.lock_timeouts;
        stats.windows += s.windows;
        stats.release_calls += s.release_calls;
        stats.release_misses += s.release_misses;
        stats.epoch_misses += s.epoch_misses;
        stats.lines += s.lines;
        windows.extend(w);
    }
    let wall = t0.elapsed();

    // ── channel 1: claim windows bracketed by state epochs ──
    let mut by_file: HashMap<String, Vec<&Window>> = HashMap::new();
    for w in &windows {
        by_file.entry(w.file.clone()).or_default().push(w);
    }
    let mut totals = Totals::default();
    let mut state_pairs: Vec<(String, String, String)> = Vec::new();
    let mut seq_pairs: Vec<(String, String, String)> = Vec::new();
    let mut gt_pairs: Vec<(String, String, String)> = Vec::new();
    for (file, ws) in &by_file {
        for i in 0..ws.len() {
            for j in (i + 1)..ws.len() {
                let (a, b) = (ws[i], ws[j]);
                if let (Some(ac), Some(bc)) = (a.state_close, b.state_close) {
                    if intervals_overlap(a.state_open, ac, b.state_open, bc) {
                        totals.state_overlaps += 1;
                        state_pairs.push((file.clone(), a.key(), b.key()));
                    }
                }
                if seq_overlap(a.seq_open, a.seq_close, b.seq_open, b.seq_close) {
                    totals.seq_overlaps += 1;
                    seq_pairs.push((file.clone(), a.key(), b.key()));
                }
                if seq_overlap(a.edit_open, a.edit_close, b.edit_open, b.edit_close) {
                    totals.gt_overlaps += 1;
                    gt_pairs.push((file.clone(), a.key(), b.key()));
                }
            }
        }
    }

    // ── channel 2: file artifacts alone ──
    let mut artifact_pairs: Vec<(String, String, String)> = Vec::new();
    let mut data_pairs: Vec<(String, String, String)> = Vec::new();
    for name in &names {
        let rep = analyze_artifact(&works_path(&root, name));
        totals.segment_violations += rep.segment_violations;
        for (a, b) in rep.overlaps {
            totals.artifact_overlaps += 1;
            artifact_pairs.push((name.clone(), a, b));
        }
        for (a, b) in rep.data_overlaps {
            totals.data_overlaps += 1;
            data_pairs.push((name.clone(), a, b));
        }
    }

    // ── channel 3: protocol / tool reports ──
    for w in &windows {
        let held_by_self = w.tool_holder.as_deref() == Some(w.agent.as_str());
        match cfg.mode {
            Mode::Claim => {
                if !held_by_self {
                    totals.protocol_misses += 1;
                }
            }
            Mode::Stomp => {
                if !held_by_self {
                    totals.protocol_flagged += 1;
                }
                if w.tool_holder.is_some() && !held_by_self {
                    totals.tool_reported_foreign += 1;
                }
            }
        }
    }

    // ── cross-channel accounting (the honest numbers) ──
    let gt_set: HashSet<(String, String, String)> = gt_pairs.iter().cloned().collect();
    let art_set: HashSet<(String, String, String)> = artifact_pairs.iter().cloned().collect();
    let claim_set: HashSet<(String, String, String)> = state_pairs
        .iter()
        .chain(seq_pairs.iter())
        .cloned()
        .collect();
    totals.detector_misses = gt_pairs.iter().filter(|p| !art_set.contains(*p)).count() as u64;
    totals.spurious_artifact = artifact_pairs
        .iter()
        .filter(|p| !gt_set.contains(*p))
        .count() as u64;
    // Undetected double-write: physical write overlap with no claim
    // window overlap behind it — the tool's model says "sequential"
    // while the files say "concurrent".
    totals.undetected = data_pairs
        .iter()
        .chain(artifact_pairs.iter())
        .filter(|p| !claim_set.contains(*p))
        .count() as u64;

    Summary {
        cfg,
        stats,
        totals,
        wall,
        root,
    }
}

// ─── reporting ──────────────────────────────────────────────────────

impl Summary {
    fn ops_per_sec(&self) -> f64 {
        let s = self.wall.as_secs_f64();
        if s > 0.0 {
            self.stats.iterations as f64 / s
        } else {
            0.0
        }
    }

    fn to_json(&self) -> Value {
        let t = &self.totals;
        let s = &self.stats;
        json!({
            "mode": self.cfg.mode.as_str(),
            "workers": self.cfg.workers,
            "iters_per_worker": self.cfg.iters,
            "files": self.cfg.files,
            "seed": self.cfg.seed,
            "wall_secs": self.wall.as_secs_f64(),
            "ops": {
                "iterations": s.iterations,
                "windows": s.windows,
                "grants": s.grants,
                "conflicts": s.conflicts,
                "backoffs": s.backoffs,
                "tool_errors": s.tool_errors,
                "lock_timeouts": s.lock_timeouts,
                "release_calls": s.release_calls,
                "release_misses": s.release_misses,
                "epoch_misses": s.epoch_misses,
                "lines_written": s.lines,
                "throughput_ops_per_sec": self.ops_per_sec(),
            },
            "violations": {
                "gt_edit_overlaps": t.gt_overlaps,
                "state_epoch_overlaps": t.state_overlaps,
                "seq_bracket_overlaps": t.seq_overlaps,
                "artifact_overlaps": t.artifact_overlaps,
                "data_overlaps": t.data_overlaps,
                "segment_violations": t.segment_violations,
                "protocol_misses": t.protocol_misses,
                "protocol_flagged": t.protocol_flagged,
                "tool_reported_foreign": t.tool_reported_foreign,
                "detector_misses": t.detector_misses,
                "undetected_double_writes": t.undetected,
                "spurious_artifact": t.spurious_artifact,
            },
            "root": self.root.display().to_string(),
        })
    }

    fn print_human(&self) {
        let t = &self.totals;
        let s = &self.stats;
        println!(
            "== clobber chaos: mode={} workers={} iters/worker={} files={} seed={} ==",
            self.cfg.mode.as_str(),
            self.cfg.workers,
            self.cfg.iters,
            self.cfg.files,
            self.cfg.seed
        );
        println!(
            "wall: {:.3}s   throughput: {:.1} iters/s",
            self.wall.as_secs_f64(),
            self.ops_per_sec()
        );
        println!(
            "ops: iterations={} windows={} grants={} conflicts={} backoffs={} tool_errors={} lock_timeouts={}",
            s.iterations, s.windows, s.grants, s.conflicts, s.backoffs, s.tool_errors, s.lock_timeouts
        );
        println!(
            "release: calls={} misses={} epoch_misses={}   lines_written={}",
            s.release_calls, s.release_misses, s.epoch_misses, s.lines
        );
        println!("violations:");
        println!("  ground-truth edit-window overlaps : {}", t.gt_overlaps);
        println!("  state-epoch claim overlaps (ch1)  : {}", t.state_overlaps);
        println!("  seq-bracket overlaps (ch1b)       : {}", t.seq_overlaps);
        println!(
            "  artifact overlaps (ch2)           : {}",
            t.artifact_overlaps
        );
        println!("  data-only overlaps (ch2 strict)   : {}", t.data_overlaps);
        println!(
            "  segment violations (ch2)          : {}",
            t.segment_violations
        );
        println!(
            "  protocol misses (ch3, claim)      : {}",
            t.protocol_misses
        );
        println!(
            "  protocol flagged (ch3, stomp)     : {}",
            t.protocol_flagged
        );
        println!(
            "  tool-reported foreign holder      : {}",
            t.tool_reported_foreign
        );
        println!(
            "  detector misses (gt - artifact)   : {}",
            t.detector_misses
        );
        println!(
            "  spurious artifact                 : {}",
            t.spurious_artifact
        );
        println!("  UNDETECTED double-writes          : {}", t.undetected);
        if self.cfg.mode == Mode::Stomp && t.gt_overlaps > 0 {
            let rate = 100.0 * (t.gt_overlaps - t.detector_misses) as f64 / t.gt_overlaps as f64;
            println!("  detection rate (artifact/gt)      : {rate:.1}%");
        }
        println!("json: {}", self.to_json());
    }

    /// The pass contract. `Err(reasons)` on failure.
    fn check(&self) -> Result<(), String> {
        let t = &self.totals;
        let mut bad = Vec::new();
        if self.stats.release_misses > 0 {
            bad.push(format!("release_misses={}", self.stats.release_misses));
        }
        match self.cfg.mode {
            Mode::Claim => {
                for (name, n) in [
                    ("state_epoch_overlaps", t.state_overlaps),
                    ("seq_bracket_overlaps", t.seq_overlaps),
                    ("artifact_overlaps", t.artifact_overlaps),
                    ("data_overlaps", t.data_overlaps),
                    ("segment_violations", t.segment_violations),
                    ("protocol_misses", t.protocol_misses),
                    ("undetected_double_writes", t.undetected),
                    ("detector_misses", t.detector_misses),
                    ("spurious_artifact", t.spurious_artifact),
                ] {
                    if n > 0 {
                        bad.push(format!("{name}={n}"));
                    }
                }
            }
            Mode::Stomp => {
                if t.gt_overlaps == 0 {
                    bad.push(
                        "positive control produced zero actual double-writes \
                         (detector unvalidated)"
                            .to_string(),
                    );
                }
                if t.detector_misses > 0 {
                    bad.push(format!(
                        "detector missed {}/{} actual stomps",
                        t.detector_misses, t.gt_overlaps
                    ));
                }
                if t.protocol_flagged < self.stats.windows {
                    bad.push(format!(
                        "protocol flagged {}/{} deliberate edits",
                        t.protocol_flagged, self.stats.windows
                    ));
                }
                if t.undetected > 0 {
                    bad.push(format!("undetected_double_writes={}", t.undetected));
                }
            }
        }
        if bad.is_empty() {
            Ok(())
        } else {
            Err(bad.join("; "))
        }
    }
}

// ─── tests ──────────────────────────────────────────────────────────

/// Fast smoke over both modes — keeps the harness exercised in the
/// default `cargo test` lane.
#[test]
fn clobber_chaos_smoke() {
    const NO_POSITIVE: &str = "positive control produced zero actual double-writes \
                              (detector unvalidated)";
    for mode in [Mode::Claim, Mode::Stomp] {
        // The stomp positive control needs two workers to actually
        // interleave on a hot file; at smoke scale that is a random
        // schedule, so resample (bounded) until the control fires.
        // The pass contract is unchanged: real overlaps must happen
        // and the detector must catch every one of them.
        let mut attempt = 0u64;
        loop {
            let cfg = Cfg {
                workers: 2,
                iters: 8,
                files: 4,
                mode,
                seed: 0x0005_2026 ^ attempt,
            };
            let s = run(cfg);
            s.print_human();
            match s.check() {
                Ok(()) => {
                    if mode == Mode::Stomp {
                        assert!(
                            s.totals.protocol_flagged > 0,
                            "stomp smoke must flag deliberate edits"
                        );
                    }
                    let _ = std::fs::remove_dir_all(&s.root);
                    break;
                }
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&s.root);
                    if mode == Mode::Stomp && e == NO_POSITIVE && attempt < 63 {
                        attempt += 1;
                        continue;
                    }
                    panic!("smoke ({}): {e}", mode.as_str());
                }
            }
        }
    }
}

/// The full chaos run. `ZOO_CHAOS_MODE=stomp` turns it into the
/// positive-control calibration. Defaults: 16 workers × 200 iters ×
/// 50 files.
#[test]
#[ignore = "full chaos run: cargo test --release --test clobber_chaos -- --ignored --nocapture"]
fn clobber_chaos() {
    let cfg = Cfg::from_env();
    let s = run(cfg);
    s.print_human();
    match s.check() {
        Ok(()) => {}
        Err(e) => panic!("clobber chaos FAILED: {e}\nsummary: {}", s.to_json()),
    }
}
