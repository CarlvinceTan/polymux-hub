// hub: a stateless CLI over the private Matrix homeserver. Each invocation
// reads the token from the Keychain, does one thing and exits.
mod hub;
mod matrix;
mod ops;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Parser)]
#[command(
    name = "hub",
    about = "Stateless messaging over the private Matrix homeserver",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: HubCommand,
}

#[derive(Subcommand)]
enum HubCommand {
    /// Platform and account status.
    Connections {
        #[arg(long)]
        json: bool,
    },
    /// Unread conversations, without marking them read.
    Unread {
        #[arg(long)]
        json: bool,
    },
    /// Joined conversations, optionally filtered by name.
    Rooms {
        query: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Recent messages of one conversation, newest last.
    Messages {
        room_id: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Mark one conversation read; requires the user's explicit request.
    MarkRead {
        room_id: String,
        #[arg(long = "requested-by")]
        requested_by: String,
        #[arg(long = "event-id")]
        event_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send exact text; requires the user's approval of this recipient and body.
    Send {
        room_id: String,
        #[arg(long = "approved-by")]
        approved_by: String,
        #[arg(long = "reply-to")]
        reply_to: Option<String>,
        /// Message body; read from stdin when omitted.
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Link this skill into each detected agent.
    Skills {
        #[arg(default_value = "status")]
        action: String,
    },
}

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();
    if matches!(
        argv.first().map(String::as_str),
        Some("status" | "check" | "update" | "install")
    ) {
        std::process::exit(forward_maintenance(&argv));
    }
    if let Err(error) = run() {
        eprintln!("hub: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        HubCommand::Connections { json } => {
            let list = hub::sort_rows(hub::rows());
            let connected = list.iter().filter(|row| row.up > 0).count();
            if json {
                print_json(
                    &serde_json::json!({ "connected": connected, "total": list.len(), "bridges": list }),
                );
            } else {
                println!("connected {connected}/{}", list.len());
                for row in &list {
                    println!("{}", hub::summary_line(row));
                }
            }
        }
        HubCommand::Unread { json } => {
            let rooms = ops::unread()?;
            emit_lines(
                &rooms,
                json,
                |room| {
                    format!(
                        "{:>2}  {}\t{}",
                        room["count"],
                        room["name"].as_str().unwrap_or(""),
                        room["room_id"].as_str().unwrap_or("")
                    )
                },
                "Nothing unread.",
            );
        }
        HubCommand::Rooms { query, json } => {
            let rooms = ops::rooms(query.as_deref())?;
            emit_lines(
                &rooms,
                json,
                |room| {
                    format!(
                        "{}\t{}",
                        room["name"].as_str().unwrap_or(""),
                        room["room_id"].as_str().unwrap_or("")
                    )
                },
                "No rooms matched.",
            );
        }
        HubCommand::Messages {
            room_id,
            limit,
            json,
        } => {
            let list = ops::messages(&room_id, Some(limit))?;
            emit_lines(
                &list,
                json,
                |message| {
                    format!(
                        "{}  {}  {}  {}",
                        message["timestamp"].as_str().unwrap_or(""),
                        message["event_id"].as_str().unwrap_or(""),
                        message["sender"].as_str().unwrap_or(""),
                        message["body"].as_str().unwrap_or("")
                    )
                },
                "No messages.",
            );
        }
        HubCommand::MarkRead {
            room_id,
            requested_by,
            event_id,
            json,
        } => {
            let result = ops::mark_read(&room_id, event_id.as_deref(), true, &requested_by)?;
            emit(
                &result,
                &format!(
                    "marked {} read up to {}",
                    result["room_id"],
                    result["event_id"].as_str().unwrap_or("")
                ),
                json,
            );
        }
        HubCommand::Send {
            room_id,
            approved_by,
            reply_to,
            text,
            json,
        } => {
            let body = match text {
                Some(text) => text,
                None => read_stdin()?,
            };
            if body.is_empty() {
                bail!("refusing to send an empty message");
            }
            let result = ops::send(&room_id, &body, true, &approved_by, reply_to.as_deref())?;
            emit(
                &result,
                &format!(
                    "sent to {} (event {})",
                    result["room_id"],
                    result["event_id"].as_str().unwrap_or("")
                ),
                json,
            );
        }
        HubCommand::Skills { action } => skills(&action)?,
    }
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("read stdin")?;
    Ok(input.strip_suffix('\n').unwrap_or(&input).to_string())
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("JSON serialization")
    );
}

fn emit(value: &Value, text: &str, json: bool) {
    if json {
        print_json(value);
    } else {
        println!("{text}");
    }
}

fn emit_lines(value: &Value, json: bool, line: impl Fn(&Value) -> String, empty: &str) {
    let items = value.as_array().cloned().unwrap_or_default();
    if json {
        print_json(value);
    } else if items.is_empty() {
        println!("{empty}");
    } else {
        for item in &items {
            println!("{}", line(item));
        }
    }
}

/// Resolve the repo root at runtime. `CARGO_MANIFEST_DIR` is baked in at
/// compile time, so it goes stale when the checkout is moved; prefer the env
/// var set by bin/hub, then the running executable's own location.
fn repo_root() -> PathBuf {
    if let Some(root) = env::var_os("HUB_REPO_ROOT") {
        return PathBuf::from(root);
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            if root.join("Cargo.toml").exists() && root.join("skills/hub").is_dir() {
                return root.to_path_buf();
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Bridge maintenance is still the TypeScript implementation; forward to it so
/// `hub status|check|update|install` keep working from the same command.
fn forward_maintenance(args: &[String]) -> i32 {
    let script = repo_root().join("bin/hub-maintenance");
    match Command::new(&script).args(args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("hub: cannot run bridge maintenance ({error})");
            1
        }
    }
}

/// True when `link` is a symlink this tool made in a previous run, judged by
/// the pack's own `<...>/skills/hub` shape. A moved checkout leaves these
/// dangling; they are ours to repair, unlike a foreign link or real file.
fn is_prior_pack_link(link: &Path) -> bool {
    fs::read_link(link)
        .ok()
        .is_some_and(|target| {
            target.file_name().and_then(|n| n.to_str()) == Some("hub")
                && target
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    == Some("skills")
        })
}

fn skills(action: &str) -> Result<()> {
    if !["status", "install", "update", "uninstall"].contains(&action) {
        bail!("usage: hub skills <status|install|update|uninstall>");
    }
    let root = repo_root();
    let pack = root.join("skills/hub");
    let home = hub::home();
    let agents = [
        ("Claude Code", home.join(".claude/skills")),
        ("Codex", home.join(".agents/skills")),
        ("Prime Agent", home.join(".prime/agent/skills")),
        ("OpenClaw", home.join(".openclaw/skills")),
        ("OpenCode", home.join(".config/opencode/skills")),
        ("Antigravity", home.join(".gemini/skills")),
        ("Hermes", home.join(".hermes/skills")),
    ];
    println!("Local skill pack: {}", pack.display());
    let uninstall = action == "uninstall";
    let mutate = action != "status";
    let mut failed = false;
    for (agent, dir) in agents {
        let link = dir.join("hub");
        if !dir.parent().is_some_and(Path::exists) {
            println!("  {agent} — agent dir not present: {}", dir.display());
            continue;
        }
        let state = fs::symlink_metadata(&link).ok();
        let correct = state
            .as_ref()
            .is_some_and(|meta| meta.file_type().is_symlink())
            && fs::read_link(&link)
                .ok()
                .and_then(|target| fs::canonicalize(link.parent().unwrap().join(target)).ok())
                == fs::canonicalize(&pack).ok();
        if !mutate {
            let detail = if correct {
                format!("{} → {}", link.display(), pack.display())
            } else {
                link.display().to_string()
            };
            let label = if correct {
                "linked"
            } else if state.is_some() {
                "occupied/foreign"
            } else {
                "not linked"
            };
            println!("  {agent} — {label}: {detail}");
            continue;
        }
        if uninstall {
            if correct || is_prior_pack_link(&link) {
                fs::remove_file(&link)?;
                println!("  {agent} — unlinked: {}", link.display());
            } else if state.is_none() {
                println!("  {agent} — nothing to remove: {}", link.display());
            } else {
                eprintln!("  {agent} — left alone: {}", link.display());
                failed = true;
            }
        } else if correct {
            println!("  {agent} — already linked: {}", link.display());
        } else if state.is_some() && is_prior_pack_link(&link) {
            fs::remove_file(&link)?;
            std::os::unix::fs::symlink(&pack, &link)?;
            println!("  {agent} — relinked: {}", link.display());
        } else if state.is_some() {
            eprintln!("  {agent} — left alone: {}", link.display());
            failed = true;
        } else {
            fs::create_dir_all(&dir)?;
            std::os::unix::fs::symlink(&pack, &link)?;
            println!("  {agent} — linked: {}", link.display());
        }
    }
    if failed {
        bail!("one or more skill links were left alone");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_pack_links_are_recognized_but_foreign_ones_are_not() {
        let dir = std::env::temp_dir().join(format!("hub-prior-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let prior = dir.join("hub");
        std::os::unix::fs::symlink("/moved/away/skills/hub", &prior).unwrap();
        assert!(is_prior_pack_link(&prior));
        let foreign = dir.join("foreign");
        std::os::unix::fs::symlink("/etc", &foreign).unwrap();
        assert!(!is_prior_pack_link(&foreign));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn encodes_room_ids_like_encode_uri_component() {
        assert_eq!(hub::enc("!a:b"), "!a%3Ab");
        assert_eq!(hub::enc("abc-_.!~*'()XYZ"), "abc-_.!~*'()XYZ");
    }

    #[test]
    fn state_labels_fall_back_to_the_raw_state() {
        assert_eq!(hub::state_label("BAD_CREDENTIALS"), "needs re-login");
        assert_eq!(hub::state_label("WHATEVER"), "WHATEVER");
        assert_eq!(hub::state_label(""), "?");
    }
}
