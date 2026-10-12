//! zoo-bench: latency and throughput numbers for zoanthrope's claim
//! path, postboard, and state persistence.
//!
//! A plain timed loop, not criterion: the deliverable is a report with
//! p50/p95/p99 per operation, and a statistical regression harness
//! earns nothing here. Sample counts scale with `ZOO_BENCH_SCALE`
//! (default 1).
//!
//! ```text
//! cargo run --release --bin zoo-bench          # human table + JSON
//! cargo run --release --bin zoo-bench -- --json  # JSON only
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use serde_json::{json, Map, Value};
use zoanthrope::state::{self, ClaimIntent, State};
use zoanthrope::tools;

fn main() {
    let json_only = std::env::args().any(|a| a == "--json");
    let scale: usize = std::env::var("ZOO_BENCH_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);

    let mut rows: Vec<Value> = Vec::new();
    let mut extras: Map<String, Value> = Map::new();

    let root = bench_root("uncontended");
    let (claim, release, pair) = uncontended(&root, 300 * scale);
    rows.push(stat("claim_release/uncontended/claim", &claim));
    rows.push(stat("claim_release/uncontended/release", &release));
    rows.push(stat("claim_release/uncontended/pair", &pair));
    let _ = std::fs::remove_dir_all(&root);

    let root = bench_root("contended");
    let c = contended(&root, 8, 80 * scale);
    rows.push(stat("claim_release/contended8/claim_ok", &c.claim_ok));
    rows.push(stat(
        "claim_release/contended8/claim_conflict",
        &c.claim_conflict,
    ));
    rows.push(stat("claim_release/contended8/release", &c.release));
    rows.push(stat("claim_release/contended8/pair", &c.pair));
    extras.insert(
        "contended8".to_string(),
        json!({
            "threads": 8,
            "hot_files": 3,
            "grants": c.grants,
            "conflicts": c.conflicts,
        }),
    );
    let _ = std::fs::remove_dir_all(&root);

    let root = bench_root("post");
    let p = post_roundtrip(&root, 200 * scale);
    rows.push(stat("postboard/roundtrip/post", &p.post));
    rows.push(stat("postboard/roundtrip/list", &p.list));
    rows.push(stat("postboard/roundtrip/total", &p.total));
    let (create, list1000, posts_per_sec) = postboard_1000(&root, 1000, 50);
    rows.push(stat("postboard/list_1000_posts", &list1000));
    extras.insert(
        "postboard".to_string(),
        json!({
            "posts_created": 1000,
            "create_pair_latency": stat("postboard/create_1000", &create),
            "create_throughput_posts_per_sec": posts_per_sec,
        }),
    );
    let _ = std::fs::remove_dir_all(&root);

    let root = bench_root("persistence");
    let (disk, mem) = persistence(&root, 200 * scale);
    rows.push(stat("persistence/disk_claim_release_pair", &disk));
    rows.push(stat("persistence/memory_claim_release_pair", &mem));
    let _ = std::fs::remove_dir_all(&root);

    let out = json!({
        "bench": "zoo-bench",
        "scale": scale,
        "rows": rows,
        "extras": extras,
    });

    if json_only {
        println!("{}", out);
        return;
    }
    println!(
        "zoanthrope zoo-bench — plain timed loop, sample counts x{scale} \
         (ZOO_BENCH_SCALE), latencies in microseconds"
    );
    println!(
        "{:<46} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "section", "n", "p50", "p95", "p99", "mean", "max"
    );
    for r in &rows {
        println!(
            "{:<46} {:>6} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
            r["name"].as_str().unwrap_or("?"),
            r["n"].as_u64().unwrap_or(0),
            r["p50_us"].as_f64().unwrap_or(0.0),
            r["p95_us"].as_f64().unwrap_or(0.0),
            r["p99_us"].as_f64().unwrap_or(0.0),
            r["mean_us"].as_f64().unwrap_or(0.0),
            r["max_us"].as_f64().unwrap_or(0.0),
        );
    }
    println!("\nextras: {}", Value::Object(extras));
    println!("--- json ---");
    println!("{}", out);
}

// ─── stats ──────────────────────────────────────────────────────────

fn stat(name: &str, samples: &[u64]) -> Value {
    let mut s = samples.to_vec();
    s.sort_unstable();
    let n = s.len();
    let pct = |p: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        let idx = ((p / 100.0) * (n as f64 - 1.0)).round() as usize;
        s[idx.min(n - 1)] as f64 / 1000.0
    };
    let mean = if n == 0 {
        0.0
    } else {
        s.iter().sum::<u64>() as f64 / n as f64 / 1000.0
    };
    let max = s.last().copied().unwrap_or(0) as f64 / 1000.0;
    json!({
        "name": name,
        "n": n,
        "p50_us": pct(50.0),
        "p95_us": pct(95.0),
        "p99_us": pct(99.0),
        "mean_us": mean,
        "max_us": max,
    })
}

