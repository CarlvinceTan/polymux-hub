// Search across every bridged conversation. Messages are copied into a local
// index (~/.local/share/hub/index.db): SQLite full text for exact words, and
// EmbeddingGemma 2 vectors from a local embedding server for meaning, with photos,
// voice notes and videos embedded as media in the same space as the text. Search
// ranks messages containing every word first, then messages that mean the same.
// The index lives only on this machine; nothing leaves it but requests to the
// homeserver and the local embedding server.
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use crate::{
    hub::{enc, home},
    matrix, ops,
};

const MAX_PER_ROOM: usize = 20_000; // a first fill stops here for one conversation
const MEDIA_MAX: usize = 12 * 1024 * 1024; // larger files are indexed by caption only

fn embed_url() -> String {
    std::env::var("HUB_EMBED_URL").unwrap_or_else(|_| "http://127.0.0.1:8277/v1/embeddings".into())
}

fn index_path() -> PathBuf {
    home().join(".local/share/hub/index.db")
}

fn open() -> Result<Connection> {
    let path = index_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let db = Connection::open(&path).context("open the search index")?;
    db.execute_batch(
        "pragma journal_mode = wal;
         create table if not exists rooms(room_id text primary key, name text, filled integer default 0);
         create table if not exists messages(event_id text primary key, room_id text, room text, sender text,
             ts integer, kind text, body text, mxc text, mime text, embedded integer default 0);
         create index if not exists messages_pending on messages(embedded, ts);
         create virtual table if not exists fts using fts5(event_id unindexed, body, tokenize = 'porter unicode61');
         create table if not exists vecs(event_id text primary key, vec blob);",
    )?;
    Ok(db)
}

// ── filling the index from the homeserver ──

