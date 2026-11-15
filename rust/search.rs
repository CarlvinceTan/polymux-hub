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
         create table if not exists vecs(event_id text primary key, vec blob);
         create table if not exists edits(event_id text primary key, target text, body text, ts integer);
         create index if not exists edits_target on edits(target, ts);
         create table if not exists redacted(target text primary key);
         create table if not exists state(key text primary key, value text);
         create table if not exists members(room_id text, user_id text, name text, primary key(room_id, user_id));",
    )?;
    let cols: Vec<String> = db
        .prepare("select name from pragma_table_info('rooms')")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if !cols.iter().any(|c| c == "back") {
        db.execute_batch("alter table rooms add column back text")?;
    }
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

// Only messages and deletions, no room state: a refresh is one small request.
const SYNC_FILTER: &str = r#"{"room":{"timeline":{"limit":100,"types":["m.room.message","m.room.redaction"]},"state":{"types":[]},"ephemeral":{"types":[]},"account_data":{"types":[]}},"presence":{"types":[]},"account_data":{"types":[]}}"#;

fn state_get(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db.query_row("select value from state where key = ?", [key], |r| r.get(0)).optional()?)
}

fn state_set(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute("insert or replace into state values (?, ?)", params![key, value])?;
    Ok(())
}

fn has(tx: &Connection, sql: &str, id: &str) -> Result<bool> {
    Ok(tx.query_row(sql, [id], |_| Ok(true)).optional()?.is_some())
}

/// Apply a batch of a room's events to the index: new messages are added, edits
/// replace a message's text (and queue it to be embedded again), deletions remove
/// it. Returns how many messages were added and whether any event was already known.
fn ingest(db: &Connection, room_id: &str, name: &str, names: &HashMap<String, String>, events: &[Value]) -> Result<(usize, bool)> {
    let tx = db.unchecked_transaction()?;
    let (mut added, mut known) = (0, false);
    for event in events {
        let id = event["event_id"].as_str().unwrap_or("");
        // A deletion: drop the message, its words and its vector, and remember it
        // so a first fill (newest first) does not add the original afterwards.
        if event["type"] == "m.room.redaction" {
            let target = event["redacts"].as_str().or_else(|| event["content"]["redacts"].as_str()).unwrap_or("");
            if !target.is_empty() {
                if tx.execute("insert or ignore into redacted values (?)", [target])? > 0 {
                    tx.execute("delete from messages where event_id = ?", [target])?;
                    tx.execute("delete from fts where event_id = ?", [target])?;
                    tx.execute("delete from vecs where event_id = ?", [target])?;
                } else {
                    known = true;
                }
            }
            continue;
        }
        if event["type"] != "m.room.message" {
            continue;
        }
        // An edit: the message's text becomes the new text and is embedded again.
        let relates = &event["content"]["m.relates_to"];
        if relates["rel_type"] == "m.replace" {
            let target = relates["event_id"].as_str().unwrap_or("");
            let body = event["content"]["m.new_content"]["body"].as_str().unwrap_or("");
            let ts = event["origin_server_ts"].as_i64().unwrap_or(0);
            if tx.execute("insert or ignore into edits values (?,?,?,?)", params![id, target, body, ts])? == 0 {
                known = true;
                continue;
            }
            let latest: Option<i64> = tx.query_row("select max(ts) from edits where target = ?", [target], |r| r.get(0))?;
            if latest == Some(ts) {
                let sender: Option<String> =
                    tx.query_row("select sender from messages where event_id = ?", [target], |r| r.get(0)).optional()?;
                if let Some(sender) = sender {
                    tx.execute("update messages set body = ?, embedded = 0 where event_id = ?", params![body, target])?;
                    tx.execute("delete from vecs where event_id = ?", [target])?;
                    tx.execute("update fts set body = ? where event_id = ?", params![format!("{sender}: {body}"), target])?;
                }
            }
            continue;
        }
        if has(&tx, "select 1 from redacted where target = ?", id)? {
            continue;
        }
        if has(&tx, "select 1 from messages where event_id = ?", id)? {
            known = true;
            continue;
        }
        let content = &event["content"];
        let kind = content["msgtype"].as_str().unwrap_or("m.text");
        // an edit that arrived before its original (a first fill reads newest first)
        let edited: Option<String> = tx
            .query_row("select body from edits where target = ? order by ts desc limit 1", [id], |r| r.get(0))
            .optional()?;
        let body = edited.as_deref().unwrap_or_else(|| content["body"].as_str().unwrap_or(""));
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
    }
    tx.commit()?;
    Ok((added, known))
}