fn bench_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoo-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("bench workspace");
    dir
}

fn timed(mut f: impl FnMut()) -> u64 {
    let t0 = Instant::now();
    f();
    t0.elapsed().as_nanos() as u64
}

fn call(root: &Path, name: &str, args: Value) {
    tools::call_tool(root, name, &args).unwrap_or_else(|e| panic!("{name}: {e}"));
}

// ─── sections ───────────────────────────────────────────────────────

/// Single thread, single private file: the uncontended cost of a
/// claim+release round trip through the real tool surface.
fn uncontended(root: &Path, n: usize) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
    let mut claim = Vec::with_capacity(n);
    let mut release = Vec::with_capacity(n);
    let mut pair = Vec::with_capacity(n);
    for i in 0..n {
        let file = "u0.rs";
        let total = timed(|| {
            claim.push(timed(|| {
                call(
                    root,
                    "claim_files",
                    json!({"agent_id": "bench-u", "files": [file], "purpose": format!("it={i}")}),
                );
            }));
            release.push(timed(|| {
                call(
                    root,
                    "claim_files",
                    json!({"agent_id": "bench-u", "mode": "release", "files": [file]}),
                );
            }));
        });
        pair.push(total);
    }
    (claim, release, pair)
}

struct Contended {
    claim_ok: Vec<u64>,
    claim_conflict: Vec<u64>,
    release: Vec<u64>,
    pair: Vec<u64>,
    grants: u64,
    conflicts: u64,
}

/// Eight threads on three hot files — the contested path. Each granted
/// claim holds through a tiny real edit before the release, the way
/// agents actually use the tool; back-to-back claim/release would
/// finish inside the lock's poll gap and never contend for the file.
fn contended(root: &Path, threads: usize, iters: usize) -> Contended {
    std::fs::create_dir_all(root.join("works")).expect("bench works dir");
    let mut handles = Vec::new();
    for t in 0..threads {
        let root = root.to_path_buf();
        handles.push(thread::spawn(move || {
            let agent = format!("bench-c{t}");
            let mut claim_ok = Vec::with_capacity(iters);
            let mut claim_conflict = Vec::with_capacity(iters);
            let mut release = Vec::with_capacity(iters);
            let mut pair = Vec::with_capacity(iters);
            let (mut grants, mut conflicts) = (0u64, 0u64);
            for i in 0..iters {
                let file = format!("hot{}.rs", i % 3);
                let mut ok = false;
                let ct = timed(|| {
                    let v = tools::call_tool(
                        &root,
                        "claim_files",
                        &json!({"agent_id": agent.as_str(), "files": [file.as_str()], "purpose": "bench"}),
                    )
                    .expect("claim call");
                    ok = v.get("ok").and_then(Value::as_bool) == Some(true);
                });
                if ok {
                    claim_ok.push(ct);
                    grants += 1;
                } else {
                    claim_conflict.push(ct);
                    conflicts += 1;
                }
                if ok {
                    // The micro-edit the claim is held for (~2ms, like
                    // a real save): short holds finish inside the lock
                    // poll gap and never contend for the file.
                    let mut edit = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(root.join("works").join(&file))
                        .expect("bench edit");
                    let _ = writeln!(edit, "agent={agent} it={i}");
                    let _ = writeln!(edit, "agent={agent} it={i} second line");
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    let rt = timed(|| {
                        call(
                            &root,
                            "claim_files",
                            json!({"agent_id": agent.as_str(), "mode": "release", "files": [file.as_str()]}),
                        );
                    });
                    release.push(rt);
                    // Pair = the two tool calls only; the edit shapes
                    // the contention but is not part of the latency.
                    pair.push(ct + rt);
                }
            }
            (claim_ok, claim_conflict, release, pair, grants, conflicts)
        }));
    }
    let mut out = Contended {
        claim_ok: Vec::new(),
        claim_conflict: Vec::new(),
        release: Vec::new(),
        pair: Vec::new(),
        grants: 0,
        conflicts: 0,
    };
    for h in handles {
        let (ok, conf, rel, pair, g, c) = h.join().expect("bench thread");
        out.claim_ok.extend(ok);
        out.claim_conflict.extend(conf);
        out.release.extend(rel);
        out.pair.extend(pair);
        out.grants += g;
        out.conflicts += c;
    }
    out
}

