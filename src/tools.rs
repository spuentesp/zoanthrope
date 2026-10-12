//! MCP tool implementations. Each tool takes parsed JSON arguments and
//! returns a JSON value; the transport lives in [`crate::mcp`].
//!
//! Success responses carry `"ok": true`; a refused claim is a *success*
//! whose body has `"ok": false` plus the conflict shape. `Err` is
//! reserved for malformed arguments and transport-level failures.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::fs_lease;
use crate::state::{self, Claim, ClaimIntent, ClaimOutcome, ContentStamp, RejoinReport};

const DEFAULT_AUDIT_LIMIT: usize = 200;

/// All tool names, in listing order.
pub const TOOL_NAMES: &[&str] = &[
    "claim_files",
    "list_claims",
    "who_am_i",
    "list_active_agents",
    "register_agent",
    "post_intention",
    "list_posts",
    "update_post",
    "remove_post",
    "rejoin_agent",
    "get_audit_log",
];

/// `tools/list` payload: name, description, inputSchema per tool.
pub fn tool_defs() -> Vec<Value> {
    vec![
        def(
            "claim_files",
            "Claim, release, or list file claims. mode is 'claim' (default), 'release', or 'list'. \
             files is a list of paths (strings) or {path, purpose?} objects; intent is 'edit' \
             (default) or 'read'. A claim is refused when it conflicts with another agent's \
             recorded intent (edit conflicts with anything, read conflicts with edit) and the \
             response is the conflict shape {ok: false, holder, path, claimed_at, purpose, \
             holder_stale, intent}. Claims are all-or-nothing per call. A stale holder still \
             blocks — claims are never auto-released. A granted claim also takes the filesystem \
             lease under .zoanthrope/locks/ (anti-clobbering) and binds the file's content \
             fingerprint (mtime+size, sha256 for small files). stale_check=true (what the \
             write guard passes) refuses with {ok: false, reason: 'stale-claim', holder} when \
             the file changed under the caller's own claim — re-read, then re-claim to re-bind.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string", "description": "Calling agent's stable name"},
                    "mode": {"type": "string", "enum": ["claim", "release", "list"]},
                    "intent": {"type": "string", "enum": ["read", "edit"]},
                    "stale_check": {"type": "boolean", "description": "Refuse a claim whose file changed since it was taken (guarded writes)"},
                    "files": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                {"type": "string"},
                                {
                                    "type": "object",
                                    "properties": {
                                        "path": {"type": "string"},
                                        "purpose": {"type": "string"}
                                    },
                                    "required": ["path"]
                                }
                            ]
                        }
                    },
                    "purpose": {"type": "string", "description": "Purpose for files that do not carry their own"}
                },
                "required": ["agent_id"]
            }),
        ),
        def(
            "list_claims",
            "List file claims across the workspace (or one agent's). Each entry carries \
             {path, agent, intent, claimed_at, purpose, held_by_stale}.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"}
                }
            }),
        ),
        def(
            "who_am_i",
            "Identity and holdings of one agent: kind, last_seen, stale flag, claims, posts.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"}
                },
                "required": ["agent_id"]
            }),
        ),
        def(
            "list_active_agents",
            "All known agents with {agent_id, name, kind, last_seen, stale, claims_count}. \
             Stale agents are listed, never dropped. Set include_stale=false to filter them out.",
            json!({
                "type": "object",
                "properties": {
                    "include_stale": {"type": "boolean"}
                }
            }),
        ),
        def(
            "register_agent",
            "Register this agent (or refresh its presence clock). kind is a free-form label \
             (e.g. 'claude', 'codex', 'kimi'). claim/release/post calls also register implicitly.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "kind": {"type": "string"}
                },
                "required": ["name"]
            }),
        ),
        def(
            "post_intention",
            "Post a public intention to the postboard: what this agent is about to do. \
             Returns {ok, post_id, post: {post_id, agent_id, title, files, note, status, \
             created_at, updated_at}}. New posts are status 'open'. Posts are the swarm's \
             public intentions — kept readable for humans debugging a swarm.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "title": {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}},
                    "note": {"type": "string"}
                },
                "required": ["agent_id", "title", "note"]
            }),
        ),
        def(
            "list_posts",
            "Posts, newest-updated first: {post_id, agent_id, title, files, note, status, \
             created_at, updated_at}. Filter by agent_id and/or status \
             (open|taken|done|dropped).",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "status": {"type": "string", "enum": ["open", "taken", "done", "dropped"]}
                }
            }),
        ),
        def(
            "update_post",
            "Update a post you own — usually its status: open|taken|done|dropped. \
             title, note, and files are optional partial updates.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "post_id": {"type": "string"},
                    "status": {"type": "string", "enum": ["open", "taken", "done", "dropped"]},
                    "title": {"type": "string"},
                    "note": {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["agent_id", "post_id"]
            }),
        ),
        def(
            "remove_post",
            "Remove a post you own from the postboard. Unknown ids fail loudly.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "post_id": {"type": "string"}
                },
                "required": ["agent_id", "post_id"]
            }),
        ),
        def(
            "rejoin_agent",
            "Rejoin the swarm after a gap (restart, sleep, disconnect). Refreshes presence, \
             re-asserts the caller's own claims (filesystem lease refreshed or re-taken), and \
             republishes the board. Never breaks claims — own or foreign. Returns \
             {ok, agent_id, claims_kept, claims_at_risk, view_epoch, board}.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}, "description": "Limit the rejoin to these claims"}
                },
                "required": ["agent_id"]
            }),
        ),
        def(
            "get_audit_log",
            "Audit ring (claims, releases, registrations, posts, rejoins), oldest first. \
             Args: limit (default 200), since_unix (drop events strictly older).",
            json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 0},
                    "since_unix": {"type": "number"}
                }
            }),
        ),
    ]
}

