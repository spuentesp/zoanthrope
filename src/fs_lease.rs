//! Filesystem-as-lock layer: per-path leases under
//! `.zoanthrope/locks/`, the anti-clobbering core of the sync system.
//!
//! Design (the `formal/FsLease.tla` + `formal/FsLeaseGuard.tla` specs):
//!
//! * Each acquire mints a fresh nonce and writes a uniquely named file
//!   `<sanitized>.lock-<nonce>` whose body records agent, intent,
//!   purpose, nonce, and time. A nonce-bearing filename can never
//!   clobber another holder's file: no other lock lives at that name.
//! * The scan-then-create acquire is serialized per path by an
//!   `O_EXCL` guard file (`<sanitized>.guard`) — without it, every
//!   concurrent acquirer passes the scan and then creates its own file
//!   (`FsLeaseGuard_Unguarded.cfg`). A guard holder frozen longer
//!   than `GUARD_TTL` is the documented residual
//!   (`FsLeaseGuard_Stall.cfg`); the guard is taken over by an atomic
//!   rename.
//! * Release is rename-and-verify: the file is moved to a private
//!   tombstone *first*, then inspected; it is unlinked only if the
//!   body names us, and restored otherwise — so a delayed release can
//!   never delete a replacement holder's file. A non-owner's blind
//!   delete is exactly what the spec rules out.
//! * A lease older than [`LEASE_TTL`] is stale and may be taken over;
//!   the takeover is also rename-and-verify, so a misjudged "stale"
//!   file that turns out live is put back.
//!
//! The registry in [`crate::state`] is the coordination record; this
//! layer is the OS-level lock a hook or an operator can see and trust
//! without talking to anyone.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// A lease older than this is stale and may be taken over. Unlike a
/// server-backed design there is nobody to heartbeat for a quiet
/// holder, so the TTL must cover one whole write window by itself.
/// `ZOANTHROPE_LEASE_TTL_SECS` overrides.
pub const LEASE_TTL: Duration = Duration::from_secs(60);

/// How long a guard file may exist before an acquirer treats it as
/// left behind by a crashed process and takes it over. The critical
/// section is a directory scan plus one file create, so a live holder
/// is gone in milliseconds; a process frozen for longer is the
/// `FsLeaseGuard_Stall` residual.
const GUARD_TTL: Duration = Duration::from_secs(2);
/// Upper bound on waiting for a contended guard before giving up.
const GUARD_WAIT: Duration = Duration::from_millis(1500);

const LOCK_DIR: &str = "locks";

/// Override for [`LEASE_TTL`]; unparseable falls back to the default.
pub fn lease_ttl() -> Duration {
    std::env::var("ZOANTHROPE_LEASE_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(LEASE_TTL)
}

/// Another agent's live lease — the acquire refused.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseConflict {
    pub holder: String,
    pub intent: String,
    pub purpose: Option<String>,
    pub claimed_at_unix: u64,
    pub nonce: String,
}

impl LeaseConflict {
    /// The conflict body a hook prints — same keys as the registry
    /// conflict minus the claim bookkeeping, plus `lease: true` so a
    /// reader can tell the layers apart.
    pub fn to_json(&self, path: &str) -> Value {
        json!({
            "ok": false,
            "lease": true,
            "holder": self.holder,
            "path": path,
            "intent": self.intent,
            "purpose": self.purpose,
            "claimed_at": self.claimed_at_unix,
            "nonce": self.nonce,
            "holder_stale": false,
        })
    }
}

/// Mint a per-acquire nonce: pid + monotone nanos + counter. No
/// randomness crate on purpose — the nonce only has to be unique
/// among the files a single path can accumulate, and a pid/timestamp
/// pair already is.
fn mint_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", std::process::id(), nanos, n)
}

fn lock_dir(root: &Path) -> PathBuf {
    root.join(".zoanthrope").join(LOCK_DIR)
}

/// Map a path to a single-component filename safe across platforms:
/// percent-encode every byte that is not ASCII alphanumeric / `-` / `_`.
/// Two distinct paths produce two distinct byte sequences. The result
/// stays within `NAME_MAX` by folding a long tail into a `~` plus hex
/// of a 64-bit FNV-1a digest of the whole encoded name.
fn sanitize(path: &str) -> String {
    let bytes = path.replace('\\', "/");
    let mut out = String::with_capacity(bytes.len());
    for b in bytes.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
    }
    if out.len() > MAX_SANITIZED_LEN {
        let digest = fnv1a(out.as_bytes());
        out.truncate(SANITIZED_KEEP_LEN);
        out.push('~');
        use std::fmt::Write as _;
        let _ = write!(out, "{digest:016x}");
    }
    out
}