struct PostRoundtrip {
    post: Vec<u64>,
    list: Vec<u64>,
    total: Vec<u64>,
}

/// `post_intention` → `list_posts` round trip latency.
fn post_roundtrip(root: &Path, n: usize) -> PostRoundtrip {
    let mut post = Vec::with_capacity(n);
    let mut list = Vec::with_capacity(n);
    let mut total = Vec::with_capacity(n);
    for i in 0..n {
        let t = timed(|| {
            post.push(timed(|| {
                call(
                    root,
                    "post_intention",
                    json!({
                        "agent_id": "bench-p",
                        "title": format!("intention {i}"),
                        "note": "bench",
                        "files": ["a.rs"],
                    }),
                );
            }));
            list.push(timed(|| {
                call(root, "list_posts", json!({"agent_id": "bench-p"}));
            }));
        });
        total.push(t);
    }
    PostRoundtrip { post, list, total }
}

/// Create 1000 posts (write throughput), then time `list_posts` with
/// the full board.
fn postboard_1000(root: &Path, posts: usize, list_samples: usize) -> (Vec<u64>, Vec<u64>, f64) {
    let mut create = Vec::with_capacity(posts);
    let t0 = Instant::now();
    for i in 0..posts {
        create.push(timed(|| {
            call(
                root,
                "post_intention",
                json!({
                    "agent_id": "bench-board",
                    "title": format!("post {i}"),
                    "note": "filling the board",
                    "files": [],
                }),
            );
        }));
    }
    let elapsed = t0.elapsed().as_secs_f64().max(f64::EPSILON);
    let posts_per_sec = posts as f64 / elapsed;
    let mut list = Vec::with_capacity(list_samples);
    for _ in 0..list_samples {
        list.push(timed(|| {
            call(root, "list_posts", json!({}));
        }));
    }
    (create, list, posts_per_sec)
}

/// The persistence cost: the same claim+release bookkeeping with the
/// state file on disk (real load+save per call) vs in memory.
fn persistence(root: &Path, n: usize) -> (Vec<u64>, Vec<u64>) {
    let mut disk = Vec::with_capacity(n);
    for i in 0..n {
        let file = format!("p{}.rs", i % 8);
        disk.push(timed(|| {
            call(
                root,
                "claim_files",
                json!({"agent_id": "bench-d", "files": [file.as_str()], "purpose": "disk"}),
            );
            call(
                root,
                "claim_files",
                json!({"agent_id": "bench-d", "mode": "release", "files": [file.as_str()]}),
            );
        }));
    }

    let mut mem = Vec::with_capacity(n);
    let mut s = State::default();
    let now = state::now_unix();
    s.register("bench-m", "bench", now);
    for _ in 0..n {
        mem.push(timed(|| {
            let now = state::now_unix();
            s.claim(
                "bench-m",
                &[("m0.rs".to_string(), None)],
                ClaimIntent::Edit,
                now,
                900,
            );
            s.release("bench-m", &["m0.rs".to_string()], now);
        }));
    }
    (disk, mem)
}