fn def(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
    })
}

/// Dispatch one tool call. `Err` means the call itself was malformed.
pub fn call_tool(root: &Path, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "claim_files" => claim_files(root, args),
        "list_claims" => list_claims(root, args),
        "who_am_i" => who_am_i(root, args),
        "list_active_agents" => list_active_agents(root, args),
        "register_agent" => register_agent(root, args),
        "post_intention" => post_intention(root, args),
        "list_posts" => list_posts(root, args),
        "update_post" => update_post(root, args),
        "remove_post" => remove_post(root, args),
        "rejoin_agent" => rejoin_agent(root, args),
        "get_audit_log" => get_audit_log(root, args),
        other => Err(format!(
            "unknown tool '{other}' (known: {})",
            TOOL_NAMES.join(", ")
        )),
    }
}

// ─── argument helpers ───────────────────────────────────────────────

fn agent_id(args: &Value) -> Result<String, String> {
    args.get("agent_id")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "agent_id is required".to_string())
}

fn optional_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn optional_str_list(args: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            let mut out = Vec::new();
            for v in a {
                let s = v
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("{key} entries must be non-empty strings"))?;
                out.push(s);
            }
            Ok(Some(out))
        }
        Some(_) => Err(format!("{key} must be a list of strings")),
    }
}

fn claim_intent(args: &Value) -> Result<ClaimIntent, String> {
    match args.get("intent") {
        None | Some(Value::Null) => Ok(ClaimIntent::Edit),
        Some(Value::String(s)) => ClaimIntent::parse(s),
        Some(_) => Err("intent must be 'read' or 'edit'".into()),
    }
}

struct FileReq {
    path: String,
    purpose: Option<String>,
}

fn file_list(args: &Value, common_purpose: Option<&str>) -> Result<Vec<FileReq>, String> {
    let files = args.get("files").and_then(Value::as_array).ok_or_else(|| {
        "files is required (list of paths or {path, purpose?} objects)".to_string()
    })?;
    let mut out = Vec::new();
    for f in files {
        let req = match f {
            Value::String(s) => FileReq {
                path: s.trim().to_string(),
                purpose: common_purpose.map(str::to_string),
            },
            Value::Object(_) => FileReq {
                path: f
                    .get("path")
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "file entry needs a non-empty 'path'".to_string())?,
                purpose: f
                    .get("purpose")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| common_purpose.map(str::to_string)),
            },
            _ => return Err("file entries must be strings or {path, purpose?} objects".into()),
        };
        if req.path.is_empty() {
            return Err("file path must not be empty".into());
        }
        out.push(req);
    }
    if out.is_empty() {
        return Err("files must not be empty".into());
    }
    Ok(out)
}

/// The single path-identity boundary: every incoming spelling (CLI,
/// MCP, hooks) becomes [`state::canonical_key`] before it touches a
/// claim, a lease, or a postboard entry.
fn canon(root: &Path, raw: &str) -> String {
    state::canonical_key(root, raw)
}

fn canon_pairs(root: &Path, files: &[(String, Option<String>)]) -> Vec<(String, Option<String>)> {
    files
        .iter()
        .map(|(p, purpose)| (canon(root, p), purpose.clone()))
        .collect()
}

fn canon_list(root: &Path, files: &[String]) -> Vec<String> {
    files.iter().map(|p| canon(root, p)).collect()
}

// ─── tools ──────────────────────────────────────────────────────────

fn claim_files(root: &Path, args: &Value) -> Result<Value, String> {
    let agent = agent_id(args)?;
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("claim");
    let intent = claim_intent(args)?;
    let common_purpose = args.get("purpose").and_then(Value::as_str);
    match mode {
        "claim" => {
            let files = file_list(args, common_purpose)?;
            let stale_check = args
                .get("stale_check")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let pairs: Vec<(String, Option<String>)> =
                files.into_iter().map(|f| (f.path, f.purpose)).collect();
            cli_claim_inner(root, &agent, &pairs, intent, stale_check)
        }
        "release" => {
            let files = file_list(args, common_purpose)?;
            let paths: Vec<String> = files.into_iter().map(|f| f.path).collect();
            Ok(cli_release_inner(root, &agent, &paths))
        }
        "list" => state::read(root, |s| {
            let now = state::now_unix();
            json!({
                "ok": true,
                "claims": s.claim_views(Some(&agent), now, state::ttl_secs()),
            })
        }),
        other => Err(format!(
            "claim_files: mode must be 'claim', 'release', or 'list' (got '{other}')"
        )),
    }
}

fn list_claims(root: &Path, args: &Value) -> Result<Value, String> {
    let agent = match args.get("agent_id").and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    };
    state::read(root, |s| {
        let now = state::now_unix();
        json!({
            "ok": true,
            "claims": s.claim_views(agent.as_deref(), now, state::ttl_secs()),
        })
    })
}

