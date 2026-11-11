// Bridge catalogue, status rows and provisioning access. This mirrors the
// TypeScript hub core so `connections` reports exactly what it used to.
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

pub const TOKEN_SERVICE: &str = "Matrix Hub Codex Token";
pub const CONNECTED: &str = "●";
pub const ISSUE: &str = "○";

pub fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn mxid() -> String {
    env::var("MATRIX_USER_ID").unwrap_or_else(|_| "@user:localhost".to_string())
}

pub fn homeserver() -> String {
    env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "http://127.0.0.1:8008".to_string())
}

pub fn hub_root() -> PathBuf {
    env::var_os("MATRIX_HUB_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("Library/Application Support/matrix-hub"))
}

pub fn bridge_root() -> PathBuf {
    hub_root().join("bridges")
}

/// Percent-encode a path segment the way `encodeURIComponent` does, so room IDs
/// (`!id:server`) survive as a single Matrix path segment.
pub fn enc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Clone, Copy)]
pub struct BridgeSpec {
    pub platform: &'static str,
    pub label: &'static str,
    pub port: Option<u16>,
    pub process: Option<&'static str>,
}

/// Every bridge the hub can host, in catalogue order. Labels that a bridge
/// reports differently are normalized in `label_for`.
pub const BRIDGES: &[BridgeSpec] = &[
    BridgeSpec {
        platform: "whatsapp",
        label: "WhatsApp",
        port: Some(29318),
        process: None,
    },
    BridgeSpec {
        platform: "telegram",
        label: "Telegram",
        port: Some(29317),
        process: None,
    },
    BridgeSpec {
        platform: "discord",
        label: "Discord",
        port: Some(29334),
        process: None,
    },
    BridgeSpec {
        platform: "messenger",
        label: "Messenger",
        port: Some(29319),
        process: None,
    },
    BridgeSpec {
        platform: "instagram",
        label: "Instagram",
        port: Some(29320),
        process: None,
    },
    BridgeSpec {
        platform: "linkedin",
        label: "LinkedIn",
        port: Some(29341),
        process: None,
    },
    BridgeSpec {
        platform: "imessage",
        label: "iMessage",
        port: None,
        process: Some("mautrix-imessage"),
    },
    BridgeSpec {
        platform: "wechat",
        label: "WeChat",
        port: None,
        process: Some("matrix-bridge.mjs"),
    },
    BridgeSpec {
        platform: "signal",
        label: "Signal",
        port: Some(29328),
        process: None,
    },
    BridgeSpec {
        platform: "googlechat",
        label: "Google Chat",
        port: Some(29338),
        process: None,
    },
    BridgeSpec {
        platform: "slack",
        label: "Slack",
        port: Some(29335),
        process: None,
    },
    BridgeSpec {
        platform: "bluesky",
        label: "Bluesky",
        port: Some(29346),
        process: None,
    },
    BridgeSpec {
        platform: "twitter",
        label: "X (Twitter)",
        port: Some(29327),
        process: None,
    },
    BridgeSpec {
        platform: "reddit",
        label: "Reddit",
        port: Some(29336),
        process: None,
    },
    BridgeSpec {
        platform: "gmessages",
        label: "Google Messages",
        port: Some(29345),
        process: None,
    },
];

const TRANSIENT_STATES: [&str; 2] = ["CONNECTING", "TRANSIENT_DISCONNECT"];

#[derive(Serialize)]
pub struct BridgeRow {
    pub platform: String,
    pub label: String,
    pub account: String,
    pub note: String,
    pub up: u64,
    pub accounts: u64,
    pub transient: u64,
    pub installed: bool,
    pub reachable: bool,
    pub state: String,
}

pub fn is_installed(platform: &str) -> bool {
    bridge_root()
        .join("registrations")
        .join(format!("{platform}.yaml"))
        .is_file()
}

/// The marker a text surface shows: filled when the bridge is working.
pub fn marker_of(row: &BridgeRow) -> &'static str {
    match row.state.as_str() {
        "missing" | "empty" | "connecting" => ISSUE,
        _ => CONNECTED,
    }
}

pub fn summary_line(row: &BridgeRow) -> String {
    let extra = if row.state != "ok" && !row.note.is_empty() {
        format!("  ·  {}", row.note)
    } else {
        String::new()
    };
    format!(
        "{} {:<20} {}{}",
        marker_of(row),
        row.label,
        row.account,
        extra
    )
}

pub fn state_label(state: &str) -> &str {
    match state {
        "CONNECTED" => "connected",
        "CONNECTING" => "connecting",
        "TRANSIENT_DISCONNECT" => "reconnecting",
        "BAD_CREDENTIALS" => "needs re-login",
        "UNKNOWN_ERROR" => "error",
        "" => "?",
        other => other,
    }
}

