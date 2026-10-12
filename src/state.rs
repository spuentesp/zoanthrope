//! Workspace state: claims, agents, the published board, and the
//! audit ring. Persisted as `.zoanthrope/state.json` under the workspace
//! root.
//!
//! Claim arbitration follows the `PresenceClaims` formal spec
//! (`formal/PresenceClaims.tla`): a claim carries an intent (`read` or
//! `edit`), an agent's recorded intent on a path is the *strongest* it
//! holds (never the last one — see `PresenceClaims_LastWins.cfg`), and a
//! new claim is granted unless another agent's recorded intent conflicts
//! (`edit` conflicts with anything; `read` conflicts with `edit`).
//!
//! The board is the derived "who holds what" view. It is published with a
//! generation `epoch` under the rejoin protocol (`formal/RejoinProtocol.tla`
//! variant b): clear-before-read, so a mutation landing mid-rebuild is
//! never lost, and the `(board, epoch)` pair is published as one value so
//! a reader never sees two generations.
//!
//! A holder whose `last_seen` is older than the TTL is `stale`, and its
//! claims are reported as `held_by_stale` — never auto-released.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// State file format version. Bump on incompatible on-disk changes.
/// Version 2: claims carry an intent, the
/// published board section was added.
pub const STATE_VERSION: u32 = 2;

/// Audit ring capacity: oldest events fall off the end.
pub const AUDIT_CAP: usize = 1000;

/// Default presence TTL: an agent silent for longer is `stale`.
pub const DEFAULT_TTL_SECS: u64 = 15 * 60;

const STATE_DIR: &str = ".zoanthrope";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = ".lock";
const LOCK_STALE_SECS: u64 = 10;

/// Lock-acquire deadline. Equals the historical fixed-poll budget
/// (250 polls x 20 ms), so when acquisition fails is unchanged.
const LOCK_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Backoff for lock re-tries: start ~150 us, double up to the cap.
const LOCK_BACKOFF_MIN: Duration = Duration::from_micros(150);
const LOCK_BACKOFF_MAX: Duration = Duration::from_millis(10);

/// Presence TTL in seconds. `ZOANTHROPE_TTL_SECS` overrides the
/// default; an unparseable value falls back to [`DEFAULT_TTL_SECS`].
pub fn ttl_secs() -> u64 {
    parse_ttl(std::env::var("ZOANTHROPE_TTL_SECS").ok().as_deref())
}

fn parse_ttl(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TTL_SECS)
}

/// Walk up from `start` looking for `.git`; fall back to `start`.
pub fn discover_root(start: &Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            return start.to_path_buf();
        }
    }
}

/// True when `last_seen` is strictly older than `ttl` seconds ago.
/// A claim refreshed at exactly `ttl` is still live.
pub fn is_stale(last_seen: f64, now: f64, ttl: u64) -> bool {
    now - last_seen > ttl as f64
}

/// Current wall-clock time as a UNIX-epoch fractional second count.
pub fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn posix(path: &str) -> String {
    path.replace('\\', "/")
}

/// Claim intent. `Edit` is the stronger one; conflicts are symmetric
/// around it: `edit` conflicts with anything, `read` conflicts with
/// `edit` only (`formal/PresenceClaims.tla`, `Conflicts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimIntent {
    Read,
    Edit,
}

impl ClaimIntent {
    /// Parse the CLI/MCP spelling. Anything but `read`/`edit` is an
    /// error — a silent default hides typos in hooks.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "read" => Ok(Self::Read),
            "edit" => Ok(Self::Edit),
            other => Err(format!("intent must be 'read' or 'edit' (got '{other}')")),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
        }
    }

    /// The strongest of two intents (`PresenceClaims`: the recorded
    /// intent is the strongest of everything held, never the last).
    pub fn strongest(self, other: Self) -> Self {
        self.max(other)
    }
}

/// `Conflicts(i, j)` from `formal/PresenceClaims.tla`: an edit
/// conflicts with anything, a read conflicts with an edit.
fn intents_conflict(a: ClaimIntent, b: ClaimIntent) -> bool {
    a == ClaimIntent::Edit || b == ClaimIntent::Edit
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub path: String,
    pub agent: String,
    /// The agent's recorded (strongest) intent on this path.
    pub intent: ClaimIntent,
    pub claimed_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub kind: String,
    pub last_seen: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts: f64,
    pub agent: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

/// The conflict response — the heart of the product. A claim that
/// conflicts with another agent's recorded intent is refused, and this
/// names the holder and what the holder is doing.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub holder: String,
    pub path: String,
    pub claimed_at: f64,
    pub purpose: Option<String>,
    pub holder_stale: bool,
    /// The holder's recorded (strongest) intent on `path`.
    pub intent: ClaimIntent,
}