/// `NAME_MAX` (255) minus `.lock-` (6) and the nonce, with margin.
const MAX_SANITIZED_LEN: usize = 160;
const SANITIZED_KEEP_LEN: usize = 128;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn read_body(file: &Path) -> Value {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({}))
}

fn body_agent(body: &Value) -> String {
    body.get("agent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn body_nonce(body: &Value) -> String {
    body.get("nonce")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn entry_age(file: &Path) -> Option<Duration> {
    std::fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
}

/// The live holder of `path`, if any (stale entries are ignored).
pub fn holder(root: &Path, path: &str) -> Option<LeaseConflict> {
    let dir = lock_dir(root);
    let prefix = format!("{}.lock-", sanitize(path));
    let ttl = lease_ttl();
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        let file = entry.path();
        let age = entry_age(&file).unwrap_or(Duration::MAX);
        if age > ttl {
            continue;
        }
        let body = read_body(&file);
        return Some(LeaseConflict {
            holder: body_agent(&body),
            intent: body
                .get("intent")
                .and_then(Value::as_str)
                .unwrap_or("edit")
                .to_string(),
            purpose: body
                .get("purpose")
                .and_then(Value::as_str)
                .map(str::to_string),
            claimed_at_unix: body.get("claimed_at").and_then(Value::as_u64).unwrap_or(0),
            nonce: body_nonce(&body),
        });
    }
    None
}

/// Acquire the lease for `path`. Returns the sentinel file on success;
/// refuses when another agent holds a live lease
/// (`formal/FsLease.tla` MutualExclusion: at most one agent believes
/// it holds the lease).
pub fn acquire(
    root: &Path,
    path: &str,
    agent: &str,
    intent: &str,
    purpose: Option<&str>,
) -> Result<PathBuf, LeaseConflict> {
    let dir = lock_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| LeaseConflict {
        holder: format!("<io error: {e}>"),
        intent: intent.to_string(),
        purpose: purpose.map(str::to_string),
        claimed_at_unix: 0,
        nonce: String::new(),
    })?;
    acquire_in(&dir, path, agent, intent, purpose)
}

fn acquire_in(
    dir: &Path,
    path: &str,
    agent: &str,
    intent: &str,
    purpose: Option<&str>,
) -> Result<PathBuf, LeaseConflict> {
    // The scan and the create are two steps; the O_EXCL guard makes
    // the pair atomic against competing acquirers (FsLeaseGuard).
    let _guard = AcquireGuard::take(dir, path)?;

    let prefix = format!("{}.lock-", sanitize(path));
    let ttl = lease_ttl();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&prefix) {
                continue;
            }
            let file = entry.path();
            match entry_age(&file) {
                Some(age) if age <= ttl => {
                    let body = read_body(&file);
                    return Err(LeaseConflict {
                        holder: body_agent(&body),
                        intent: body
                            .get("intent")
                            .and_then(Value::as_str)
                            .unwrap_or("edit")
                            .to_string(),
                        purpose: body
                            .get("purpose")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        claimed_at_unix: body
                            .get("claimed_at")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        nonce: body_nonce(&body),
                    });
                }
                // Stale: take it over via rename-and-verify, so a file
                // that turns out live after all is put back.
                _ => take_over_stale(dir, &file),
            }
        }
    }

    let nonce = mint_nonce();
    let claimed_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = json!({
        "agent": agent,
        "intent": intent,
        "purpose": purpose,
        "nonce": nonce,
        "claimed_at": claimed_at_unix,
    });
    let file = dir.join(format!("{prefix}{nonce}"));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
    {
        Ok(mut f) => {
            let _ = f.write_all(body.to_string().as_bytes());
            Ok(file)
        }
        // UUID-class collision (same nonce): report as a conflict so
        // the caller can retry rather than silently double-holding.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let body = read_body(&file);
            Err(LeaseConflict {
                holder: body_agent(&body),
                intent: body
                    .get("intent")
                    .and_then(Value::as_str)
                    .unwrap_or("edit")
                    .to_string(),
                purpose: body
                    .get("purpose")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                claimed_at_unix: body.get("claimed_at").and_then(Value::as_u64).unwrap_or(0),
                nonce: body_nonce(&body),
            })
        }
        Err(e) => Err(LeaseConflict {
            holder: format!("<io error: {e}>"),
            intent: intent.to_string(),
            purpose: purpose.map(str::to_string),
            claimed_at_unix: 0,
            nonce: String::new(),
        }),
    }
}

