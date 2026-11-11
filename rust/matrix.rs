// Matrix client: the access token comes from the Keychain on every call, so a
// command is stateless and never relies on a login step.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{process::Command, time::Duration};

use crate::hub::{TOKEN_SERVICE, homeserver, mxid};

pub fn read_token() -> Result<String> {
    // Keychain first, then ~/.config/hub/matrix-token (mode 600) for headless use.
    if let Ok(output) = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-a", &mxid(), "-s", TOKEN_SERVICE, "-w"])
        .output()
    {
        if output.status.success() {
            let token = String::from_utf8(output.stdout)
                .context("token is not UTF-8")?
                .trim()
                .to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
    }
    read_token_file()
}

fn read_token_file() -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let home = std::env::var("HOME").context("HOME is not set")?;
    let path = std::path::Path::new(&home).join(".config/hub/matrix-token");
    let meta = std::fs::metadata(&path).with_context(|| {
        format!("no Matrix access token in the Keychain (service {TOKEN_SERVICE}) or at {}", path.display())
    })?;
    if meta.permissions().mode() & 0o077 != 0 {
        bail!("{} must be mode 600", path.display());
    }
    let token = std::fs::read_to_string(&path)
        .context("read token file")?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("token file {} is empty", path.display());
    }
    Ok(token)
}

pub fn get(path: &str) -> Result<Value> {
    request("GET", path, None)
}

pub fn post(path: &str, body: &Value) -> Result<Value> {
    request("POST", path, Some(body))
}

pub fn put(path: &str, body: &Value) -> Result<Value> {
    request("PUT", path, Some(body))
}

fn request(method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
    let token = read_token()?;
    let url = format!("{}{}", homeserver(), path);
    let auth = format!("Bearer {token}");
    let timeout = Duration::from_secs(30);
    let result = match (method, body) {
        ("GET", None) => ureq::get(&url)
            .header("Authorization", &auth)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call(),
        ("GET", Some(_)) => bail!("GET does not take a body"),
        ("POST", Some(value)) => ureq::post(&url)
            .header("Authorization", &auth)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send_json(value),
        ("PUT", Some(value)) => ureq::put(&url)
            .header("Authorization", &auth)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send_json(value),
        ("POST", None) => ureq::post(&url)
            .header("Authorization", &auth)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send_empty(),
        ("PUT", None) => ureq::put(&url)
            .header("Authorization", &auth)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send_empty(),
        (other, _) => bail!("unsupported HTTP method {other}"),
    };
    match result {
        Ok(mut response) => Ok(response
            .body_mut()
            .read_json::<Value>()
            .unwrap_or(Value::Null)),
        Err(ureq::Error::StatusCode(code)) => bail!("Matrix {code}"),
        Err(error) => bail!("{error}"),
    }
}