/// Page back through a room from `from` (None: the newest message). A gap fill
/// stops at the first page holding something already indexed; a first fill goes
/// to the start of the conversation (or the cap), saving its place in rooms.back
/// so a run cut short by the budget resumes there. Returns (added, finished).
fn backfill(db: &Connection, room_id: &str, name: &str, names: &HashMap<String, String>, mut from: Option<String>,
            first_fill: bool, deadline: Option<Instant>) -> Result<(usize, bool)> {
    let mut added = 0;
    loop {
        let mut path = format!("/_matrix/client/v3/rooms/{}/messages?dir=b&limit=100", enc(room_id));
        if let Some(token) = &from {
            path.push_str(&format!("&from={}", enc(token)));
        }
        let page = matrix::get(&path)?;
        let chunk = page["chunk"].as_array().cloned().unwrap_or_default();
        let (n, known) = ingest(db, room_id, name, names, &chunk)?;
        added += n;
        let end = page["end"].as_str().map(str::to_owned);
        let total: i64 = db.query_row("select count(*) from messages where room_id = ?", [room_id], |r| r.get(0))?;
        if chunk.is_empty() || end.is_none() || (known && !first_fill) || total as usize >= MAX_PER_ROOM {
            return Ok((added, true));
        }
        if first_fill {
            db.execute("update rooms set back = ? where room_id = ?", params![end, room_id])?;
        }
        if deadline.is_some_and(|d| Instant::now() > d) {
            return Ok((added, false));
        }
        from = end;
    }
}

/// A room's members (fetched, and remembered for telling who is who in results).
fn members_in(db: &Connection, room_id: &str) -> HashMap<String, String> {
    let names = members(room_id);
    if !names.is_empty() {
        let _ = db.execute("delete from members where room_id = ?", [room_id]);
        for (id, name) in &names {
            let _ = db.execute("insert or replace into members values (?,?,?)", params![room_id, id, name]);
        }
    }
    names
}