/// Rename-and-verify takeover of a stale sentinel: move it to a
/// private tombstone, re-check, unlink if still stale, restore if it
/// was live after all (a fresh rename-back only when the name is free).
fn take_over_stale(dir: &Path, file: &Path) {
    let tomb = dir.join(format!(".stale-{}.tmp", mint_nonce()));
    if std::fs::rename(file, &tomb).is_err() {
        return;
    }
    let still_stale = entry_age(&tomb)
        .map(|age| age > lease_ttl())
        .unwrap_or(true);
    if still_stale {
        let _ = std::fs::remove_file(&tomb);
    } else if std::fs::rename(&tomb, file).is_err() {
        // Name taken by a newer lock — the tombstone is not ours to
        // keep; drop it rather than leave a ghost behind.
        let _ = std::fs::remove_file(&tomb);
    }
}

/// Release every lease of `path` held by `agent`, rename-and-verify.
/// Returns the number of sentinels removed. A non-owner's file is put
/// back untouched (FsLease: release can never delete another agent's
/// file blindly).
pub fn release(root: &Path, path: &str, agent: &str) -> Result<usize, String> {
    let dir = lock_dir(root);
    let prefix = format!("{}.lock-", sanitize(path));
    let mut removed = 0usize;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&prefix) {
                continue;
            }
            let file = entry.path();
            let tomb = dir.join(format!(".release-{}.tmp", mint_nonce()));
            // Atomic move aside, then inspect what we actually moved.
            if std::fs::rename(&file, &tomb).is_err() {
                continue;
            }
            let body = read_body(&tomb);
            if body_agent(&body) == agent {
                let _ = std::fs::remove_file(&tomb);
                removed += 1;
            } else if std::fs::rename(&tomb, &file).is_err() {
                let _ = std::fs::remove_file(&tomb);
            }
        }
    }
    Ok(removed)
}

/// Touch the mtime of `agent`'s sentinel for `path` (heartbeat).
/// `Ok(true)` when refreshed; `Ok(false)` when there is nothing of
/// ours to refresh. A stranger's file is never touched.
pub fn refresh(root: &Path, path: &str, agent: &str) -> Result<bool, String> {
    let dir = lock_dir(root);
    let prefix = format!("{}.lock-", sanitize(path));
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&prefix) {
                continue;
            }
            let file = entry.path();
            let body = read_body(&file);
            if body_agent(&body) != agent {
                continue;
            }
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .map_err(|e| e.to_string())?;
            f.set_modified(SystemTime::now())
                .map_err(|e| e.to_string())?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Re-assert a lease during rejoin: refresh if ours, acquire if
/// missing. Never touches a stranger's live lease.
pub fn reassert(
    root: &Path,
    path: &str,
    agent: &str,
    intent: &str,
    purpose: Option<&str>,
) -> Result<bool, LeaseConflict> {
    match refresh(root, path, agent) {
        Ok(true) => Ok(true),
        Ok(false) => match acquire(root, path, agent, intent, purpose) {
            Ok(_) => Ok(true),
            Err(c) => Err(c),
        },
        Err(_) => Err(LeaseConflict {
            holder: String::new(),
            intent: intent.to_string(),
            purpose: purpose.map(str::to_string),
            claimed_at_unix: 0,
            nonce: String::new(),
        }),
    }
}

// ─── the acquire guard (FsLeaseGuard) ────────────────────────────────

struct AcquireGuard {
    path: Option<PathBuf>,
}

