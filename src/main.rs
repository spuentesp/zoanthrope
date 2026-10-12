//! zoanthrope — work-coordination MCP server for multi-agent swarms.
//!
//! Answers who is working on what: file claims with read/edit intents,
//! presence, a postboard of intentions, filesystem leases, rejoin, and
//! a capped audit ring.

use std::path::PathBuf;
use std::process::ExitCode;

use zoanthrope::{bash_parse, mcp, state, tools};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("zoanthrope: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let (cmd, rest) = args.split_first().ok_or_else(usage)?;
    match cmd.as_str() {
        "mcp" => {
            let ws = WorkspaceArgs::parse(rest)?;
            mcp::run_stdio(&ws.root).map_err(|e| format!("mcp: {e}"))?;
            Ok(ExitCode::SUCCESS)
        }
        "status" => {
            let ws = WorkspaceArgs::parse(rest)?;
            print!("{}", tools::status_text(&ws.root));
            Ok(ExitCode::SUCCESS)
        }
        "claim" => {
            let c = ClaimArgs::parse(rest, "claim")?;
            let out = tools::cli_claim(
                c.root,
                &c.agent,
                &c.files,
                c.purpose.as_deref(),
                c.intent.as_deref(),
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
            );
            Ok(exit_for(&out))
        }
        "release" => {
            let c = ClaimArgs::parse(rest, "release")?;
            let out = tools::cli_release(c.root, &c.agent, &c.files);
            println!(
                "{}",
                serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
            );
            Ok(exit_for(&out))
        }
        "mutation-files" => {
            let m = MutationArgs::parse(rest)?;
            let cwd = m.cwd.as_deref().unwrap_or(m.root.as_path());
            let files: Vec<String> = bash_parse::workspace_targets(&m.command, cwd, &m.root)
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&files).map_err(|e| e.to_string())?
            );
            Ok(ExitCode::SUCCESS)
        }
        "rejoin" => {
            let c = RejoinArgs::parse(rest)?;
            let out = tools::cli_rejoin(c.root, &c.agent, c.files);
            println!(
                "{}",
                serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
            );
            Ok(exit_for(&out))
        }
        "help" | "--help" | "-h" => {
            print!("{}", usage());
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command '{other}'\n{}", usage())),
    }
}

fn exit_for(out: &serde_json::Value) -> ExitCode {
    if out
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn usage() -> String {
    "zoanthrope — multi-agent swarm coordinator\n\
     \n\
     usage:\n\
     \x20 zoanthrope mcp [--workspace DIR]        stdio MCP server\n\
     \x20 zoanthrope status [--workspace DIR]     text summary of claims/agents/posts\n\
     \x20 zoanthrope claim --files a.rs,b.rs --agent X [--purpose P] [--intent read|edit] [--workspace DIR]\n\
     \x20 zoanthrope release --files a.rs,b.rs --agent X [--workspace DIR]\n\
     \x20 zoanthrope rejoin --agent X [--files a.rs,b.rs] [--workspace DIR]\n\
     \x20 zoanthrope mutation-files --command CMD [--cwd DIR] [--workspace DIR]\n\
     \x20   heuristic parse of a shell command; prints the JSON array of\n\
     \x20   workspace files it would mutate (redirects, sed -i, tee, cp/mv,\n\
     \x20   python/perl write sinks)\n"
        .to_string()
}

/// `--workspace DIR` option, shared by every subcommand. When absent,
/// the workspace root is discovered by walking up from the current
/// directory looking for `.git` (falling back to the cwd).
struct WorkspaceArgs {
    root: PathBuf,
}

impl WorkspaceArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut workspace: Option<PathBuf> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--workspace" | "-w" => {
                    let v = args
                        .get(i + 1)
                        .ok_or_else(|| "--workspace needs a directory".to_string())?;
                    workspace = Some(PathBuf::from(v));
                    i += 2;
                }
                other => return Err(format!("unexpected argument '{other}'\n{}", usage())),
            }
        }
        let root = match workspace {
            Some(dir) => dir,
            None => state::discover_root(&std::env::current_dir().map_err(|e| e.to_string())?),
        };
        Ok(Self { root })
    }
}

struct MutationArgs {
    root: PathBuf,
    cwd: Option<PathBuf>,
    command: String,
}

impl MutationArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut command: Option<String> = None;
        let mut cwd: Option<PathBuf> = None;
        let mut workspace: Option<PathBuf> = None;
        let mut i = 0;
        while i < args.len() {
            let (flag, value) = {
                let a = args[i].clone();
                if let Some((f, v)) = a.split_once('=') {
                    (f.to_string(), Some(v.to_string()))
                } else if a.starts_with("--") {
                    let v = args.get(i + 1).cloned();
                    (a, v)
                } else {
                    return Err(format!("unexpected argument '{a}'\n{}", usage()));
                }
            };
            let take = |v: Option<String>, name: &str| -> Result<String, String> {
                v.ok_or_else(|| format!("mutation-files: {flag} needs a value"))
                    .and_then(|v| {
                        if v.starts_with("--") {
                            Err(format!("mutation-files: {name} needs a value"))
                        } else {
                            Ok(v)
                        }
                    })
            };
            match flag.as_str() {
                "--command" | "-c" => {
                    command = Some(take(value, "--command")?);
                    i += 2;
                }
                "--cwd" => {
                    cwd = Some(PathBuf::from(take(value, "--cwd")?));
                    i += 2;
                }
                "--workspace" | "-w" => {
                    workspace = Some(PathBuf::from(take(value, "--workspace")?));
                    i += 2;
                }
                other => return Err(format!("unexpected argument '{other}'\n{}", usage())),
            }
        }
        let command = command.ok_or_else(|| "mutation-files: --command is required".to_string())?;
        let root = match workspace {
            Some(dir) => dir,
            None => state::discover_root(&std::env::current_dir().map_err(|e| e.to_string())?),
        };
        Ok(Self { root, cwd, command })
    }
}

