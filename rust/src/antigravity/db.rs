//! Antigravity CLI's SQLite stores under `~/.gemini/antigravity-cli`:
//! `conversation_summaries.db` (one row per conversation: status, title,
//! workspace, parent) and `conversations/<id>.db` (the conversation's steps,
//! each model call's usage inside a protobuf blob).

use crate::antigravity::proto;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::path::Path;

/// Open read-only. The CLI keeps these databases in WAL mode and removes the
/// `-shm` file once no writer has them open, and a read-only connection
/// cannot create it, so that case falls back to `immutable=1`: nothing is
/// writing then, so nothing can be missed.
pub fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    // Opening succeeds lazily; the first read is what hits the missing -shm.
    let ro = Connection::open_with_flags(format!("file:{}?mode=ro", path.display()), flags).and_then(|c| {
        c.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
        Ok(c)
    });
    match ro {
        Ok(connection) => {
            connection.busy_timeout(std::time::Duration::from_secs(1))?;
            Ok(connection)
        }
        Err(_) => Connection::open_with_flags(format!("file:{}?immutable=1", path.display()), flags),
    }
}

#[derive(Debug, Clone)]
pub struct Conversation {
    #[allow(dead_code)] // the map key; kept so a row is self-describing
    pub id: String,
    pub title: String,
    pub preview: String,
    pub workspace: String,
    pub status: String,
    pub parent_id: Option<String>,
    pub agent_name: String,
    pub last_modified_at: Option<f64>,
    pub created_at: Option<f64>,
}

/// "2026-11-22 22:47:29.20742+00:00" (Go's time format, zero time when unset).
fn parse_go_time(value: &str) -> Option<f64> {
    if value.starts_with("0001-") {
        return None;
    }
    let normalized = value.replacen(' ', "T", 1);
    chrono::DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|dt| dt.timestamp() as f64 + dt.timestamp_subsec_nanos() as f64 / 1e9)
}

/// `file:///Users/x/proj` -> `/Users/x/proj` (percent-escapes decoded).
fn uri_to_path(uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Every conversation in the summaries database, keyed by id.
pub fn load_conversations(home: &Path) -> rusqlite::Result<HashMap<String, Conversation>> {
    let connection = open_read_only(&home.join("conversation_summaries.db"))?;
    let mut stmt = connection.prepare(
        "SELECT conversation_id, title, preview, workspace_uris, status, parent_conversation_id, agent_name, \
         last_modified_time, raw_summary FROM conversation_summaries",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = HashMap::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let workspaces: String = row.get(3)?;
        let workspace = serde_json::from_str::<Vec<String>>(&workspaces)
            .ok()
            .and_then(|w| w.into_iter().next())
            .map(|uri| uri_to_path(&uri))
            .unwrap_or_default();
        let parent: String = row.get(5)?;
        let modified: String = row.get(7)?;
        let raw: Option<Vec<u8>> = row.get(8)?;
        out.insert(
            id.clone(),
            Conversation {
                id,
                title: row.get(1)?,
                preview: row.get(2)?,
                workspace,
                status: row.get(4)?,
                parent_id: if parent.is_empty() { None } else { Some(parent) },
                agent_name: row.get(6)?,
                last_modified_at: parse_go_time(&modified),
                // the summary message's field 7 is when the conversation began
                created_at: raw.as_deref().and_then(|r| proto::timestamp(r, 7)),
            },
        );
    }
    Ok(out)
}

/// One model call's token counts (Antigravity's `ModelUsageStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CallUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
}

impl CallUsage {
    /// ModelUsageStats: 1 model (enum), 2 input, 3 output (thinking and
    /// response together), 4 cache write, 5 cache read. Input excludes the
    /// cache reads.
    fn from_stats(stats: &[u8]) -> (Option<u64>, CallUsage) {
        let get = |n| proto::varint_field(stats, n).unwrap_or(0) as i64;
        (
            proto::varint_field(stats, 1),
            CallUsage {
                input_tokens: get(2),
                output_tokens: get(3),
                cache_write_tokens: get(4),
                cache_read_tokens: get(5),
            },
        )
    }

    pub fn total_tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_write_tokens + self.cache_read_tokens
    }
}

#[derive(Debug, Clone)]
pub struct CallEvent {
    pub at: f64,
    pub model: Option<String>,
    pub usage: CallUsage,
}

/// The model calls of one `conversations/<id>.db`, re-read whenever the file
/// (or its WAL) changes. Steps are small and few; the per-call generator
/// records that name each model enum are large, so only new ones are read.
pub struct ConversationLog {
    path: String,
    signature: Option<(u64, u64, u64, u64)>,
    model_names: HashMap<u64, (String, String)>,
    last_gen_idx: i64,
    pub events: Vec<CallEvent>,
    /// Display name of the latest call's model, e.g. "Gemini 4 Neon (Medium)".
    pub last_display_name: Option<String>,
}