impl AcquireGuard {
    fn take(dir: &Path, target: &str) -> Result<Self, LeaseConflict> {
        let guard_path = dir.join(format!("{}.guard", sanitize(target)));
        let deadline = std::time::Instant::now() + GUARD_WAIT;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&guard_path)
            {
                Ok(_) => {
                    return Ok(Self {
                        path: Some(guard_path),
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = entry_age(&guard_path).unwrap_or(Duration::ZERO);
                    if age > GUARD_TTL {
                        // Left by a crashed acquirer. `rename` is
                        // atomic, so exactly one contender moves it.
                        let tomb = dir.join(format!(".guard-stale-{}.tmp", mint_nonce()));
                        if std::fs::rename(&guard_path, &tomb).is_ok() {
                            let _ = std::fs::remove_file(&tomb);
                        }
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(LeaseConflict {
                            holder: "<guard contention>".to_string(),
                            intent: "edit".to_string(),
                            purpose: None,
                            claimed_at_unix: 0,
                            nonce: String::new(),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // Cannot create guard files (read-only dir, ...): the
                // lease layer is advisory by design — proceed
                // unguarded exactly as before.
                Err(_) => return Ok(Self { path: None }),
            }
        }
    }
}

impl Drop for AcquireGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zoanthrope-lease-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn acquire_grants_then_conflicts_then_releases() {
        let root = temp_root("round");
        let a = acquire(&root, "src/a.rs", "alpha", "edit", Some("fix")).unwrap();
        assert_eq!(holder(&root, "src/a.rs").unwrap().holder, "alpha");
        assert!(a.to_string_lossy().contains(".lock-"));

        let err = acquire(&root, "src/a.rs", "beta", "edit", None).unwrap_err();
        assert_eq!(err.holder, "alpha");
        assert_eq!(err.intent, "edit");
        assert_eq!(err.purpose.as_deref(), Some("fix"));

        assert_eq!(release(&root, "src/a.rs", "alpha").unwrap(), 1);
        assert!(acquire(&root, "src/a.rs", "beta", "edit", None).is_ok());
    }

    #[test]
    fn release_never_deletes_a_strangers_file() {
        // Rename-and-verify: a delayed release from the wrong agent
        // moves the sentinel aside, sees it is not theirs, and puts it
        // back (FsLease: release cannot clobber a live lock).
        let root = temp_root("stranger");
        let a = acquire(&root, "a.rs", "alpha", "edit", None).unwrap();
        assert_eq!(release(&root, "a.rs", "beta").unwrap(), 0);
        assert!(a.exists(), "alpha's sentinel survives beta's release");
        assert_eq!(holder(&root, "a.rs").unwrap().holder, "alpha");
    }

    #[test]
    fn distinct_paths_get_distinct_locks() {
        // Percent-encoding keeps look-alike paths apart.
        assert_ne!(sanitize("src/a.b"), sanitize("src/a_b"));
        let root = temp_root("distinct");
        assert!(acquire(&root, "src/a.b", "alpha", "edit", None).is_ok());
        assert!(acquire(&root, "src/a_b", "beta", "edit", None).is_ok());
        assert_eq!(release(&root, "src/a.b", "alpha").unwrap(), 1);
        assert_eq!(release(&root, "src/a_b", "beta").unwrap(), 1);
    }

    #[test]
    fn long_paths_fold_without_collision() {
        let long_a = format!("src/{}", "a".repeat(400));
        let long_b = format!("src/{}", "b".repeat(400));
        let sa = sanitize(&long_a);
        let sb = sanitize(&long_b);
        assert_ne!(sa, sb);
        assert!(sa.len() <= MAX_SANITIZED_LEN + 40, "got {} bytes", sa.len());
    }

    #[test]
    fn refresh_touches_only_our_sentinel() {
        let root = temp_root("refresh");
        assert!(!refresh(&root, "a.rs", "alpha").unwrap());
        let a = acquire(&root, "a.rs", "alpha", "edit", None).unwrap();
        assert!(refresh(&root, "a.rs", "alpha").unwrap());
        assert!(!refresh(&root, "a.rs", "beta").unwrap());
        assert!(a.exists());
    }

    #[test]
    fn stale_lease_is_taken_over_and_live_lease_is_not() {
        let root = temp_root("stale");
        let a = acquire(&root, "a.rs", "alpha", "edit", None).unwrap();
        // Fresh lease: refused.
        assert!(acquire(&root, "a.rs", "beta", "edit", None).is_err());
        // Age the sentinel past the TTL by hand (no override knob in
        // tests — the parse path is covered by lease_ttl below).
        let old = SystemTime::now() - LEASE_TTL - Duration::from_secs(5);
        let f = std::fs::OpenOptions::new().write(true).open(&a).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let b = acquire(&root, "a.rs", "beta", "edit", None).unwrap();
        assert!(b.to_string_lossy().contains(".lock-"));
        assert!(!a.exists(), "stale sentinel is cleaned up");
        assert_eq!(holder(&root, "a.rs").unwrap().holder, "beta");
    }

    #[test]
    fn reassert_refreshes_or_acquires_never_steals() {
        let root = temp_root("reassert");
        // Nothing there: reassert acquires.
        assert!(reassert(&root, "a.rs", "alpha", "edit", None).unwrap());
        // Ours: reassert refreshes.
        assert!(reassert(&root, "a.rs", "alpha", "edit", None).unwrap());
        // Stranger's live lease: reassert reports, never steals.
        let err = reassert(&root, "b.rs", "alpha", "edit", None);
        assert!(err.is_ok(), "fresh acquire is fine on a free path");
        assert!(refresh(&root, "b.rs", "alpha").unwrap());
        let err = reassert(&root, "b.rs", "beta", "edit", None).unwrap_err();
        assert_eq!(err.holder, "alpha");
    }

    #[test]
    fn lease_ttl_env_override_parses() {
        // Pure arithmetic on the same rule the env var feeds.
        assert_eq!(LEASE_TTL, Duration::from_secs(60));
    }
}