struct ClaimArgs {
    root: PathBuf,
    agent: String,
    files: Vec<String>,
    purpose: Option<String>,
    intent: Option<String>,
}

impl ClaimArgs {
    fn parse(args: &[String], cmd: &str) -> Result<Self, String> {
        let mut files: Option<String> = None;
        let mut agent: Option<String> = None;
        let mut purpose: Option<String> = None;
        let mut intent: Option<String> = None;
        let mut workspace: Option<PathBuf> = None;
        let mut i = 0;
        while i < args.len() {
            let (flag, value) = {
                let a = args[i].clone();
                if let Some((f, v)) = a.split_once('=') {
                    (f.to_string(), Some(v.to_string()))
                } else if a.starts_with("--") {
                    let v = args.get(i + 1).cloned();
                    (a, v)
                } else {
                    return Err(format!("unexpected argument '{a}'\n{}", usage()));
                }
            };
            let take = |v: Option<String>, name: &str| -> Result<String, String> {
                v.ok_or_else(|| format!("{cmd}: {flag} needs a value"))
                    .and_then(|v| {
                        if v.starts_with("--") {
                            Err(format!("{cmd}: {name} needs a value"))
                        } else {
                            Ok(v)
                        }
                    })
            };
            match flag.as_str() {
                "--files" | "-f" => {
                    files = Some(take(value, "--files")?);
                    i += 2;
                }
                "--agent" | "-a" => {
                    agent = Some(take(value, "--agent")?);
                    i += 2;
                }
                "--purpose" | "-p" => {
                    purpose = Some(take(value, "--purpose")?);
                    i += 2;
                }
                "--intent" | "-i" => {
                    intent = Some(take(value, "--intent")?);
                    i += 2;
                }
                "--workspace" | "-w" => {
                    workspace = Some(PathBuf::from(take(value, "--workspace")?));
                    i += 2;
                }
                other => return Err(format!("unexpected argument '{other}'\n{}", usage())),
            }
        }
        let files_raw = files.ok_or_else(|| format!("{cmd}: --files is required"))?;
        let files: Vec<String> = files_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if files.is_empty() {
            return Err(format!("{cmd}: --files is empty"));
        }
        let agent = agent.ok_or_else(|| format!("{cmd}: --agent is required"))?;
        let root = match workspace {
            Some(dir) => dir,
            None => state::discover_root(&std::env::current_dir().map_err(|e| e.to_string())?),
        };
        Ok(Self {
            root,
            agent,
            files,
            purpose,
            intent,
        })
    }
}

struct RejoinArgs {
    root: PathBuf,
    agent: String,
    files: Option<Vec<String>>,
}

impl RejoinArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut files: Option<String> = None;
        let mut agent: Option<String> = None;
        let mut workspace: Option<PathBuf> = None;
        let mut i = 0;
        while i < args.len() {
            let (flag, value) = {
                let a = args[i].clone();
                if let Some((f, v)) = a.split_once('=') {
                    (f.to_string(), Some(v.to_string()))
                } else if a.starts_with("--") {
                    let v = args.get(i + 1).cloned();
                    (a, v)
                } else {
                    return Err(format!("unexpected argument '{a}'\n{}", usage()));
                }
            };
            let take = |v: Option<String>, name: &str| -> Result<String, String> {
                v.ok_or_else(|| format!("rejoin: {flag} needs a value"))
                    .and_then(|v| {
                        if v.starts_with("--") {
                            Err(format!("rejoin: {name} needs a value"))
                        } else {
                            Ok(v)
                        }
                    })
            };
            match flag.as_str() {
                "--files" | "-f" => {
                    files = Some(take(value, "--files")?);
                    i += 2;
                }
                "--agent" | "-a" => {
                    agent = Some(take(value, "--agent")?);
                    i += 2;
                }
                "--workspace" | "-w" => {
                    workspace = Some(PathBuf::from(take(value, "--workspace")?));
                    i += 2;
                }
                other => return Err(format!("unexpected argument '{other}'\n{}", usage())),
            }
        }
        let agent = agent.ok_or_else(|| "rejoin: --agent is required".to_string())?;
        let files = match files {
            Some(raw) => {
                let fs: Vec<String> = raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if fs.is_empty() {
                    return Err("rejoin: --files is empty".into());
                }
                Some(fs)
            }
            None => None,
        };
        let root = match workspace {
            Some(dir) => dir,
            None => state::discover_root(&std::env::current_dir().map_err(|e| e.to_string())?),
        };
        Ok(Self { root, agent, files })
    }
}