fn who_am_i(root: &Path, args: &Value) -> Result<Value, String> {
    let agent = agent_id(args)?;
    state::read(root, |s| {
        let now = state::now_unix();
        match s.who(&agent, now, state::ttl_secs()) {
            Some(mut v) => {
                v["ok"] = json!(true);
                v
            }
            None => json!({"ok": false, "error": format!("unknown agent '{agent}'")}),
        }
    })
}

fn list_active_agents(root: &Path, args: &Value) -> Result<Value, String> {
    let include_stale = args
        .get("include_stale")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    state::read(root, |s| {
        let now = state::now_unix();
        let agents: Vec<Value> = s
            .agent_views(now, state::ttl_secs())
            .into_iter()
            .filter(|a| include_stale || a["stale"] != json!(true))
            .collect();
        json!({"ok": true, "agents": agents})
    })
}

fn register_agent(root: &Path, args: &Value) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "register_agent: name is required".to_string())?;
    let kind = args
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("other")
        .trim()
        .to_string();
    state::update(root, |s| {
        let now = state::now_unix();
        let a = s.register(&name, &kind, now);
        Ok(json!({
            "ok": true,
            "agent_id": a.name,
            "name": a.name,
            "kind": a.kind,
            "last_seen": a.last_seen,
        }))
    })
}

// ─── postboard ──────────────────────────────────────────────────────

fn post_intention(root: &Path, args: &Value) -> Result<Value, String> {
    let agent = agent_id(args)?;
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "post_intention: title is required".to_string())?;
    let note = args
        .get("note")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "post_intention: note is required".to_string())?;
    let files = optional_str_list(args, "files")?.unwrap_or_default();
    let files = canon_list(root, &files);
    state::update(root, |s| {
        let now = state::now_unix();
        // Implicit registration keeps hooks frictionless.
        s.register(&agent, "other", now);
        let p = s.create_post(&agent, &title, files, &note, now);
        Ok(json!({
            "ok": true,
            "post_id": p.post_id,
            "post": {
                "post_id": p.post_id,
                "agent_id": p.agent,
                "title": p.title,
                "files": p.files,
                "note": p.note,
                "status": p.status,
                "created_at": p.created_at,
                "updated_at": p.updated_at,
            }
        }))
    })
}

fn list_posts(root: &Path, args: &Value) -> Result<Value, String> {
    let agent = optional_str(args, "agent_id")?.filter(|s| !s.trim().is_empty());
    let status = match optional_str(args, "status")? {
        Some(s) if !s.trim().is_empty() => Some(state::normalize_post_status(&s)?),
        _ => None,
    };
    state::read(
        root,
        |s| json!({"ok": true, "posts": s.post_views(agent.as_deref(), status.as_deref())}),
    )
}

fn update_post(root: &Path, args: &Value) -> Result<Value, String> {
    let agent = agent_id(args)?;
    let post_id = args
        .get("post_id")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "update_post: post_id is required".to_string())?;
    let status = optional_str(args, "status")?;
    let title = optional_str(args, "title")?;
    let note = optional_str(args, "note")?;
    let files = optional_str_list(args, "files")?.map(|f| canon_list(root, &f));
    if status.is_none() && title.is_none() && note.is_none() && files.is_none() {
        return Err("update_post: nothing to update (status, title, note, or files)".into());
    }
    state::update(root, |s| {
        let now = state::now_unix();
        s.register(&agent, "other", now);
        let p = s.update_post(
            &agent,
            &post_id,
            status.as_deref(),
            title.as_deref(),
            note.as_deref(),
            files,
            now,
        )?;
        Ok(json!({
            "ok": true,
            "post_id": p.post_id,
            "post": {
                "post_id": p.post_id,
                "agent_id": p.agent,
                "title": p.title,
                "files": p.files,
                "note": p.note,
                "status": p.status,
                "created_at": p.created_at,
                "updated_at": p.updated_at,
            }
        }))
    })
}

fn remove_post(root: &Path, args: &Value) -> Result<Value, String> {
    let agent = agent_id(args)?;
    let post_id = args
        .get("post_id")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "remove_post: post_id is required".to_string())?;
    state::update(root, |s| {
        let now = state::now_unix();
        s.register(&agent, "other", now);
        let p = s.remove_post(&agent, &post_id, now)?;
        Ok(json!({"ok": true, "post_id": p.post_id, "removed": true}))
    })
}

// ─── rejoin ─────────────────────────────────────────────────────────

fn rejoin_agent(root: &Path, args: &Value) -> Result<Value, String> {
    let agent = agent_id(args)?;
    let files = optional_str_list(args, "files")?.map(|f| canon_list(root, &f));
    let mut report = state::update(root, |s| {
        let now = state::now_unix();
        s.register(&agent, "other", now);
        let scope = s.rejoin_report(&agent, files.as_deref(), now);
        let mut kept = Vec::new();
        let mut at_risk: Vec<Value> = Vec::new();
        for path in &scope {
            let claim = s
                .claims
                .iter()
                .find(|c| c.agent == agent && c.path == *path)
                .cloned();
            let intent = claim
                .as_ref()
                .map(|c| c.intent.as_str().to_string())
                .unwrap_or_else(|| "edit".to_string());
            let purpose = claim.as_ref().and_then(|c| c.purpose.clone());
            // Re-assert the caller's own lease (edit claims hold one);
            // a stranger's live lease is reported, never stolen back.
            if intent != "edit" {
                kept.push(path.clone());
                continue;
            }
            match fs_lease::reassert(root, path, &agent, &intent, purpose.as_deref()) {
                Ok(_) => kept.push(path.clone()),
                Err(c) => {
                    let mut row = c.to_json(path);
                    row["at_risk"] = json!(true);
                    at_risk.push(row);
                }
            }
        }
        Ok(RejoinReport {
            agent: agent.clone(),
            claims_kept: kept,
            claims_at_risk: at_risk,
            view_epoch: 0,
        })
    })?;
    let board = state::read(root, |s| s.board.clone())?;
    report.view_epoch = board.epoch;
    Ok(report.to_json(&board))
}

