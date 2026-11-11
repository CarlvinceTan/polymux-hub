// The six messaging operations as structured data. Reading preserves unread
// state; marking read and sending are gated by explicit-approval arguments.
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    hub::{enc, mxid},
    matrix,
};

fn room_name(room_id: &str) -> String {
    let path = format!(
        "/_matrix/client/v3/rooms/{}/state/m.room.name",
        enc(room_id)
    );
    match matrix::get(&path) {
        Ok(state) => state
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(room_id)
            .to_string(),
        Err(_) => room_id.to_string(),
    }
}

fn title(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

pub fn unread() -> Result<Value> {
    use std::collections::BTreeMap;
    let result = matrix::get("/_matrix/client/v3/notifications?limit=100")?;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    if let Some(notifications) = result.get("notifications").and_then(Value::as_array) {
        for notification in notifications {
            let event = notification
                .get("event")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let is_read = notification
                .get("read")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let is_message = event.get("type").and_then(Value::as_str) == Some("m.room.message");
            let from_self = event.get("sender").and_then(Value::as_str) == Some(mxid().as_str());
            if !is_read && is_message && !from_self {
                if let Some(room_id) = notification.get("room_id").and_then(Value::as_str) {
                    *counts.entry(room_id.to_string()).or_default() += 1;
                }
            }
        }
    }
    let rooms: Vec<Value> = counts
        .into_iter()
        .map(|(room_id, count)| json!({ "room_id": room_id, "name": room_name(&room_id), "count": count }))
        .collect();
    Ok(json!(rooms))
}

pub fn rooms(query: Option<&str>) -> Result<Value> {
    let joined = matrix::get("/_matrix/client/v3/joined_rooms")?;
    let ids: Vec<String> = joined
        .get("joined_rooms")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .take(120)
                .collect()
        })
        .unwrap_or_default();
    let needle = query.unwrap_or("").trim().to_lowercase();
    let rooms: Vec<Value> = ids
        .into_iter()
        .map(|room_id| {
            let name = room_name(&room_id);
            json!({ "room_id": room_id, "name": name })
        })
        .filter(|room| {
            needle.is_empty()
                || room["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&needle)
        })
        .collect();
    Ok(json!(rooms))
}

pub fn messages(room_id: &str, limit: Option<u32>) -> Result<Value> {
    let size = limit.unwrap_or(20).clamp(1, 100);
    let path = format!(
        "/_matrix/client/v3/rooms/{}/messages?dir=b&limit={size}",
        enc(room_id)
    );
    let result = matrix::get(&path)?;
    let mut list: Vec<Value> = result
        .get("chunk")
        .and_then(Value::as_array)
        .map(|chunk| {
            chunk
                .iter()
                .filter(|event| event.get("type").and_then(Value::as_str) == Some("m.room.message"))
                .map(|event| {
                    let ts = event.get("origin_server_ts").and_then(Value::as_i64).unwrap_or(0);
                    let timestamp = chrono::DateTime::from_timestamp_millis(ts)
                        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                        .unwrap_or_default();
                    json!({
                        "event_id": event.get("event_id").cloned().unwrap_or(Value::Null),
                        "timestamp": timestamp,
                        "sender": event.get("sender").and_then(Value::as_str).unwrap_or(""),
                        "body": event.pointer("/content/body").and_then(Value::as_str).unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    list.reverse();
    Ok(json!(list))
}

fn latest_event_id(room_id: &str) -> Result<String> {
    let path = format!(
        "/_matrix/client/v3/rooms/{}/messages?dir=b&limit=1",
        enc(room_id)
    );
    let result = matrix::get(&path)?;
    Ok(result
        .get("chunk")
        .and_then(Value::as_array)
        .and_then(|chunk| chunk.first())
        .and_then(|event| event.get("event_id"))
        .map(title)
        .unwrap_or_default())
}

pub fn mark_read(
    room_id: &str,
    event_id: Option<&str>,
    explicit: bool,
    reference: &str,
) -> Result<Value> {
    if !explicit || reference.trim().is_empty() {
        bail!("refused: the user must have explicitly asked for this conversation to be read");
    }
    let target = match event_id.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => latest_event_id(room_id)?,
    };
    if target.is_empty() {
        bail!("no event to mark read in {room_id}");
    }
    let body = json!({ "m.read": target, "m.fully_read": target });
    matrix::post(
        &format!("/_matrix/client/v3/rooms/{}/read_markers", enc(room_id)),
        &body,
    )?;
    Ok(json!({ "room_id": room_id, "event_id": target }))
}

pub fn send(
    room_id: &str,
    text: &str,
    approved: bool,
    reference: &str,
    reply_to: Option<&str>,
) -> Result<Value> {
    if !approved || reference.trim().is_empty() {
        bail!("refused: exact recipient/payload approval is required");
    }
    let mut body = json!({ "msgtype": "m.text", "body": text });
    if let Some(reply) = reply_to.map(str::trim).filter(|value| !value.is_empty()) {
        body["m.relates_to"] = json!({ "m.in_reply_to": { "event_id": reply } });
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let transaction = format!("{stamp}-{}", std::process::id());
    let path = format!(
        "/_matrix/client/v3/rooms/{}/send/m.room.message/{transaction}",
        enc(room_id)
    );
    let result = matrix::put(&path, &body)?;
    Ok(
        json!({ "room_id": room_id, "event_id": result.get("event_id").cloned().unwrap_or(Value::Null) }),
    )
}