/// Bring the index up to date, for up to `budget` (None: until done). After the
/// first run this is one /sync request for everything that changed since the last
/// one, plus paging back only in rooms with more new messages than /sync returns.
pub fn sync(budget: Option<Duration>) -> Result<usize> {
    let db = open()?;
    let deadline = budget.map(|b| Instant::now() + b);
    let mut added = 0;
    let since = state_get(&db, "since")?;
    let mut url = format!("/_matrix/client/v3/sync?timeout=0&filter={}", enc(SYNC_FILTER));
    if let Some(s) = &since {
        url.push_str(&format!("&since={}", enc(s)));
    }
    // Without a token this only marks where the first fill starts: anything sent
    // while it runs arrives with the next sync.
    let page = matrix::get(&url)?;
    if since.is_none() {
        for room_id in joined_rooms()? {
            db.execute("insert or ignore into rooms(room_id, name) values (?, '')", [&room_id])?;
            // an index filled before tokens were kept: catch each room up once
            let filled: i64 = db.query_row("select filled from rooms where room_id = ?", [&room_id], |r| r.get(0))?;
            if filled == 1 {
                let name = ops::room_name(&room_id);
                added += backfill(&db, &room_id, &name, &members_in(&db, &room_id), None, false, None)?.0;
            }
        }
    } else if let Some(rooms) = page["rooms"]["join"].as_object() {
        for (room_id, room) in rooms {
            let events = room["timeline"]["events"].as_array().cloned().unwrap_or_default();
            let filled: Option<i64> =
                db.query_row("select filled from rooms where room_id = ?", [room_id], |r| r.get(0)).optional()?;
            if filled.is_none() {
                db.execute("insert into rooms(room_id, name) values (?, '')", [room_id])?; // a new room: filled below
                continue;
            }
            if events.is_empty() || filled == Some(0) {
                continue;
            }
            let name = ops::room_name(room_id);
            db.execute("update rooms set name = ? where room_id = ?", params![name, room_id])?;
            let names = members_in(&db, room_id);
            added += ingest(&db, room_id, &name, &names, &events)?.0;
            if room["timeline"]["limited"] == true {
                let prev = room["timeline"]["prev_batch"].as_str().map(str::to_owned);
                added += backfill(&db, room_id, &name, &names, prev, false, None)?.0;
            }
        }
    }
    if let Some(next) = page["next_batch"].as_str() {
        state_set(&db, "since", next)?;
    }
    // First fills: new rooms, and any cut short by the budget last time.
    let pending: Vec<(String, Option<String>)> = {
        let mut q = db.prepare("select room_id, back from rooms where filled = 0")?;
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    for (room_id, back) in pending {
        if deadline.is_some_and(|d| Instant::now() > d) {
            break;
        }
        let name = ops::room_name(&room_id);
        db.execute("update rooms set name = ? where room_id = ?", params![name, room_id])?;
        let names = members_in(&db, &room_id);
        let (n, finished) = backfill(&db, &room_id, &name, &names, back, true, deadline)?;
        added += n;
        if finished {
            db.execute("update rooms set filled = 1, back = null where room_id = ?", [&room_id])?;
        }
    }
    // rooms indexed before members were remembered
    let missing: Vec<String> = {
        let mut q = db.prepare("select room_id from rooms where room_id not in (select room_id from members)")?;
        q.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    for room_id in missing {
        if deadline.is_some_and(|d| Instant::now() > d) {
            break;
        }
        members_in(&db, &room_id);
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

/// Who is who in results. Bridges without double puppeting show the user's own
/// account as one more ghost (often just a phone number): it is the ghost of that
/// bridge found in nearly every private chat. Returns the (room, display name) pairs
/// that are the user, and the private chats with their one other person.
fn people(db: &Connection) -> Result<(HashSet<String>, HashMap<String, String>)> {
    let own = crate::hub::mxid();
    let mut rooms: HashMap<String, Vec<(String, String)>> = HashMap::new();
    {
        let mut q = db.prepare("select room_id, user_id, name from members")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
            let (room, id, name) = row?;
            rooms.entry(room).or_default().push((id, name));
        }
    }
    let ghost = |id: &str| -> Option<String> {
        let local = id.trim_start_matches('@').split(':').next().unwrap_or("");
        let (prefix, _) = local.split_once('_')?;
        (!local.ends_with("bot")).then(|| prefix.to_string())
    };
    // private chats: exactly two ghosts of one bridge
    let mut dms: HashMap<String, Vec<String>> = HashMap::new(); // prefix -> rooms
    let mut seen: HashMap<String, usize> = HashMap::new(); // ghost id -> private chats it is in
    for (room, list) in &rooms {
        let ghosts: Vec<(&String, String)> = list.iter().filter_map(|(id, _)| ghost(id).map(|p| (id, p))).collect();
        if ghosts.len() == 2 && ghosts[0].1 == ghosts[1].1 {
            dms.entry(ghosts[0].1.clone()).or_default().push(room.clone());
            for (id, _) in &ghosts {
                *seen.entry((*id).clone()).or_default() += 1;
            }
        }
    }
    let mut me_ids: HashSet<String> = HashSet::from([own.clone()]);
    for (prefix, list) in &dms {
        if list.len() < 2 {
            continue;
        }
        if let Some((id, n)) = seen.iter().filter(|(id, _)| ghost(id).as_deref() == Some(prefix)).max_by_key(|(_, n)| **n) {
            if *n * 10 >= list.len() * 6 {
                me_ids.insert(id.clone());
            }
        }
    }
    let mut me_names = HashSet::new();
    for (room, list) in &rooms {
        for (id, name) in list {
            if me_ids.contains(id) && !name.is_empty() {
                me_names.insert(format!("{room}\u{0}{name}"));
            }
        }
    }
    let mut other = HashMap::new();
    for list in dms.values() {
        for room in list {
            if let Some((_, name)) = rooms[room].iter().find(|(id, _)| ghost(id).is_some() && !me_ids.contains(id)) {
                other.insert(format!("{room}\u{0}{name}"), room.clone());
            }
        }
    }
    Ok((me_names, other))
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
    // The same text from the same sender in the same conversation (a notice sent
    // twice, a forward repeated) shows once. Media without text are all distinct.
    let (me, dm_other) = people(&db).unwrap_or_default();
    let mut out = Vec::new();
    let mut shown: HashSet<(String, String, String)> = HashSet::new();
    for (id, how) in order {
        if out.len() >= limit {
            break;
        }
        let row = db.query_row(
            "select room, room_id, sender, ts, kind, body from messages where event_id = ?",
            [&id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?)),
        )?;
        if !row.5.trim().is_empty() && !shown.insert((row.1.clone(), row.2.clone(), row.5.clone())) {
            continue;
        }
        let when = chrono::DateTime::from_timestamp_millis(row.3).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).unwrap_or_default();
        let from_me = me.contains(&format!("{}\u{0}{}", row.1, row.2)) || row.2 == crate::hub::mxid();
        // in a private chat the other person is the one the chat is named after
        let sender = if from_me {
            "me".to_string()
        } else if dm_other.contains_key(&format!("{}\u{0}{}", row.1, row.2)) {
            row.0.clone()
        } else {
            row.2.clone()
        };
        out.push(json!({"event_id": id, "room": row.0, "room_id": row.1, "sender": sender, "from_me": from_me,
                        "timestamp": when, "kind": row.4.trim_start_matches("m."), "body": row.5, "match": how}));
    }
    Ok(json!(out))
}

/// Save a message's photo, video or voice note to ~/.cache/hub/media so it can be
/// looked at; a video also gets a sheet of frames as one image.
pub fn media(event_id: &str) -> Result<Value> {
    let db = open()?;
    let row: Option<(String, String, String, String, String, i64)> = db
        .query_row("select mxc, mime, kind, body, sender, ts from messages where event_id = ?", [event_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .optional()?;
    let Some((mxc, mime, kind, body, _, _)) = row else { bail!("no message {event_id} in the index") };
    let Some(rest) = mxc.strip_prefix("mxc://") else { bail!("message {event_id} has no media") };
    let ext = match mime.as_str() {
        "image/jpeg" => "jpg".to_string(),
        "image/png" => "png".into(),
        "image/webp" => "webp".into(),
        "image/gif" => "gif".into(),
        "video/mp4" => "mp4".into(),
        "video/quicktime" => "mov".into(),
        "audio/ogg" => "ogg".into(),
        "audio/mpeg" => "mp3".into(),
        "audio/mp4" | "audio/x-m4a" => "m4a".into(),
        m => std::path::Path::new(&body)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| m.split('/').nth(1).unwrap_or("bin").to_string()),
    };
    let dir = home().join(".cache/hub/media");
    std::fs::create_dir_all(&dir)?;
    let stem: String = event_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(24).collect();
    let file = dir.join(format!("{stem}.{ext}"));
    if !file.exists() {
        let data = matrix::get_bytes(&format!("/_matrix/client/v1/media/download/{rest}"), 500 * 1024 * 1024)?;
        std::fs::write(&file, data)?;
    }
    let mut out = json!({"path": file, "kind": kind.trim_start_matches("m."), "mime": mime});
    if kind == "m.video" || mime.starts_with("video/") {
        let sheet = dir.join(format!("{stem}-frames.jpg"));
        if !sheet.exists() {
            let _ = Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-i"])
                .arg(&file)
                .args(["-vf", "fps=1/3,scale=360:-2,tile=4x2", "-frames:v", "1"])
                .arg(&sheet)
                .status();
        }
        if sheet.exists() {
            out["frames"] = json!(sheet);
        }
    }
    Ok(out)
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