// ─── audit ──────────────────────────────────────────────────────────

fn get_audit_log(root: &Path, args: &Value) -> Result<Value, String> {
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_AUDIT_LIMIT,
        Some(v) => v
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| "get_audit_log: limit must be a non-negative integer".to_string())?,
    };
    let since = match args.get("since_unix") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .ok_or_else(|| "get_audit_log: since_unix must be a number".to_string())?,
        ),
    };
    state::read(
        root,
        |s| json!({"ok": true, "events": s.audit_events(limit, since)}),
    )
}

// ─── CLI mirrors ────────────────────────────────────────────────────

/// The `claim_files` body without JSON parsing — shared by the
/// `zoanthrope claim` subcommand hooks shell out to. `stale_check`
/// turns a fingerprint mismatch on the caller's own claim into a
/// `stale-claim` refusal instead of a re-bind (what claim-guard.sh
/// passes for every guarded write).
pub fn cli_claim(
    root: PathBuf,
    agent: &str,
    files: &[String],
    purpose: Option<&str>,
    intent: Option<&str>,
    stale_check: bool,
) -> Value {
    let intent = match intent {
        None => ClaimIntent::Edit,
        Some(raw) => match ClaimIntent::parse(raw) {
            Ok(i) => i,
            Err(e) => return json!({"ok": false, "error": e}),
        },
    };
    let pairs: Vec<(String, Option<String>)> = files
        .iter()
        .map(|f| (f.clone(), purpose.map(str::to_string)))
        .collect();
    cli_claim_inner(&root, agent, &pairs, intent, stale_check)
        .unwrap_or_else(|e| json!({"ok": false, "error": e}))
}

/// The `release` body without JSON parsing — shared by
/// `zoanthrope release`.
pub fn cli_release(root: PathBuf, agent: &str, files: &[String]) -> Value {
    cli_release_inner(&root, agent, files)
}

/// The `rejoin` body without JSON parsing — shared by
/// `zoanthrope rejoin`.
pub fn cli_rejoin(root: PathBuf, agent: &str, files: Option<Vec<String>>) -> Value {
    let args = match files {
        Some(f) => json!({"agent_id": agent, "files": f}),
        None => json!({"agent_id": agent}),
    };
    rejoin_agent(&root, &args).unwrap_or_else(|e| json!({"ok": false, "error": e}))
}

fn cli_claim_inner(
    root: &Path,
    agent: &str,
    files: &[(String, Option<String>)],
    intent: ClaimIntent,
    stale_check: bool,
) -> Result<Value, String> {
    let files = canon_pairs(root, files);
    state::update(root, |s| {
        let now = state::now_unix();
        let ttl = state::ttl_secs();
        s.register(agent, "other", now);
        // Fingerprint check for guarded writes: the agent's own claim
        // must still describe the file's content. All-or-nothing; a
        // stale claim is left untouched — the recovery is re-read then
        // re-claim (a plain re-claim re-binds the fingerprint). No
        // deliberate takeover exists: a foreign claim is a conflict.
        if stale_check {
            for (path, _) in &files {
                let own = s
                    .claims
                    .iter()
                    .find(|c| c.agent == agent && &c.path == path);
                if let (Some(c), Some(stamp)) = (own, own.and_then(|c| c.fingerprint.as_ref())) {
                    let cur = ContentStamp::capture(&abs_of(root, path));
                    if !stamp.matches(&cur) {
                        return Ok(stale_claim_json(agent, c));
                    }
                }
            }
        }
        let checks: Vec<(String, ClaimIntent)> =
            files.iter().map(|(p, _)| (p.clone(), intent)).collect();
        // Registry arbitration first (PresenceClaims). Nothing is
        // written on a conflict — all-or-nothing per call.
        if let Err(c) = s.check_claim(agent, &checks, now, ttl) {
            return Ok(c.to_json());
        }
        // Anti-clobbering step: the filesystem lease guards *writes*,
        // so only edit claims take one (readers co-hold and cannot
        // clobber). Re-assert, so a re-claim of a path we already hold
        // refreshes our own lease instead of colliding with it. One
        // live foreign lease refuses the whole call.
        let mut taken: Vec<String> = Vec::new();
        if intent == ClaimIntent::Edit {
            for (path, purpose) in &files {
                match fs_lease::reassert(root, path, agent, intent.as_str(), purpose.as_deref()) {
                    Ok(_) => taken.push(path.clone()),
                    Err(c) => {
                        for p in &taken {
                            let _ = fs_lease::release(root, p, agent);
                        }
                        return Ok(c.to_json(path));
                    }
                }
            }
        }
        Ok(match s.claim(agent, &files, intent, now, ttl) {
            ClaimOutcome::Granted(granted) => {
                // Bind content at claim/re-claim time.
                for (path, _) in &files {
                    if let Some(c) = s
                        .claims
                        .iter_mut()
                        .find(|c| c.agent == agent && &c.path == path)
                    {
                        c.fingerprint = Some(ContentStamp::capture(&abs_of(root, path)));
                    }
                }
                grant_json(&granted)
            }
            ClaimOutcome::Conflict(c) => c.to_json(),
        })
    })
}