impl Conflict {
    pub fn to_json(&self) -> Value {
        json!({
            "ok": false,
            "holder": self.holder,
            "path": self.path,
            "claimed_at": self.claimed_at,
            "purpose": self.purpose,
            "holder_stale": self.holder_stale,
            "intent": self.intent.as_str(),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    /// Claims granted to the calling agent, in request order.
    Granted(Vec<Claim>),
    /// Nothing was granted (claims are all-or-nothing per call).
    Conflict(Conflict),
}

/// One row of the published board: who currently holds which path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardHolding {
    pub path: String,
    pub agent: String,
    pub intent: ClaimIntent,
}

/// The published swarm board — the derived "who holds what" view,
/// stamped with the generation it was computed at. Published and read
/// as one value (`RejoinProtocol` I7: a reader sees one generation).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Board {
    pub epoch: u64,
    pub by_path: Vec<BoardHolding>,
    pub claims_count: usize,
    pub agents_count: usize,
}

/// Clear-before-read dirty flag (`RejoinProtocol` variant (b)).
///
/// `run_if_dirty` clears the flag *before* running the rebuild, so a
/// writer that lands during the rebuild re-marks and the change is
/// never lost. The variant-(a) order (clear after the rebuild) is what
/// `formal/RejoinProtocol.cfg` catches; the guard test
/// `rejoin_lost_mark_guard` in this module fails if the order flips.
pub struct DirtyFlag(AtomicBool);

impl DirtyFlag {
    pub fn new(dirty: bool) -> Self {
        Self(AtomicBool::new(dirty))
    }

    pub fn mark(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_dirty(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Run `rebuild` if the flag is set. Clears the flag first
    /// (clear-before-read); restores it if `rebuild` fails so the
    /// change is not silently swallowed.
    pub fn run_if_dirty<E>(&self, rebuild: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
        if !self.is_dirty() {
            return Ok(());
        }
        self.0.store(false, Ordering::Release);
        match rebuild() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.mark();
                Err(e)
            }
        }
    }
}

/// What an agent's rejoin found. Rejoin never breaks claims: it
/// re-asserts the caller's own claims and reports, never rewrites.
#[derive(Debug, Clone, PartialEq)]
pub struct RejoinReport {
    pub agent: String,
    /// Paths whose claim was re-asserted (lease refreshed or re-taken).
    pub claims_kept: Vec<String>,
    /// Paths the registry still credits to the caller but where
    /// another agent now holds a live filesystem lease. Reported, not
    /// stolen back, not released.
    pub claims_at_risk: Vec<Value>,
    pub view_epoch: u64,
}

impl RejoinReport {
    pub fn to_json(&self, board: &Board) -> Value {
        json!({
            "ok": true,
            "agent_id": self.agent,
            "claims_kept": self.claims_kept,
            "claims_at_risk": self.claims_at_risk,
            "view_epoch": self.view_epoch,
            "board": board_json(board),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub claims: Vec<Claim>,
    pub agents: Vec<AgentInfo>,
    pub audit: VecDeque<AuditEvent>,
    /// The published board (derived view + its generation).
    #[serde(default)]
    pub board: Board,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            claims: Vec::new(),
            agents: Vec::new(),
            audit: VecDeque::new(),
            board: Board::default(),
        }
    }
}

impl State {
    /// Register (or refresh) an agent. Returns the stored record.
    /// `kind` is a free-form label (e.g. `claude`, `codex`, `kimi`).
    pub fn register(&mut self, name: &str, kind: &str, now: f64) -> AgentInfo {
        let kind = if kind.is_empty() { "other" } else { kind };
        match self.agents.iter_mut().find(|a| a.name == name) {
            Some(a) => {
                a.kind = kind.to_string();
                a.last_seen = now;
                a.clone()
            }
            None => {
                let a = AgentInfo {
                    name: name.to_string(),
                    kind: kind.to_string(),
                    last_seen: now,
                };
                self.agents.push(a.clone());
                self.push_audit(AuditEvent {
                    ts: now,
                    agent: name.to_string(),
                    action: "register".to_string(),
                    path: None,
                    purpose: Some(kind.to_string()),
                });
                a
            }
        }
    }

    /// Refresh an agent's presence clock.
    pub fn touch(&mut self, agent: &str, now: f64) {
        if let Some(a) = self.agents.iter_mut().find(|a| a.name == agent) {
            a.last_seen = now;
        }
    }

    /// Arbitration only: does `agent`'s batch of `(path, intent)`
    /// requests clear every other agent's recorded intent?
    /// (`Grantable` in `formal/PresenceClaims.tla`.) The first
    /// conflicting holder in request order is reported.
    pub fn check_claim(
        &self,
        agent: &str,
        files: &[(String, ClaimIntent)],
        now: f64,
        ttl: u64,
    ) -> Result<(), Conflict> {
        for (raw, intent) in files {
            let path = posix(raw);
            for c in self
                .claims
                .iter()
                .filter(|c| c.path == path && c.agent != agent)
            {
                if intents_conflict(c.intent, *intent) {
                    let holder_stale = self
                        .agents
                        .iter()
                        .find(|a| a.name == c.agent)
                        .map(|a| is_stale(a.last_seen, now, ttl))
                        .unwrap_or(true);
                    return Err(Conflict {
                        holder: c.agent.clone(),
                        path,
                        claimed_at: c.claimed_at,
                        purpose: c.purpose.clone(),
                        holder_stale,
                        intent: c.intent,
                    });
                }
            }
        }
        Ok(())
    }