fn joined_rooms() -> Result<Vec<String>> {
    let joined = matrix::get("/_matrix/client/v3/joined_rooms")?;
    Ok(joined["joined_rooms"]
        .as_array()
        .map(|l| l.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default())
}

fn members(room_id: &str) -> HashMap<String, String> {
    let path = format!("/_matrix/client/v3/rooms/{}/joined_members", enc(room_id));
    matrix::get(&path)
        .ok()
        .and_then(|v| v["joined"].as_object().cloned())
        .map(|m| {
            m.into_iter()
                .map(|(id, info)| {
                    let name = info["display_name"].as_str().unwrap_or("").to_string();
                    (id, name)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Copy new messages into the index, for up to `budget` (None: until done).
pub fn sync(budget: Option<Duration>) -> Result<usize> {
    let db = open()?;
    let started = Instant::now();
    let mut added = 0;
    for room_id in joined_rooms()? {
        if budget.is_some_and(|b| started.elapsed() > b) {
            break;
        }
        let filled: i64 = db
            .query_row("select filled from rooms where room_id = ?", [&room_id], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        let name = ops::room_name(&room_id);
        db.execute(
            "insert into rooms(room_id, name) values (?1, ?2) on conflict(room_id) do update set name = ?2",
            params![room_id, name],
        )?;
        let names = members(&room_id);
        let mut from: Option<String> = None;
        let mut seen = 0usize;
        loop {
            let mut path = format!("/_matrix/client/v3/rooms/{}/messages?dir=b&limit=100", enc(&room_id));
            if let Some(token) = &from {
                path.push_str(&format!("&from={}", enc(token)));
            }
            let page = matrix::get(&path)?;
            let chunk = page["chunk"].as_array().cloned().unwrap_or_default();
            let mut known = false;
            let tx = db.unchecked_transaction()?;
            for event in &chunk {
                if event["type"] != "m.room.message" {
                    continue;
                }
                let id = event["event_id"].as_str().unwrap_or("");
                let exists: bool = tx
                    .query_row("select 1 from messages where event_id = ?", [id], |_| Ok(true))
                    .optional()?
                    .is_some();
                if exists {
                    known = true;
                    continue;
                }
                let content = &event["content"];
                let kind = content["msgtype"].as_str().unwrap_or("m.text");
                let body = content["body"].as_str().unwrap_or("");
                let sender_id = event["sender"].as_str().unwrap_or("");
                let sender = names.get(sender_id).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| sender_id.to_string());
                let mxc = content["url"].as_str().unwrap_or("");
                let mime = content["info"]["mimetype"].as_str().unwrap_or("");
                tx.execute(
                    "insert into messages(event_id, room_id, room, sender, ts, kind, body, mxc, mime) values (?,?,?,?,?,?,?,?,?)",
                    params![id, room_id, name, sender, event["origin_server_ts"].as_i64().unwrap_or(0), kind, body, mxc, mime],
                )?;
                tx.execute("insert into fts(event_id, body) values (?, ?)", params![id, format!("{sender}: {body}")])?;
                added += 1;
                seen += 1;
            }
            tx.commit()?;
            let end = page["end"].as_str().map(str::to_owned);
            // Caught up once a page reaches what an earlier run copied; a first
            // fill goes back to the start of the conversation (or the cap).
            if chunk.is_empty() || end.is_none() || (known && filled == 1) || seen >= MAX_PER_ROOM {
                break;
            }
            if budget.is_some_and(|b| started.elapsed() > b) {
                return Ok(added); // the room is not marked filled: the next run carries on
            }
            from = end;
        }
        db.execute("update rooms set filled = 1 where room_id = ?", [&room_id])?;
    }
    Ok(added)
}

// ── embedding ──

fn unit(v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.into_iter().map(|x| x / n).collect()
}

fn post_embeddings(input: Value) -> Result<Vec<Vec<f32>>> {
    let mut response = ureq::post(&embed_url())
        .config()
        .timeout_global(Some(Duration::from_secs(180)))
        .build()
        .send_json(json!({ "input": input }))
        .context("embedding server (tools/embed in cstack) did not answer")?;
    let body: Value = response.body_mut().read_json()?;
    Ok(body["data"]
        .as_array()
        .map(|l| {
            l.iter()
                .map(|d| unit(d["embedding"].as_array().map(|e| e.iter().filter_map(Value::as_f64).map(|x| x as f32).collect()).unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default())
}

fn doc_text(room: &str, sender: &str, body: &str) -> String {
    let body: String = body.chars().take(1500).collect();
    format!("title: none | text: {room} — {sender}: {body}")
}

fn download(mxc: &str) -> Option<Vec<u8>> {
    let rest = mxc.strip_prefix("mxc://")?;
    matrix::get_bytes(&format!("/_matrix/client/v1/media/download/{rest}"), MEDIA_MAX).ok()
}

// ffmpeg turns voice notes (ogg/opus, m4a…) into the wav the model reads, and
// pulls a few frames and the sound out of a video. Without ffmpeg media is
// indexed by its caption.
fn ffmpeg(args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg").args(["-v", "error"]).args(args).output().ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

fn media_parts(kind: &str, mime: &str, data: &[u8]) -> Vec<Value> {
    let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD.encode(d);
    let tmp = std::env::temp_dir().join(format!("hub-media-{}", std::process::id()));
    let mut parts = Vec::new();
    if kind == "m.image" || mime.starts_with("image/") {
        let m = if mime.is_empty() { "image/jpeg" } else { mime };
        parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:{m};base64,{}", b64(data))}}));
    } else if kind == "m.audio" || mime.starts_with("audio/") || kind == "m.video" || mime.starts_with("video/") {
        if std::fs::write(&tmp, data).is_err() {
            return parts;
        }
        let p = tmp.to_string_lossy().to_string();
        if kind == "m.video" || mime.starts_with("video/") {
            for t in ["00:00:00.5", "00:00:03", "00:00:08"] {
                if let Some(frame) = ffmpeg(&["-ss", t, "-i", &p, "-frames:v", "1", "-vf", "scale=512:-2", "-f", "image2", "-c:v", "mjpeg", "-"]) {
                    parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{}", b64(&frame))}}));
                }
            }
        }
        if let Some(wav) = ffmpeg(&["-i", &p, "-t", "60", "-vn", "-ac", "1", "-ar", "16000", "-f", "wav", "-"]) {
            parts.push(json!({"type": "input_audio", "input_audio": {"data": b64(&wav), "format": "wav"}}));
        }
        let _ = std::fs::remove_file(&tmp);
    }
    parts
}

/// Embed messages that have no vector yet, newest first, for up to `budget`.
pub fn embed_pending(budget: Option<Duration>) -> Result<usize> {
    let db = open()?;
    let started = Instant::now();
    let mut done = 0;
    loop {
        if budget.is_some_and(|b| started.elapsed() > b) {
            break;
        }
        let rows: Vec<(String, String, String, String, String, String, String)> = {
            let mut q = db.prepare(
                "select event_id, room, sender, kind, body, mxc, mime from messages where embedded = 0 order by ts desc limit 48",
            )?;
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
                .collect::<Result<_, _>>()?
        };
        if rows.is_empty() {
            break;
        }
        let (media, text): (Vec<_>, Vec<_>) = rows.into_iter().partition(|r| !r.5.is_empty());
        if !text.is_empty() {
            let input: Vec<Value> = text.iter().map(|r| json!(doc_text(&r.1, &r.2, &r.4))).collect();
            let vecs = match post_embeddings(json!(input)) {
                Ok(v) => v,
                Err(_) if done == 0 => bail!("embedding server is not running"),
                Err(_) => break,
            };
            for (r, v) in text.iter().zip(vecs) {
                store(&db, &r.0, &v, 1)?;
            }
            done += text.len();
        }
        for r in &media {
            let caption = doc_text(&r.1, &r.2, &format!("[{}] {}", r.3.trim_start_matches("m."), r.4));
            let parts = download(&r.5).map(|d| media_parts(&r.3, &r.6, &d)).unwrap_or_default();
            let (input, state) = if parts.is_empty() {
                (json!([caption]), 2) // no media to read: the caption stands in
            } else {
                let mut content = vec![json!({"type": "text", "text": caption})];
                content.extend(parts);
                (json!([{ "content": content }]), 1)
            };
            let v = post_embeddings(input.clone()).or_else(|_| post_embeddings(json!([caption])));
            match v {
                Ok(v) if !v.is_empty() => store(&db, &r.0, &v[0], state)?,
                _ => {
                    db.execute("update messages set embedded = 3 where event_id = ?", [&r.0])?;
                }
            }
            done += 1;
        }
    }
    Ok(done)
}

fn store(db: &Connection, id: &str, v: &[f32], state: i64) -> Result<()> {
    let blob: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    db.execute("insert or replace into vecs values (?, ?)", params![id, blob])?;
    db.execute("update messages set embedded = ? where event_id = ?", params![state, id])?;
    Ok(())
}

// ── searching ──

fn fts_query(words: &[String]) -> String {
    words
        .iter()
        .map(|w| match w.to_uppercase().as_str() {
            "OR" | "AND" | "NOT" => w.to_uppercase(),
            _ => format!("\"{}\"", w.replace('"', "\"\"")),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Filters {
    pub room: Option<String>,
    pub since_ms: Option<i64>,
}

fn filter_sql(f: &Filters, alias: &str) -> (String, Vec<String>) {
    let mut sql = String::new();
    let mut args = Vec::new();
    if let Some(r) = &f.room {
        sql.push_str(&format!(" and ({alias}.room like ? or {alias}.room_id = ?)"));
        args.push(format!("%{r}%"));
        args.push(r.clone());
    }
    if let Some(t) = f.since_ms {
        sql.push_str(&format!(" and {alias}.ts >= {t}"));
    }
    (sql, args)
}

pub fn search(words: &[String], f: &Filters, limit: usize, refresh: bool) -> Result<Value> {
    if refresh {
        // keep the index current without making a search slow: a little each time
        let _ = sync(Some(Duration::from_secs(8)));
        let _ = embed_pending(Some(Duration::from_secs(8)));
    }
    let db = open()?;
    let (extra, extra_args) = filter_sql(f, "m");
    // 1. messages containing every word
    let mut keyword: Vec<String> = Vec::new();
    {
        let sql = format!(
            "select m.event_id from fts join messages m on m.event_id = fts.event_id where fts match ?{extra} order by bm25(fts) limit 200"
        );
        let mut q = db.prepare(&sql)?;
        let mut args: Vec<String> = vec![fts_query(words)];
        args.extend(extra_args.clone());
        let rows = q.query_map(rusqlite::params_from_iter(args.iter()), |r| r.get::<_, String>(0))?;
        for r in rows.flatten() {
            keyword.push(r);
        }
    }
    // 2. messages that mean the same (text and media alike)
    let mut semantic: Vec<(String, f32)> = Vec::new();
    let text = words.iter().filter(|w| !["OR", "AND", "NOT"].contains(&w.to_uppercase().as_str())).cloned().collect::<Vec<_>>().join(" ");
    if let Ok(q) = post_embeddings(json!([format!("task: search result | query: {text}")])) {
        if let Some(q) = q.first() {
            let sql = format!("select v.event_id, v.vec from vecs v join messages m on m.event_id = v.event_id where 1=1{extra}");
            let mut stmt = db.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(extra_args.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
            for (id, blob) in rows.flatten() {
                let score: f32 = blob.chunks_exact(4).zip(q).map(|(b, x)| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * x).sum();
                semantic.push((id, score));
            }
            semantic.sort_by(|a, b| b.1.total_cmp(&a.1));
            semantic.truncate(200);
        }
    }
    let exact: HashSet<&String> = keyword.iter().collect();
    let mut order: Vec<(String, &str)> = Vec::new();
    let mut seen = HashSet::new();
    for id in keyword.iter().take(5) {
        if seen.insert(id.clone()) { order.push((id.clone(), "exact")); }
    }
    for (id, _) in &semantic {
        if seen.insert(id.clone()) { order.push((id.clone(), if exact.contains(id) { "exact" } else { "meaning" })); }
    }
    for id in keyword.iter().skip(5) {
        if seen.insert(id.clone()) { order.push((id.clone(), "exact")); }
    }
    order.truncate(limit);
    let mut out = Vec::new();
    for (id, how) in order {
        let row = db.query_row(
            "select room, room_id, sender, ts, kind, body from messages where event_id = ?",
            [&id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?)),
        )?;
        let when = chrono::DateTime::from_timestamp_millis(row.3).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).unwrap_or_default();
        out.push(json!({"event_id": id, "room": row.0, "room_id": row.1, "sender": row.2, "timestamp": when,
                        "kind": row.4.trim_start_matches("m."), "body": row.5, "match": how}));
    }
    Ok(json!(out))
}

pub fn status() -> Result<Value> {
    let db = open()?;
    let count = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    Ok(json!({
        "index": index_path(),
        "rooms": count("select count(*) from rooms"),
        "rooms_filled": count("select count(*) from rooms where filled = 1"),
        "messages": count("select count(*) from messages"),
        "media": count("select count(*) from messages where mxc != ''"),
        "embedded": count("select count(*) from messages where embedded in (1, 2)"),
        "media_embedded_as_media": count("select count(*) from messages where mxc != '' and embedded = 1"),
        "waiting": count("select count(*) from messages where embedded = 0"),
    }))
}