fn bridge_state(row: &BridgeRow) -> &'static str {
    if !row.installed {
        "missing"
    } else if !row.reachable {
        "down"
    } else if row.accounts == 0 {
        "empty"
    } else if row.up == row.accounts {
        "ok"
    } else if row.up + row.transient == row.accounts {
        "connecting"
    } else {
        "partial"
    }
}

fn state_order(state: &str) -> u8 {
    match state {
        "partial" | "down" => 0,
        "ok" => 1,
        "connecting" => 2,
        "empty" => 3,
        _ => 4,
    }
}

pub fn sort_rows(mut rows: Vec<BridgeRow>) -> Vec<BridgeRow> {
    rows.sort_by_key(|row| state_order(&row.state));
    rows
}

fn label_for(platform: &str, reported: Option<&str>, fallback: &str) -> String {
    if let Some(fixed) = match platform {
        "messenger" => Some("Messenger"),
        "instagram" => Some("Instagram"),
        _ => None,
    } {
        return fixed.to_string();
    }
    reported
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn absent_row(spec: &BridgeSpec, installed: bool, note: &str) -> BridgeRow {
    let mut row = BridgeRow {
        platform: spec.platform.to_string(),
        label: spec.label.to_string(),
        account: "—".to_string(),
        note: note.to_string(),
        up: 0,
        accounts: 0,
        transient: 0,
        installed,
        reachable: false,
        state: String::new(),
    };
    row.state = bridge_state(&row).to_string();
    row
}

fn row_for(spec: &BridgeSpec, info: &Value) -> BridgeRow {
    let logins = info
        .get("logins")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let label = label_for(
        spec.platform,
        info.pointer("/network/displayname").and_then(Value::as_str),
        spec.label,
    );
    if logins.is_empty() {
        let mut row = BridgeRow {
            platform: spec.platform.to_string(),
            label,
            account: "no account".to_string(),
            note: "not connected".to_string(),
            up: 0,
            accounts: 0,
            transient: 0,
            installed: true,
            reachable: true,
            state: String::new(),
        };
        row.state = bridge_state(&row).to_string();
        return row;
    }
    let states: Vec<&str> = logins
        .iter()
        .map(|login| {
            login
                .pointer("/state/state_event")
                .and_then(Value::as_str)
                .unwrap_or("")
        })
        .collect();
    let connected = states.iter().filter(|state| **state == "CONNECTED").count() as u64;
    let transient = states
        .iter()
        .filter(|state| TRANSIENT_STATES.contains(state))
        .count() as u64;
    let account = logins
        .iter()
        .map(|login| {
            login
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| login.get("id").and_then(Value::as_str))
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let reason = logins
        .iter()
        .find(|login| {
            login.pointer("/state/state_event").and_then(Value::as_str) != Some("CONNECTED")
        })
        .and_then(|login| login.pointer("/state/message").and_then(Value::as_str))
        .map(str::to_owned);
    let note = if connected as usize == states.len() {
        "connected".to_string()
    } else {
        let listed = states
            .iter()
            .filter(|state| **state != "CONNECTED")
            .map(|state| state_label(state).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{connected}/{} connected · {}",
            states.len(),
            reason.unwrap_or(listed)
        )
    };
    let mut row = BridgeRow {
        platform: spec.platform.to_string(),
        label,
        account,
        note,
        up: connected,
        accounts: states.len() as u64,
        transient,
        installed: true,
        reachable: true,
        state: String::new(),
    };
    row.state = bridge_state(&row).to_string();
    row
}

fn process_running(pattern: &str) -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-f", pattern])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Reads `provisioning.shared_secret` from a bridge's config.yaml without a full
/// YAML parser: the block is top level in most bridges and nested under
/// `appservice:` in others, so it is matched at any indentation.
pub fn shared_secret(platform: &str) -> String {
    let path = bridge_root().join(platform).join("config.yaml");
    let Ok(text) = fs::read_to_string(&path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|line| line.trim() == "provisioning:") else {
        return String::new();
    };
    let indent = lines[start].len() - lines[start].trim_start().len();
    for line in lines.iter().skip(start + 1).take(12) {
        if let Some(value) = line.trim().strip_prefix("shared_secret:") {
            let value = value.trim();
            return if matches!(value, "generate" | "disable" | "null" | "") {
                String::new()
            } else {
                value.to_string()
            };
        }
        if !line.trim().is_empty() && (line.len() - line.trim_start().len()) <= indent {
            break;
        }
    }
    String::new()
}

fn get_json(url: &str, secret: &str) -> Result<Value, String> {
    match ureq::get(url)
        .header("Authorization", &format!("Bearer {secret}"))
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .call()
    {
        Ok(mut response) => {
            let status = response.status().as_u16();
            if (400..600).contains(&status) {
                let body = response.body_mut().read_to_string().unwrap_or_default();
                Err(format!("{status} {body}"))
            } else {
                response
                    .body_mut()
                    .read_json::<Value>()
                    .map_err(|error| error.to_string())
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

fn provision(platform: &str, path: &str) -> Result<Value, String> {
    let spec = BRIDGES
        .iter()
        .find(|spec| spec.platform == platform)
        .ok_or_else(|| format!("unknown bridge {platform}"))?;
    let port = spec
        .port
        .ok_or_else(|| format!("unknown bridge {platform}"))?;
    let secret = shared_secret(platform);
    if secret.is_empty() {
        return Err("no provisioning secret in config.yaml".to_string());
    }
    let separator = if path.contains('?') { '&' } else { '?' };
    let url = format!(
        "http://127.0.0.1:{port}/_matrix/provision/v3{path}{separator}user_id={}",
        enc(&mxid())
    );
    get_json(&url, &secret)
}

fn discord_status() -> Result<Value, String> {
    let port = BRIDGES
        .iter()
        .find(|spec| spec.platform == "discord")
        .and_then(|spec| spec.port)
        .unwrap_or(29334);
    let secret = shared_secret("discord");
    if secret.is_empty() {
        return Err("no provisioning secret in config.yaml".to_string());
    }
    let url = format!(
        "http://127.0.0.1:{port}/_matrix/provision/v1/ping?user_id={}",
        enc(&mxid())
    );
    get_json(&url, &secret)
}

/// A port that answers at all (even a 404) counts as reachable, matching fetch.
fn port_answers(port: u16) -> bool {
    match ureq::get(&format!("http://127.0.0.1:{port}/"))
        .config()
        .timeout_global(Some(Duration::from_millis(1500)))
        .build()
        .call()
    {
        Ok(_) | Err(ureq::Error::StatusCode(_)) => true,
        Err(_) => false,
    }
}

fn strip_status(message: &str) -> String {
    message
        .trim_start_matches(|c: char| c.is_ascii_digit() || c.is_whitespace())
        .to_string()
}

fn clipped(message: &str, width: usize) -> String {
    message.chars().take(width).collect()
}

pub fn rows() -> Vec<BridgeRow> {
    BRIDGES
        .iter()
        .map(|spec| {
            if !is_installed(spec.platform) {
                return absent_row(spec, false, "not installed");
            }
            if let Some(pattern) = spec.process {
                let running = process_running(pattern);
                let mut row =
                    absent_row(spec, true, if running { "running" } else { "not running" });
                row.account = "bridge".to_string();
                row.up = u64::from(running);
                row.accounts = 1;
                row.reachable = running;
                row.state = bridge_state(&row).to_string();
                return row;
            }
            if spec.platform == "discord" {
                return match discord_status() {
                    Ok(ping) => {
                        let status = ping.get("Discord").cloned().unwrap_or_else(|| json!({}));
                        let logged_in = status
                            .get("logged_in")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let connected = status
                            .get("connected")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let note = if connected {
                            "connected"
                        } else if logged_in {
                            "reconnecting"
                        } else {
                            "not logged in"
                        };
                        let mut row = absent_row(spec, true, note);
                        row.account = if logged_in {
                            status
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .unwrap_or_else(|| "discord".to_string())
                        } else {
                            "no account".to_string()
                        };
                        row.up = u64::from(connected);
                        row.accounts = u64::from(logged_in);
                        row.transient = u64::from(logged_in && !connected);
                        row.reachable = true;
                        row.state = bridge_state(&row).to_string();
                        row
                    }
                    Err(message) => absent_row(spec, true, &clipped(&strip_status(&message), 60)),
                };
            }
            match provision(spec.platform, "/whoami") {
                Ok(info) => row_for(spec, &info),
                Err(message) => {
                    let message = strip_status(&message);
                    let lower = message.to_lowercase();
                    let no_api = [
                        "no provisioning secret",
                        "unknown bridge",
                        "404",
                        "not found",
                        "disabled",
                    ]
                    .iter()
                    .any(|needle| lower.contains(needle));
                    if spec.port.is_some() && no_api {
                        let up = port_answers(spec.port.unwrap());
                        let mut row = absent_row(
                            spec,
                            true,
                            if up {
                                "running · no provisioning API"
                            } else {
                                "not answering"
                            },
                        );
                        row.reachable = up;
                        row.state = bridge_state(&row).to_string();
                        return row;
                    }
                    absent_row(spec, true, &clipped(&message, 60))
                }
            }
        })
        .collect()
}