    /// Apply a batch that already cleared [`Self::check_claim`].
    /// A re-claim of the agent's own path refreshes `claimed_at` and
    /// `purpose` and keeps the *strongest* intent (a later weaker claim
    /// never hides a stronger one — `PresenceClaims_LastWins.cfg`).
    pub fn apply_claim(
        &mut self,
        agent: &str,
        files: &[(String, ClaimIntent, Option<String>)],
        now: f64,
    ) -> Vec<Claim> {
        let mut granted = Vec::new();
        for (raw, intent, purpose) in files {
            let path = posix(raw);
            match self
                .claims
                .iter_mut()
                .find(|c| c.path == path && c.agent == agent)
            {
                Some(c) => {
                    c.intent = c.intent.strongest(*intent);
                    c.claimed_at = now;
                    c.purpose = purpose.clone();
                    granted.push(c.clone());
                }
                None => {
                    let c = Claim {
                        path: path.clone(),
                        agent: agent.to_string(),
                        intent: *intent,
                        claimed_at: now,
                        purpose: purpose.clone(),
                    };
                    self.claims.push(c.clone());
                    granted.push(c);
                }
            }
            self.push_audit(AuditEvent {
                ts: now,
                agent: agent.to_string(),
                action: "claim".to_string(),
                path: Some(path),
                purpose: purpose.clone(),
            });
        }
        self.touch(agent, now);
        granted
    }

    /// Claim files for `agent` (all-or-nothing). `files` is
    /// `(path, purpose)` in request order; `intent` is the claim kind
    /// for the whole batch. Arbitration then application — one call
    /// for the pure-registry tests; the tool layer wraps it with the
    /// filesystem lease step in between.
    pub fn claim(
        &mut self,
        agent: &str,
        files: &[(String, Option<String>)],
        intent: ClaimIntent,
        now: f64,
        ttl: u64,
    ) -> ClaimOutcome {
        let checks: Vec<(String, ClaimIntent)> =
            files.iter().map(|(p, _)| (p.clone(), intent)).collect();
        if let Err(c) = self.check_claim(agent, &checks, now, ttl) {
            return ClaimOutcome::Conflict(c);
        }
        let batch: Vec<(String, ClaimIntent, Option<String>)> = files
            .iter()
            .map(|(p, purpose)| (p.clone(), intent, purpose.clone()))
            .collect();
        ClaimOutcome::Granted(self.apply_claim(agent, &batch, now))
    }

    /// Release files held by `agent`. Returns the paths actually
    /// released (files held by someone else are left alone).
    pub fn release(&mut self, agent: &str, files: &[String], now: f64) -> Vec<String> {
        let mut released = Vec::new();
        for path in files {
            let path = posix(path);
            let before = self.claims.len();
            self.claims
                .retain(|c| !(c.path == path && c.agent == agent));
            if self.claims.len() < before {
                released.push(path.clone());
                self.push_audit(AuditEvent {
                    ts: now,
                    agent: agent.to_string(),
                    action: "release".to_string(),
                    path: Some(path),
                    purpose: None,
                });
            }
        }
        self.touch(agent, now);
        released
    }

    /// Claims with staleness reporting. Filter by agent when given.
    pub fn claim_views(&self, agent: Option<&str>, now: f64, ttl: u64) -> Vec<Value> {
        self.claims
            .iter()
            .filter(|c| agent.is_none_or(|a| c.agent == a))
            .map(|c| {
                let held_by_stale = self
                    .agents
                    .iter()
                    .find(|a| a.name == c.agent)
                    .map(|a| is_stale(a.last_seen, now, ttl))
                    .unwrap_or(true);
                json!({
                    "path": c.path,
                    "agent": c.agent,
                    "intent": c.intent.as_str(),
                    "claimed_at": c.claimed_at,
                    "purpose": c.purpose,
                    "held_by_stale": held_by_stale,
                })
            })
            .collect()
    }

    /// All known agents with a `stale` flag (stale agents are listed,
    /// never dropped).
    pub fn agent_views(&self, now: f64, ttl: u64) -> Vec<Value> {
        self.agents
            .iter()
            .map(|a| {
                json!({
                    "agent_id": a.name,
                    "name": a.name,
                    "kind": a.kind,
                    "last_seen": a.last_seen,
                    "stale": is_stale(a.last_seen, now, ttl),
                    "claims_count": self.claims.iter().filter(|c| c.agent == a.name).count(),
                })
            })
            .collect()
    }

    /// One agent's view: identity and claims.
    pub fn who(&self, agent: &str, now: f64, ttl: u64) -> Option<Value> {
        let a = self.agents.iter().find(|a| a.name == agent)?;
        Some(json!({
            "agent_id": a.name,
            "name": a.name,
            "kind": a.kind,
            "last_seen": a.last_seen,
            "stale": is_stale(a.last_seen, now, ttl),
            "claims": self.claim_views(Some(agent), now, ttl)
        }))
    }

    // ─── board + rejoin protocol ────────────────────────────────────