/// Resolve a canonical key back to a filesystem path: keys under the
/// root are relative, foreign paths stay absolute.
fn abs_of(root: &Path, key: &str) -> PathBuf {
    if Path::new(key).is_absolute() {
        PathBuf::from(key)
    } else {
        root.join(key)
    }
}

/// The `stale-claim` refusal: the holder is the caller — the file
/// changed under their own claim. Never a takeover; re-read, re-claim.
fn stale_claim_json(agent: &str, claim: &Claim) -> Value {
    json!({
        "ok": false,
        "reason": "stale-claim",
        "holder": agent,
        "path": claim.path,
        "claimed_at": claim.claimed_at,
        "hint": "re-read the file, then re-claim before writing",
    })
}

fn grant_json(granted: &[Claim]) -> Value {
    json!({
        "ok": true,
        "granted": granted.iter().map(|c| json!({
            "path": c.path,
            "claimed_at": c.claimed_at,
            "purpose": c.purpose,
            "intent": c.intent.as_str(),
        })).collect::<Vec<_>>(),
    })
}

fn cli_release_inner(root: &Path, agent: &str, files: &[String]) -> Value {
    let files = canon_list(root, files);
    match state::update(root, |s| {
        let now = state::now_unix();
        s.register(agent, "other", now);
        let released = s.release(agent, &files, now);
        // Drop the matching filesystem leases too (rename-and-verify,
        // owner-only — a stranger's sentinel is put back).
        for path in &released {
            let _ = fs_lease::release(root, path, agent);
        }
        Ok(released)
    }) {
        Ok(released) => json!({"ok": true, "released": released}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// `zoanthrope status` text summary.
pub fn status_text(root: &Path) -> String {
    let state = state::load(root).unwrap_or_default();
    let now = state::now_unix();
    let ttl = state::ttl_secs();
    let mut out = String::new();
    out.push_str(&format!("zoanthrope workspace: {}\n", root.display()));
    out.push_str(&format!(
        "presence TTL: {}s (ZOANTHROPE_TTL_SECS overrides)\n",
        ttl
    ));
    out.push_str(&format!(
        "board: epoch {} ({} claims, {} open posts, {} agents)\n\n",
        state.board.epoch,
        state.board.claims_count,
        state.board.open_posts,
        state.board.agents_count
    ));

    out.push_str(&format!("agents ({}):\n", state.agents.len()));
    if state.agents.is_empty() {
        out.push_str("  (none)\n");
    }
    for a in state.agent_views(now, ttl) {
        let stale = if a["stale"] == json!(true) {
            "STALE"
        } else {
            "live"
        };
        out.push_str(&format!(
            "  {:<16} kind={:<10} last_seen={:>6}s ago  {stale:<5} claims={}\n",
            a["name"].as_str().unwrap_or("?"),
            a["kind"].as_str().unwrap_or("?"),
            (now - a["last_seen"].as_f64().unwrap_or(now)).max(0.0) as i64,
            a["claims_count"],
        ));
    }

    let claims = state.claim_views(None, now, ttl);
    out.push_str(&format!("\nclaims ({}):\n", claims.len()));
    if claims.is_empty() {
        out.push_str("  (none)\n");
    }
    for c in &claims {
        let stale = if c["held_by_stale"] == json!(true) {
            " HELD-BY-STALE"
        } else {
            ""
        };
        out.push_str(&format!(
            "  {:<28} {}{} [{}]  purpose={}\n",
            c["path"].as_str().unwrap_or("?"),
            c["agent"].as_str().unwrap_or("?"),
            stale,
            c["intent"].as_str().unwrap_or("?"),
            c["purpose"].as_str().unwrap_or("-"),
        ));
    }

    // The OS-level lease layer: what the filesystem itself says, for
    // debugging orphaned claims and stray sentinels.
    out.push_str("\nleases (filesystem):\n");
    let mut any_lease = false;
    for c in &claims {
        if let Some(h) = fs_lease::holder(root, c["path"].as_str().unwrap_or("")) {
            any_lease = true;
            out.push_str(&format!(
                "  {:<28} {} [{}]\n",
                c["path"].as_str().unwrap_or("?"),
                h.holder,
                h.intent,
            ));
        }
    }
    if !any_lease {
        out.push_str("  (none)\n");
    }

    let posts = state.post_views(None, None);
    out.push_str(&format!("\npostboard ({}):\n", posts.len()));
    if posts.is_empty() {
        out.push_str("  (none)\n");
    }
    for p in &posts {
        out.push_str(&format!(
            "  [{}] {:<16} {:<8} {}  files={:?}\n",
            p["post_id"].as_str().unwrap_or("?"),
            p["agent_id"].as_str().unwrap_or("?"),
            p["status"].as_str().unwrap_or("?"),
            p["title"].as_str().unwrap_or("?"),
            p["files"],
        ));
    }

    out.push_str(&format!(
        "\naudit: {} event(s) in ring (cap {})\n",
        state.audit.len(),
        state::AUDIT_CAP
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zoanthrope-tools-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claim_grant_and_conflict_shapes() {
        let root = temp_root("shapes");
        let grant = call_tool(
            &root,
            "claim_files",
            &json!({
                "agent_id": "alpha",
                "files": [{"path": "a.rs", "purpose": "fix bug"}]
            }),
        )
        .unwrap();
        assert_eq!(grant["ok"], true);
        assert_eq!(grant["granted"][0]["path"], "a.rs");
        assert_eq!(grant["granted"][0]["purpose"], "fix bug");
        assert_eq!(grant["granted"][0]["intent"], "edit");

        let conflict = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "beta", "files": ["a.rs"]}),
        )
        .unwrap();
        // The exact conflict shape — the heart of the product.
        assert_eq!(conflict["ok"], false);
        assert_eq!(conflict["holder"], "alpha");
        assert_eq!(conflict["claimed_at"], grant["granted"][0]["claimed_at"]);
        assert_eq!(conflict["purpose"], "fix bug");
        assert_eq!(conflict["holder_stale"], false);
        assert_eq!(conflict["path"], "a.rs");
        assert_eq!(conflict["intent"], "edit");
        let mut keys: Vec<&str> = conflict
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "claimed_at",
                "holder",
                "holder_stale",
                "intent",
                "ok",
                "path",
                "purpose"
            ]
        );
    }

    #[test]
    fn claim_takes_and_release_drops_the_filesystem_lease() {
        // The anti-clobbering coupling: a granted claim holds the
        // lease; a live foreign lease refuses a claim even before the
        // registry would.
        let root = temp_root("lease-coupling");
        let grant = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["a.rs"]}),
        )
        .unwrap();
        assert_eq!(grant["ok"], true);
        assert_eq!(
            fs_lease::holder(&root, "a.rs").map(|h| h.holder),
            Some("alpha".to_string())
        );

        // A lease-only holder (no registry claim) still blocks.
        let _ = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "mode": "release", "files": ["a.rs"]}),
        )
        .unwrap();
        fs_lease::acquire(&root, "a.rs", "ghost", "edit", Some("editor save")).unwrap();
        let conflict = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "beta", "files": ["a.rs"]}),
        )
        .unwrap();
        assert_eq!(conflict["ok"], false);
        assert_eq!(conflict["holder"], "ghost");
        assert_eq!(conflict["lease"], true);

        // Release frees both layers.
        let _ = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["b.rs"]}),
        )
        .unwrap();
        let rel = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "mode": "release", "files": ["b.rs"]}),
        )
        .unwrap();
        assert_eq!(rel["ok"], true);
        assert!(fs_lease::holder(&root, "b.rs").is_none());
    }

    #[test]
    fn read_claims_cohold_and_edit_claims_exclude() {
        // PresenceClaims arbitration through the tool surface.
        let root = temp_root("intents");
        let a = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["a.rs"], "intent": "read"}),
        )
        .unwrap();
        assert_eq!(a["ok"], true);
        let b = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "beta", "files": ["a.rs"], "intent": "read"}),
        )
        .unwrap();
        assert_eq!(b["ok"], true, "readers co-hold");
        let c = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "gamma", "files": ["a.rs"], "intent": "edit"}),
        )
        .unwrap();
        assert_eq!(c["ok"], false, "edit excludes readers");
        assert_eq!(c["holder"], "alpha");
    }

    #[test]
    fn release_list_and_stale_views() {
        let root = temp_root("views");
        call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["a.rs"], "purpose": "wip"}),
        )
        .unwrap();
        let rel = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "mode": "release", "files": ["a.rs"]}),
        )
        .unwrap();
        assert_eq!(rel["ok"], true);
        assert_eq!(rel["released"], json!(["a.rs"]));

        call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["b.rs"]}),
        )
        .unwrap();
        let listed = call_tool(&root, "list_claims", &json!({})).unwrap();
        assert_eq!(listed["ok"], true);
        assert_eq!(listed["claims"].as_array().unwrap().len(), 1);
        assert_eq!(listed["claims"][0]["path"], "b.rs");
        assert_eq!(listed["claims"][0]["intent"], "edit");

        let mine = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "mode": "list", "files": ["b.rs"]}),
        )
        .unwrap();
        assert_eq!(mine["claims"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn postboard_create_update_list_remove() {
        let root = temp_root("postboard");
        let posted = call_tool(
            &root,
            "post_intention",
            &json!({
                "agent_id": "alpha",
                "title": "fix the flake",
                "files": ["a.rs"],
                "note": "shows up under load"
            }),
        )
        .unwrap();
        assert_eq!(posted["ok"], true);
        assert_eq!(posted["post"]["status"], "open");
        assert_eq!(posted["post"]["title"], "fix the flake");
        let id = posted["post_id"].as_str().unwrap().to_string();

        let updated = call_tool(
            &root,
            "update_post",
            &json!({"agent_id": "alpha", "post_id": id, "status": "taken"}),
        )
        .unwrap();
        assert_eq!(updated["ok"], true);
        assert_eq!(updated["post"]["status"], "taken");
        assert_eq!(updated["post"]["title"], "fix the flake");

        let listed = call_tool(&root, "list_posts", &json!({"status": "taken"})).unwrap();
        assert_eq!(listed["posts"].as_array().unwrap().len(), 1);
        let by_agent = call_tool(&root, "list_posts", &json!({"agent_id": "alpha"})).unwrap();
        assert_eq!(by_agent["posts"].as_array().unwrap().len(), 1);

        let removed = call_tool(
            &root,
            "remove_post",
            &json!({"agent_id": "alpha", "post_id": id}),
        )
        .unwrap();
        assert_eq!(removed["ok"], true);
        let empty = call_tool(&root, "list_posts", &json!({})).unwrap();
        assert_eq!(empty["posts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn who_am_i_and_register_shapes() {
        let root = temp_root("who");
        let reg = call_tool(
            &root,
            "register_agent",
            &json!({"name": "beta", "kind": "codex"}),
        )
        .unwrap();
        assert_eq!(reg["ok"], true);
        assert_eq!(reg["agent_id"], "beta");
        assert_eq!(reg["kind"], "codex");

        let who = call_tool(&root, "who_am_i", &json!({"agent_id": "beta"})).unwrap();
        assert_eq!(who["ok"], true);
        assert_eq!(who["name"], "beta");
        assert_eq!(who["stale"], false);
        assert!(who["claims"].as_array().unwrap().is_empty());
        assert!(who["posts"].as_array().unwrap().is_empty());

        let unknown = call_tool(&root, "who_am_i", &json!({"agent_id": "ghost"})).unwrap();
        assert_eq!(unknown["ok"], false);
    }

    #[test]
    fn same_file_under_every_spelling_is_one_claim() {
        // Path identity (P0): the CLI/MCP boundary canonicalizes every
        // incoming spelling to one workspace-root-relative key, so one
        // file can never hold two claims under two spellings.
        let root = temp_root("path-identity");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        let abs = root.join("src/a.rs").to_string_lossy().into_owned();

        let grant = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["src/a.rs"]}),
        )
        .unwrap();
        assert_eq!(grant["ok"], true);
        assert_eq!(grant["granted"][0]["path"], "src/a.rs");

        for spelling in [
            "./src/a.rs".to_string(),
            abs.clone(),
            "src/../src/a.rs".to_string(),
            "src\\a.rs".to_string(),
        ] {
            let out = call_tool(
                &root,
                "claim_files",
                &json!({"agent_id": "beta", "files": [spelling]}),
            )
            .unwrap();
            assert_eq!(
                out["ok"], false,
                "spelling {spelling:?} must conflict with the held claim"
            );
            assert_eq!(out["holder"], "alpha", "spelling {spelling:?}");
            assert_eq!(
                out["path"], "src/a.rs",
                "conflict must name the canonical key for {spelling:?}"
            );
        }

        // Exactly one claim exists — the spellings never split it.
        let listed = call_tool(&root, "list_claims", &json!({})).unwrap();
        let claims = listed["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 1, "one file, one claim: {claims:?}");
        assert_eq!(claims[0]["path"], "src/a.rs");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn distinct_files_stay_distinct_under_canonicalization() {
        let root = temp_root("path-distinct");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "a\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "b\n").unwrap();
        let grant = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["src/a.rs", "./src/b.rs"]}),
        )
        .unwrap();
        assert_eq!(grant["ok"], true);
        assert_eq!(grant["granted"].as_array().unwrap().len(), 2);
        // `src/../src/b.rs` is b.rs, not a third file — re-claim of the
        // same key, not a conflict.
        let again = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["src/../src/b.rs"]}),
        )
        .unwrap();
        assert_eq!(again["ok"], true, "own re-claim under any spelling");
        let listed = call_tool(&root, "list_claims", &json!({})).unwrap();
        assert_eq!(listed["claims"].as_array().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn release_and_postboard_share_the_canonical_key() {
        let root = temp_root("path-release");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "a\n").unwrap();
        let abs = root.join("src/a.rs").to_string_lossy().into_owned();
        call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["src/a.rs"]}),
        )
        .unwrap();
        // Release under the absolute spelling frees the relative claim.
        let rel = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "mode": "release", "files": [abs]}),
        )
        .unwrap();
        assert_eq!(rel["released"], json!(["src/a.rs"]));
        let listed = call_tool(&root, "list_claims", &json!({})).unwrap();
        assert_eq!(listed["claims"].as_array().unwrap().len(), 0);

        // Postboard file refs are the same keys.
        let posted = call_tool(
            &root,
            "post_intention",
            &json!({
                "agent_id": "alpha",
                "title": "t",
                "note": "n",
                "files": [root.join("src/a.rs").to_string_lossy(), "src\\a.rs"]
            }),
        )
        .unwrap();
        assert_eq!(posted["post"]["files"], json!(["src/a.rs", "src/a.rs"]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn guard_check_denies_a_stale_held_claim() {
        // P1: a claim binds to the file's content at claim time. An
        // out-of-band change under a held claim makes the next guarded
        // write stale — DENY with `stale-claim` (holder is self) so the
        // writer re-reads instead of clobbering.
        let root = temp_root("stale-claim");
        std::fs::write(root.join("f.txt"), "seed\n").unwrap();
        call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["f.txt"]}),
        )
        .unwrap();
        // out-of-band process appends — B's addition lands under the claim
        std::fs::write(root.join("f.txt"), "seed\nbeta-new-line\n").unwrap();
        let out = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["f.txt"], "stale_check": true}),
        )
        .unwrap();
        assert_eq!(out["ok"], false, "stale write must not pass: {out}");
        assert_eq!(out["reason"], "stale-claim");
        assert_eq!(out["holder"], "alpha", "holder is self");
        assert_eq!(out["path"], "f.txt");
        // the claim is left untouched — no silent takeover/refresh
        let listed = call_tool(&root, "list_claims", &json!({})).unwrap();
        assert_eq!(listed["claims"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn plain_reclaim_refreshes_and_then_guard_writes_pass() {
        let root = temp_root("stale-recover");
        std::fs::write(root.join("f.txt"), "seed\n").unwrap();
        call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["f.txt"]}),
        )
        .unwrap();
        std::fs::write(root.join("f.txt"), "seed\nbeta-new-line\n").unwrap();
        // recovery protocol: re-read (not guarded), then re-claim — the
        // re-claim re-binds the fingerprint to the current content
        let reclaim = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["f.txt"]}),
        )
        .unwrap();
        assert_eq!(reclaim["ok"], true, "plain re-claim refreshes: {reclaim}");
        let write = call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["f.txt"], "stale_check": true}),
        )
        .unwrap();
        assert_eq!(write["ok"], true, "guarded write after re-claim: {write}");
    }

    #[test]
    fn claim_write_release_roundtrip_is_unaffected_by_fingerprints() {
        // The normal flow must never trip the check: claim → write →
        // release, then the next claim is fresh again.
        let root = temp_root("stale-roundtrip");
        std::fs::write(root.join("f.txt"), "one\n").unwrap();
        for i in 0..3 {
            let guard = call_tool(
                &root,
                "claim_files",
                &json!({"agent_id": "alpha", "files": ["f.txt"], "stale_check": true}),
            )
            .unwrap();
            assert_eq!(guard["ok"], true, "window {i} must grant: {guard}");
            std::fs::write(root.join("f.txt"), format!("write {i}\n")).unwrap();
            let rel = call_tool(
                &root,
                "claim_files",
                &json!({"agent_id": "alpha", "mode": "release", "files": ["f.txt"]}),
            )
            .unwrap();
            assert_eq!(rel["ok"], true);
        }
    }

    #[test]
    fn bad_arguments_are_errors() {
        let root = temp_root("badargs");
        assert!(call_tool(&root, "claim_files", &json!({})).is_err());
        assert!(call_tool(&root, "nope", &json!({})).is_err());
        assert!(call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "a", "mode": "sideways", "files": ["x"]})
        )
        .is_err());
        assert!(
            call_tool(
                &root,
                "post_intention",
                &json!({"agent_id": "a", "note": "n"})
            )
            .is_err(),
            "posting without a title must fail"
        );
        assert!(
            call_tool(
                &root,
                "post_intention",
                &json!({"agent_id": "a", "title": "t"})
            )
            .is_err(),
            "posting without a note must fail"
        );
        assert!(
            call_tool(
                &root,
                "update_post",
                &json!({"agent_id": "a", "post_id": "p1", "status": "cooking"})
            )
            .is_err(),
            "the status set is closed"
        );
        assert!(
            call_tool(
                &root,
                "claim_files",
                &json!({"agent_id": "a", "files": ["x"], "intent": "watch"})
            )
            .is_err(),
            "the intent set is closed"
        );
    }

    #[test]
    fn audit_log_shape() {
        let root = temp_root("audit");
        call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["a.rs"]}),
        )
        .unwrap();
        call_tool(
            &root,
            "post_intention",
            &json!({"agent_id": "alpha", "title": "ship", "note": "n"}),
        )
        .unwrap();
        let log = call_tool(&root, "get_audit_log", &json!({"limit": 10})).unwrap();
        assert_eq!(log["ok"], true);
        let events = log["events"].as_array().unwrap();
        assert!(events.len() >= 3, "register + claim + post events");
        assert_eq!(events[0]["action"], "register");
        assert_eq!(events[1]["action"], "claim");
        assert_eq!(events[1]["path"], "a.rs");
        assert_eq!(events[2]["action"], "post");
    }

    #[test]
    fn rejoin_keeps_claims_and_republishes_the_board() {
        let root = temp_root("rejoin");
        call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["a.rs", "b.rs"]}),
        )
        .unwrap();
        let body = call_tool(&root, "rejoin_agent", &json!({"agent_id": "alpha"})).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["claims_kept"], json!(["a.rs", "b.rs"]));
        assert_eq!(body["claims_at_risk"], json!([]));
        assert!(body["view_epoch"].as_u64().unwrap() >= 1);
        assert_eq!(body["board"]["claims_count"], 2);
        // Rejoin broke nothing.
        let claims = call_tool(&root, "list_claims", &json!({})).unwrap();
        assert_eq!(claims["claims"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn status_text_mentions_everything() {
        let root = temp_root("status");
        call_tool(
            &root,
            "register_agent",
            &json!({"name": "alpha", "kind": "claude"}),
        )
        .unwrap();
        call_tool(
            &root,
            "claim_files",
            &json!({"agent_id": "alpha", "files": ["a.rs"], "purpose": "wip"}),
        )
        .unwrap();
        call_tool(
            &root,
            "post_intention",
            &json!({"agent_id": "alpha", "title": "ship it", "note": "careful"}),
        )
        .unwrap();
        let text = status_text(&root);
        assert!(text.contains("alpha"));
        assert!(text.contains("a.rs"));
        assert!(text.contains("ship it"));
        assert!(text.contains("postboard ("));
        assert!(text.contains("board: epoch "));
        assert!(text.contains("audit: "));
    }
}