fn file_signature(path: &str) -> Option<(u64, u64, u64, u64)> {
    let stamp = |p: &str| {
        std::fs::metadata(p)
            .ok()
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                (mtime, m.len())
            })
    };
    let (db_mtime, db_len) = stamp(path)?;
    let (wal_mtime, wal_len) = stamp(&format!("{}-wal", path)).unwrap_or((0, 0));
    Some((db_mtime, db_len, wal_mtime, wal_len))
}

impl ConversationLog {
    pub fn new(path: String) -> Self {
        ConversationLog {
            path,
            signature: None,
            model_names: HashMap::new(),
            last_gen_idx: -1,
            events: Vec::new(),
            last_display_name: None,
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Re-read if the file changed since the last call. Errors (a file being
    /// replaced mid-read, a schema this tool does not know) keep the
    /// previous reading.
    pub fn refresh(&mut self) {
        let signature = match file_signature(&self.path) {
            Some(s) => s,
            None => return,
        };
        if self.signature == Some(signature) {
            return;
        }
        if self.read().is_ok() {
            self.signature = Some(signature);
        }
    }

    fn read(&mut self) -> rusqlite::Result<()> {
        let connection = open_read_only(Path::new(&self.path))?;
        {
            let mut stmt = connection.prepare("SELECT idx, data FROM gen_metadata WHERE idx > ?1 ORDER BY idx")?;
            let mut rows = stmt.query([self.last_gen_idx])?;
            while let Some(row) = rows.next()? {
                let idx: i64 = row.get(0)?;
                let data: Option<Vec<u8>> = row.get(1)?;
                self.last_gen_idx = self.last_gen_idx.max(idx);
                // {1: {4: ModelUsageStats, 19: model id, 21: display name}}
                let generation = match data.as_deref().and_then(|d| proto::bytes(d, 1)) {
                    Some(g) => g,
                    None => continue,
                };
                let model_enum = proto::bytes(generation, 4).and_then(|stats| proto::varint_field(stats, 1));
                if let (Some(model_enum), Some(name)) = (model_enum, proto::string(generation, 19)) {
                    let display = proto::string(generation, 21).unwrap_or_default();
                    self.model_names.insert(model_enum, (name, display));
                }
            }
        }
        let mut events = Vec::new();
        let mut last_display_name = None;
        let mut stmt = connection.prepare("SELECT metadata FROM steps WHERE metadata IS NOT NULL ORDER BY idx")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let metadata: Vec<u8> = row.get(0)?;
            // step metadata: 1 created (Timestamp), 9 ModelUsageStats
            let stats = match proto::bytes(&metadata, 9) {
                Some(s) => s,
                None => continue,
            };
            let at = match proto::timestamp(&metadata, 1) {
                Some(t) => t,
                None => continue,
            };
            let (model_enum, usage) = CallUsage::from_stats(stats);
            let named = model_enum.and_then(|m| self.model_names.get(&m));
            last_display_name = named.map(|(_, display)| display.clone()).or(last_display_name);
            events.push(CallEvent { at, model: named.map(|(name, _)| name.clone()), usage });
        }
        self.events = events;
        self.last_display_name = last_display_name;
        Ok(())
    }

    pub fn total_tokens(&self) -> i64 {
        self.events.iter().map(|e| e.usage.total_tokens()).sum()
    }

    pub fn last_model(&self) -> Option<String> {
        self.events.iter().rev().find_map(|e| e.model.clone())
    }

    /// "Gemini 4 Neon (Medium)" -> "medium": the effort Antigravity puts in
    /// the model's display name.
    pub fn last_effort(&self) -> Option<String> {
        let display = self.last_display_name.as_deref()?;
        let open = display.rfind('(')?;
        let effort = display[open + 1..].strip_suffix(')')?.trim().to_lowercase();
        if effort.is_empty() {
            None
        } else {
            Some(effort)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_times_and_file_uris_are_read() {
        assert_eq!(Some(1_795_387_649.20742), parse_go_time("2026-11-22 22:47:29.20742+00:00"));
        assert_eq!(None, parse_go_time("0001-01-01 00:00:00+00:00"));
        assert_eq!("/Users/x/my proj", uri_to_path("file:///Users/x/my%20proj"));
    }

    #[test]
    fn effort_comes_from_the_display_name() {
        let mut log = ConversationLog::new("x.db".to_string());
        log.last_display_name = Some("Gemini 4 Neon (Medium)".to_string());
        assert_eq!(Some("medium".to_string()), log.last_effort());
        log.last_display_name = Some("Gemini 3.5 Flash Lite".to_string());
        assert_eq!(None, log.last_effort());
    }
}