    /// The derived view of the current records. Pure.
    pub fn compute_board(&self) -> Board {
        let mut by_path: Vec<BoardHolding> = self
            .claims
            .iter()
            .map(|c| BoardHolding {
                path: c.path.clone(),
                agent: c.agent.clone(),
                intent: c.intent,
            })
            .collect();
        by_path.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.agent.cmp(&b.agent)));
        Board {
            epoch: self.board.epoch,
            by_path,
            claims_count: self.claims.len(),
            agents_count: self.agents.len(),
        }
    }

    /// Publish a freshly computed board: one atomic pair of
    /// `(view, epoch)` — the epoch advances with the content so a
    /// reader of the pair never mixes generations (RejoinProtocol I7).
    pub fn publish_board(&mut self) -> u64 {
        let mut board = self.compute_board();
        board.epoch = self.board.epoch + 1;
        self.board = board;
        self.board.epoch
    }

    /// The rejoin protocol (variant (b) — clear-before-read): rebuild
    /// the published board from the records. `dirty` is marked by every
    /// mutation; clearing it before reading the records means a writer
    /// landing during the rebuild re-marks and the next pass picks the
    /// change up (NoLostUpdate). When nothing is dirty this is a no-op
    /// and the already-published board stays (Convergence).
    pub fn rejoin_board(&mut self, dirty: &DirtyFlag) -> u64 {
        let _ = dirty.run_if_dirty(|| {
            self.publish_board();
            Ok::<(), ()>(())
        });
        self.board.epoch
    }

    /// Agent-facing rejoin at the registry level: refresh presence and
    /// report the caller's claims in scope. Lease re-assertion is the
    /// tool layer's job (it owns the filesystem lease); this never
    /// touches claims — own or foreign.
    pub fn rejoin_report(
        &mut self,
        agent: &str,
        files: Option<&[String]>,
        now: f64,
    ) -> Vec<String> {
        self.touch(agent, now);
        let scope = self
            .claims
            .iter()
            .filter(|c| c.agent == agent)
            .filter(|c| files.is_none_or(|fs| fs.iter().any(|f| posix(f) == c.path)))
            .map(|c| c.path.clone())
            .collect();
        self.push_audit(AuditEvent {
            ts: now,
            agent: agent.to_string(),
            action: "rejoin".to_string(),
            path: None,
            purpose: None,
        });
        scope
    }

    // ─── audit ──────────────────────────────────────────────────────

    /// Most recent `limit` audit events with `ts >= since`, oldest
    /// first (the log is read as a story, not a tail).
    pub fn audit_events(&self, limit: usize, since: Option<f64>) -> Vec<Value> {
        let mut events: Vec<&AuditEvent> = self
            .audit
            .iter()
            .filter(|e| since.is_none_or(|s| e.ts >= s))
            .collect();
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
        events.iter().map(|e| audit_json(e)).collect()
    }

    fn push_audit(&mut self, event: AuditEvent) {
        self.audit.push_back(event);
        while self.audit.len() > AUDIT_CAP {
            self.audit.pop_front();
        }
    }
}

pub fn board_json(b: &Board) -> Value {
    json!({
        "epoch": b.epoch,
        "by_path": b.by_path.iter().map(|h| json!({
            "path": h.path,
            "agent": h.agent,
            "intent": h.intent.as_str(),
        })).collect::<Vec<_>>(),
        "claims_count": b.claims_count,
        "agents_count": b.agents_count,
    })
}

fn audit_json(e: &AuditEvent) -> Value {
    json!({
        "ts": e.ts,
        "agent": e.agent,
        "action": e.action,
        "path": e.path,
        "purpose": e.purpose,
    })
}

// ─── persistence ────────────────────────────────────────────────────

pub fn state_dir(root: &Path) -> PathBuf {
    root.join(STATE_DIR)
}

pub fn state_path(root: &Path) -> PathBuf {
    state_dir(root).join(STATE_FILE)
}

/// Load state from disk. A missing file is a fresh state; a corrupt
/// file or a foreign format version is an error (never silently wiped).
/// Recovery for a version mismatch is to move `.zoanthrope/` aside and
/// re-claim — the state is workspace coordination data, regenerable by
/// the swarm.
pub fn load(root: &Path) -> Result<State, String> {
    let path = state_path(root);
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let value: Value = serde_json::from_str(&raw)
                .map_err(|e| format!("corrupt state at {}: {e}", path.display()))?;
            let version = value.get("version").and_then(Value::as_u64).unwrap_or(0);
            if version != u64::from(STATE_VERSION) {
                return Err(format!(
                    "state at {} is version {version}; this build speaks {}. \
                     Move .zoanthrope/ aside to start fresh",
                    path.display(),
                    STATE_VERSION
                ));
            }
            serde_json::from_value(value)
                .map_err(|e| format!("corrupt state at {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Write state atomically: temp file in the same directory, then rename.
/// The rename is the generation publish — a reader sees the old whole
/// state or the new whole state, never a mix.
pub fn save(root: &Path, state: &State) -> Result<(), String> {
    let dir = state_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let path = state_path(root);
    let tmp = dir.join(format!("{STATE_FILE}.{}.tmp", std::process::id()));
    let raw = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, raw).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename into {}: {e}", path.display()))
}

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Cheap jitter in `[0, base)` so waiters queued on the same
/// backoff step don't retry in lockstep. `RandomState` seeds from a
/// thread-local counter — no syscall, no dependency.
fn jitter(base: Duration) -> Duration {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    let frac = RandomState::new().hash_one(()) % 1024;
    let nanos = u64::try_from(base.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(nanos * frac / 1024)
}

fn acquire_lock(root: &Path) -> Result<LockGuard, String> {
    let dir = state_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let path = dir.join(LOCK_FILE);
    // Exponential backoff instead of a fixed 20 ms poll: the deadline
    // and stale-lock rules are the old ones (250 polls x 20 ms), only
    // how eagerly we re-try changed.
    let deadline = Instant::now() + LOCK_TIMEOUT;
    let mut backoff = LOCK_BACKOFF_MIN;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(LockGuard { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Break a lock left behind by a crashed process.
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .map(|age| age > Duration::from_secs(LOCK_STALE_SECS))
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(jitter(backoff).min(remaining));
                backoff = (backoff * 2).min(LOCK_BACKOFF_MAX);
            }
            Err(e) => return Err(format!("lock {}: {e}", path.display())),
        }
    }
    Err(format!("could not lock {}: timed out", path.display()))
}

/// Read-only access to the on-disk state.
pub fn read<R>(root: &Path, f: impl FnOnce(&State) -> R) -> Result<R, String> {
    let state = load(root)?;
    Ok(f(&state))
}

/// Locked read-modify-write of the on-disk state. The state is saved
/// only when `f` returns `Ok`; every save publishes a new board
/// generation (`RejoinProtocol`: the derived view is rebuilt
/// clear-before-read before it is swapped in).
pub fn update<R>(
    root: &Path,
    f: impl FnOnce(&mut State) -> Result<R, String>,
) -> Result<R, String> {
    let _lock = acquire_lock(root)?;
    let mut state = load(root)?;
    let out = f(&mut state)?;
    let dirty = DirtyFlag::new(true);
    state.rejoin_board(&dirty);
    save(root, &state)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zoanthrope-state-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn exclusive_claim_grants_and_conflicts_with_shape() {
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        let out = s.claim(
            "alpha",
            &[("a.rs".into(), Some("fix bug".into()))],
            ClaimIntent::Edit,
            now,
            900,
        );
        let granted = match out {
            ClaimOutcome::Granted(g) => g,
            ClaimOutcome::Conflict(_) => panic!("expected grant"),
        };
        assert_eq!(granted.len(), 1);
        assert_eq!(granted[0].path, "a.rs");
        assert_eq!(granted[0].purpose.as_deref(), Some("fix bug"));
        assert_eq!(granted[0].intent, ClaimIntent::Edit);

        s.register("beta", "codex", now + 1.0);
        let out = s.claim(
            "beta",
            &[("a.rs".into(), Some("refactor".into()))],
            ClaimIntent::Edit,
            now + 1.0,
            900,
        );
        let conflict = match out {
            ClaimOutcome::Conflict(c) => c,
            ClaimOutcome::Granted(_) => panic!("expected conflict"),
        };
        assert_eq!(conflict.holder, "alpha");
        assert_eq!(conflict.path, "a.rs");
        assert_eq!(conflict.claimed_at, now);
        assert_eq!(conflict.purpose.as_deref(), Some("fix bug"));
        assert!(!conflict.holder_stale);
        assert_eq!(conflict.intent, ClaimIntent::Edit);

        // The conflict JSON is the product's face — exact keys.
        let v = conflict.to_json();
        assert_eq!(v["ok"], false);
        assert_eq!(v["holder"], "alpha");
        assert_eq!(v["path"], "a.rs");
        assert_eq!(v["claimed_at"], now);
        assert_eq!(v["purpose"], "fix bug");
        assert_eq!(v["holder_stale"], false);
        assert_eq!(v["intent"], "edit");
        assert_eq!(v.as_object().unwrap().len(), 7);

        // Nothing was granted to beta.
        assert_eq!(s.claims.len(), 1);
        assert_eq!(s.claims[0].agent, "alpha");
    }

    #[test]
    fn claim_is_all_or_nothing_per_call() {
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.claim(
            "alpha",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            now,
            900,
        );
        s.register("beta", "codex", now);
        let out = s.claim(
            "beta",
            &[
                ("b.rs".into(), None),
                ("a.rs".into(), None),
                ("c.rs".into(), None),
            ],
            ClaimIntent::Edit,
            now,
            900,
        );
        match out {
            ClaimOutcome::Conflict(c) => assert_eq!(c.path, "a.rs"),
            ClaimOutcome::Granted(_) => panic!("expected conflict"),
        }
        assert_eq!(s.claims.len(), 1, "no partial grant");
    }

    #[test]
    fn reclaim_own_file_refreshes_and_stays_granted() {
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.claim(
            "alpha",
            &[("a.rs".into(), Some("first".into()))],
            ClaimIntent::Edit,
            now,
            900,
        );
        let out = s.claim(
            "alpha",
            &[("a.rs".into(), Some("second".into()))],
            ClaimIntent::Edit,
            now + 5.0,
            900,
        );
        let granted = match out {
            ClaimOutcome::Granted(g) => g,
            ClaimOutcome::Conflict(_) => panic!("re-claim of own file must grant"),
        };
        assert_eq!(granted[0].purpose.as_deref(), Some("second"));
        assert_eq!(granted[0].claimed_at, now + 5.0);
        assert_eq!(s.claims.len(), 1);
    }

    #[test]
    fn release_frees_the_file_for_others() {
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.register("beta", "codex", now);
        s.claim(
            "alpha",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            now,
            900,
        );
        let released = s.release("alpha", &["a.rs".into()], now + 1.0);
        assert_eq!(released, vec!["a.rs".to_string()]);
        // Releasing again is a no-op.
        assert!(s.release("alpha", &["a.rs".into()], now + 2.0).is_empty());
        // Beta can claim now.
        let out = s.claim(
            "beta",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            now + 3.0,
            900,
        );
        assert!(matches!(out, ClaimOutcome::Granted(_)));
    }

    #[test]
    fn stale_reporting_names_staleness_but_never_releases() {
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.claim(
            "alpha",
            &[("a.rs".into(), Some("deep work".into()))],
            ClaimIntent::Edit,
            now,
            900,
        );

        let later = now + 901.0; // past the 900s TTL
        let claims = s.claim_views(None, later, 900);
        assert_eq!(claims[0]["held_by_stale"], true);
        assert_eq!(claims[0]["agent"], "alpha");
        let agents = s.agent_views(later, 900);
        assert_eq!(agents[0]["stale"], true);

        // A stale holder still conflicts — auto-release is a policy we
        // did not make. The conflict just says the holder is stale.
        s.register("beta", "codex", later);
        let out = s.claim(
            "beta",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            later,
            900,
        );
        match out {
            ClaimOutcome::Conflict(c) => {
                assert_eq!(c.holder, "alpha");
                assert!(c.holder_stale);
            }
            ClaimOutcome::Granted(_) => panic!("stale claims still block"),
        }
        assert_eq!(s.claims.len(), 1, "no auto-release");
    }

    #[test]
    fn stale_flag_is_fresh_for_live_agents() {
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.claim(
            "alpha",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            now,
            900,
        );
        // Touch keeps the agent live.
        s.touch("alpha", now + 899.0);
        let claims = s.claim_views(None, now + 900.0, 900);
        assert_eq!(claims[0]["held_by_stale"], false);
    }

    #[test]
    fn ttl_boundary_is_strictly_older_than() {
        assert!(!is_stale(1_000.0, 1_900.0, 900), "age == ttl is live");
        assert!(is_stale(1_000.0, 1_900.1, 900), "age > ttl is stale");
        assert!(!is_stale(1_000.0, 1_000.0, 900));
        assert!(
            is_stale(0.0, 1.0, 0),
            "ttl 0 means stale as soon as time passes"
        );
    }

    #[test]
    fn ttl_parsing_override() {
        // Pure parse so parallel tests never race on process env.
        assert_eq!(parse_ttl(None), DEFAULT_TTL_SECS);
        assert_eq!(parse_ttl(Some("42")), 42);
        assert_eq!(parse_ttl(Some(" 42 ")), 42);
        assert_eq!(parse_ttl(Some("not-a-number")), DEFAULT_TTL_SECS);
        assert_eq!(parse_ttl(Some("")), DEFAULT_TTL_SECS);
    }

    #[test]
    fn audit_ring_caps_and_keeps_order() {
        let mut s = State::default();
        for i in 0..(AUDIT_CAP + 25) {
            s.push_audit(AuditEvent {
                ts: i as f64,
                agent: "alpha".into(),
                action: "claim".into(),
                path: Some(format!("f{i}.rs")),
                purpose: None,
            });
        }
        assert_eq!(s.audit.len(), AUDIT_CAP);
        // Oldest 25 fell off; remaining window starts at ts 25.
        assert_eq!(s.audit.front().unwrap().ts, 25.0);
        assert_eq!(s.audit.back().unwrap().ts, (AUDIT_CAP + 24) as f64);

        // Reads are oldest-first and honor limit/since.
        let all = s.audit_events(10_000, None);
        assert_eq!(all.len(), AUDIT_CAP);
        assert_eq!(all.first().unwrap()["ts"], 25.0);
        assert_eq!(all.last().unwrap()["ts"], (AUDIT_CAP + 24) as f64);
        let recent = s.audit_events(10, None);
        assert_eq!(recent.len(), 10);
        assert_eq!(recent.first().unwrap()["ts"], (AUDIT_CAP + 15) as f64);
        let since = s.audit_events(10_000, Some((AUDIT_CAP + 20) as f64));
        assert_eq!(since.len(), 5);
        assert_eq!(since.first().unwrap()["ts"], (AUDIT_CAP + 20) as f64);
    }

    #[test]
    fn save_load_roundtrip() {
        let root = temp_root("roundtrip");
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.claim(
            "alpha",
            &[("src/a.rs".into(), Some("p".into()))],
            ClaimIntent::Edit,
            now,
            900,
        );
        s.publish_board();
        save(&root, &s).unwrap();
        let loaded = load(&root).unwrap();
        assert_eq!(loaded.claims, s.claims);
        assert_eq!(loaded.agents, s.agents);
        assert_eq!(loaded.audit, s.audit);
        assert_eq!(loaded.board, s.board);
        assert_eq!(loaded.version, STATE_VERSION);

        // Missing file is a fresh state, not an error.
        let empty = temp_root("missing");
        assert_eq!(load(&empty).unwrap(), State::default());
    }

    #[test]
    fn foreign_state_version_is_refused() {
        let root = temp_root("version");
        let s = State {
            version: 1,
            ..Default::default()
        };
        save(&root, &s).unwrap();
        let err = load(&root).unwrap_err();
        assert!(err.contains("version 1"), "got: {err}");
    }

    #[test]
    fn update_is_persisted_and_reads_see_it() {
        let root = temp_root("update");
        let out = update(&root, |s| {
            s.register("alpha", "claude", 1_000.0);
            Ok(s.claim(
                "alpha",
                &[("a.rs".into(), None)],
                ClaimIntent::Edit,
                1_000.0,
                900,
            ))
        })
        .unwrap();
        assert!(matches!(out, ClaimOutcome::Granted(_)));
        let claims = read(&root, |s| s.claim_views(None, 1_000.0, 900)).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0]["path"], "a.rs");
        // Every committed update publishes a fresh board generation.
        let epoch = read(&root, |s| s.board.epoch).unwrap();
        assert_eq!(epoch, 1);
    }

    // ─── PresenceClaims semantics ───────────────────────────────────

    #[test]
    fn readers_cohold_but_editors_exclude() {
        // Grantable (PresenceClaims): read conflicts with edit, read
        // co-exists with read, edit conflicts with everything.
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.register("beta", "codex", now);
        s.register("gamma", "kimi", now);

        assert!(matches!(
            s.claim(
                "alpha",
                &[("a.rs".into(), None)],
                ClaimIntent::Read,
                now,
                900
            ),
            ClaimOutcome::Granted(_)
        ));
        // A second reader is not in the way of the first.
        assert!(matches!(
            s.claim(
                "beta",
                &[("a.rs".into(), None)],
                ClaimIntent::Read,
                now,
                900
            ),
            ClaimOutcome::Granted(_)
        ));
        // An editor conflicts with both readers (reported: first holder).
        match s.claim(
            "gamma",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            now,
            900,
        ) {
            ClaimOutcome::Conflict(c) => {
                assert_eq!(c.holder, "alpha");
                assert_eq!(c.intent, ClaimIntent::Read);
            }
            ClaimOutcome::Granted(_) => panic!("edit must conflict with readers"),
        }
        // And a reader conflicts with an editor.
        s.release("alpha", &["a.rs".into()], now);
        s.release("beta", &["a.rs".into()], now);
        assert!(matches!(
            s.claim(
                "gamma",
                &[("a.rs".into(), None)],
                ClaimIntent::Edit,
                now,
                900
            ),
            ClaimOutcome::Granted(_)
        ));
        match s.claim(
            "alpha",
            &[("a.rs".into(), None)],
            ClaimIntent::Read,
            now,
            900,
        ) {
            ClaimOutcome::Conflict(c) => {
                assert_eq!(c.holder, "gamma");
                assert_eq!(c.intent, ClaimIntent::Edit);
            }
            ClaimOutcome::Granted(_) => panic!("read must conflict with edit"),
        }
    }

    #[test]
    fn strongest_intent_beats_the_last_claim() {
        // PresenceClaims_LastWins.cfg is the counterexample: if the
        // recorded intent were the LAST claim, alpha's read re-claim
        // would hide her edit and let beta's read in beside it —
        // NoConflictingHolders violated. The recorded intent is the
        // strongest held, so the read re-claim does not downgrade.
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.register("beta", "codex", now);
        s.claim(
            "alpha",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            now,
            900,
        );
        // The later claim is weaker — it must not win.
        s.claim(
            "alpha",
            &[("a.rs".into(), None)],
            ClaimIntent::Read,
            now + 1.0,
            900,
        );
        assert_eq!(s.claims[0].intent, ClaimIntent::Edit, "no downgrade");
        match s.claim(
            "beta",
            &[("a.rs".into(), None)],
            ClaimIntent::Read,
            now + 2.0,
            900,
        ) {
            ClaimOutcome::Conflict(c) => {
                assert_eq!(c.holder, "alpha");
                assert_eq!(c.intent, ClaimIntent::Edit);
            }
            ClaimOutcome::Granted(_) => panic!("last claim must not hide the strongest"),
        }
        // A true release (then re-claim) is what downgrades.
        s.release("alpha", &["a.rs".into()], now + 3.0);
        assert!(matches!(
            s.claim(
                "alpha",
                &[("a.rs".into(), None)],
                ClaimIntent::Read,
                now + 4.0,
                900
            ),
            ClaimOutcome::Granted(_)
        ));
        assert_eq!(s.claims[0].intent, ClaimIntent::Read);
    }

    #[test]
    fn intent_parsing_is_closed() {
        assert_eq!(ClaimIntent::parse("read").unwrap(), ClaimIntent::Read);
        assert_eq!(ClaimIntent::parse("Edit").unwrap(), ClaimIntent::Edit);
        assert!(ClaimIntent::parse("watch").is_err());
        assert!(ClaimIntent::parse("").is_err());
        assert_eq!(
            ClaimIntent::Edit.strongest(ClaimIntent::Read),
            ClaimIntent::Edit
        );
    }

    // ─── rejoin protocol ────────────────────────────────────────────

    #[test]
    fn rejoin_board_is_convergent_when_quiescent() {
        // Convergence: when no rejoin is in flight and the flag is
        // clear, the published board equals what the records compute.
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.claim(
            "alpha",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            now,
            900,
        );
        let dirty = DirtyFlag::new(true);
        let epoch = s.rejoin_board(&dirty);
        assert_eq!(epoch, 1);
        assert!(!dirty.is_dirty());
        assert_eq!(s.board, s.compute_board());
        assert_eq!(s.board.epoch, 1);
        assert_eq!(s.board.by_path.len(), 1);
        assert_eq!(s.board.claims_count, 1);
    }

    #[test]
    fn rejoin_lost_mark_guard() {
        // NoLostUpdate: a writer landing during the rebuild re-marks.
        // Clear-before-read keeps the mark; the pre-fix clear-after
        // order would swallow it (formal/RejoinProtocol.cfg).
        let mut s = State::default();
        let dirty = DirtyFlag::new(true);
        let mut landed = false;
        let _ = dirty.run_if_dirty(|| {
            // The lockless writer: lands between the clear and the
            // publish, and marks.
            dirty.mark();
            landed = true;
            s.publish_board();
            Ok::<(), ()>(())
        });
        assert!(landed);
        assert!(
            dirty.is_dirty(),
            "a mark landing mid-rebuild must survive (clear-before-read)"
        );
        // The next pass picks the writer's change up.
        s.claim(
            "beta",
            &[("b.rs".into(), None)],
            ClaimIntent::Edit,
            1.0,
            900,
        );
        s.rejoin_board(&dirty);
        assert_eq!(s.board.claims_count, 1, "board reflects the late writer");
    }

    #[test]
    fn rejoin_clear_after_would_lose_the_mark() {
        // The negative control for the guard above: with the pre-fix
        // order (rebuild, then clear) the writer's mark is swallowed
        // and the board settles stale with a clear flag. Built by hand
        // so the two orders stay visibly different; this is the bug
        // `formal/RejoinProtocol.cfg` catches, kept so the order in
        // `run_if_dirty` cannot be swapped silently.
        let flag = AtomicBool::new(true);
        // The lockless writer lands mid-rebuild and marks:
        flag.store(true, Ordering::Release);
        // ...and the trailing clear swallows it:
        flag.store(false, Ordering::Release);
        assert!(
            !flag.load(Ordering::Acquire),
            "under clear-after the mark is gone"
        );
    }

    #[test]
    fn board_epoch_advances_with_each_publish() {
        // I7 reader consistency: (board, epoch) is one value; the
        // epoch moves with the content, never separately.
        let mut s = State::default();
        let dirty = DirtyFlag::new(true);
        s.rejoin_board(&dirty);
        assert_eq!(s.board.epoch, 1);
        let before = s.board.clone();
        s.rejoin_board(&DirtyFlag::new(false));
        assert_eq!(s.board, before, "a clean rejoin publishes nothing");
        dirty.mark();
        s.rejoin_board(&dirty);
        assert_eq!(s.board.epoch, 2);
        assert_eq!(s.board, s.compute_board(), "pair is consistent");
    }

    #[test]
    fn rejoin_never_breaks_claims() {
        // The agent-facing rejoin reports the caller's claims and
        // touches nothing else — no release, no steal, no rewrite.
        let mut s = State::default();
        let now = 1_000.0;
        s.register("alpha", "claude", now);
        s.register("beta", "codex", now);
        s.claim(
            "alpha",
            &[("a.rs".into(), None)],
            ClaimIntent::Edit,
            now,
            900,
        );
        s.claim(
            "beta",
            &[("b.rs".into(), None)],
            ClaimIntent::Edit,
            now,
            900,
        );
        let before = s.claims.clone();

        let scope = s.rejoin_report("alpha", None, now + 5.0);
        assert_eq!(scope, vec!["a.rs".to_string()]);
        let scope = s.rejoin_report("alpha", Some(&["b.rs".into()]), now + 6.0);
        assert!(scope.is_empty(), "scoped rejoin only covers own claims");
        assert_eq!(s.claims, before, "rejoin breaks no claims");
        assert_eq!(
            s.agents
                .iter()
                .find(|a| a.name == "alpha")
                .unwrap()
                .last_seen,
            now + 6.0,
            "presence is refreshed"
        );
    }

    #[test]
    fn discover_root_walks_up_to_git() {
        let root = temp_root("discover");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(discover_root(&nested), root);
    }
}
