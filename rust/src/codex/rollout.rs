use crate::model::{parse_timestamp, FIVE_HOUR_WINDOW_MINUTES, READ_CHUNK_SIZE};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

/// Incrementally tails one Codex rollout JSONL file, tracking RUN/IDLE/DONE
/// phase, current activity, cumulative tokens, and weekly rate-limit
/// samples. Mirrors `SessionState` in the Python script.
#[derive(Debug, Clone)]
pub struct SessionState {
    pub path: String,
    pub inode: Option<u64>,
    pub offset: u64,
    partial: Vec<u8>,
    pub phase: String,
    pub phase_at: f64,
    pub activity: String,
    pub last_event_at: f64,
    pub total_tokens: Option<i64>,
    pub weekly_used_percent: Option<f64>,
    pub weekly_resets_at: Option<f64>,
    pub weekly_sample_at: Option<f64>,
    pub weekly_limit_id: Option<String>,
    pub five_hour_used_percent: Option<f64>,
    pub five_hour_resets_at: Option<f64>,
    pub five_hour_sample_at: Option<f64>,
    pub service_tier: Option<String>,
}

impl SessionState {
    pub fn new(path: String) -> Self {
        let now = now_secs();
        SessionState {
            path,
            inode: None,
            offset: 0,
            partial: Vec::new(),
            phase: "start".to_string(),
            phase_at: now,
            activity: "starting".to_string(),
            last_event_at: now,
            total_tokens: None,
            weekly_used_percent: None,
            weekly_resets_at: None,
            weekly_sample_at: None,
            weekly_limit_id: None,
            five_hour_used_percent: None,
            five_hour_resets_at: None,
            five_hour_sample_at: None,
            service_tier: None,
        }
    }

    pub fn refresh(&mut self) {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => {
                self.phase = "stop".to_string();
                self.activity = "rollout missing".to_string();
                return;
            }
        };
        let inode = inode_of(&metadata);
        let size = metadata.len();
        if self.inode != Some(inode) || size < self.offset {
            self.inode = Some(inode);
            self.offset = 0;
            self.partial.clear();
            self.phase = "start".to_string();
            self.activity = "starting".to_string();
            self.service_tier = None;
        }
        if size == self.offset {
            return;
        }
        // Read in bounded chunks rather than one `read_to_end` covering the
        // whole remaining file. A single actively-growing rollout can reach
        // hundreds of MB to low GB (a 680MB rollout was observed live on
        // 2026-09-07); reading it whole on the first poll after startup was
        // the dominant contributor to a multi-hundred-MB peak memory spike
        // (see model::READ_CHUNK_SIZE). This has
        // no effect on which events get parsed or in what order.
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return,
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = vec![0u8; READ_CHUNK_SIZE];
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            self.feed_chunk(&buf[..n]);
        }
        if let Ok(pos) = file.stream_position() {
            self.offset = pos;
        }
    }

    fn feed_chunk(&mut self, chunk: &[u8]) {
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(chunk);
        let mut lines: Vec<Vec<u8>> = data.split(|&b| b == b'\n').map(|s| s.to_vec()).collect();
        self.partial = lines.pop().unwrap_or_default();
        for raw_line in lines {
            if raw_line.is_empty() || !relevant_event_line(&raw_line) {
                continue;
            }
            let event: Value = match serde_json::from_slice(&raw_line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            self.consume(&event);
        }
    }

    pub fn consume(&mut self, event: &Value) {
        let event_at = parse_timestamp(event.get("timestamp").and_then(|v| v.as_str()));
        if let Some(at) = event_at {
            self.last_event_at = at;
        }

        let outer_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let payload = match event.get("payload").and_then(|v| v.as_object()) {
            Some(p) => p,
            None => return,
        };

        if outer_type == "event_msg" {
            let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                "task_started" => {
                    self.phase = "run".to_string();
                    let started_at = payload.get("started_at").and_then(|v| v.as_f64());
                    self.phase_at = started_at.or(event_at).unwrap_or_else(now_secs);
                    self.activity = "thinking".to_string();
                }
                "task_complete" => {
                    self.phase = "done".to_string();
                    self.phase_at = event_at.unwrap_or_else(now_secs);
                    self.activity = "complete".to_string();
                }
                "turn_aborted" | "task_aborted" | "shutdown_complete" => {
                    self.phase = "stop".to_string();
                    self.phase_at = event_at.unwrap_or_else(now_secs);
                    self.activity = "stopped".to_string();
                }
                "agent_message" | "user_message" if self.phase == "run" => {
                    self.activity = if event_type == "agent_message" {
                        "responding".to_string()
                    } else {
                        "reading prompt".to_string()
                    };
                }
                "item_started" => {
                    if let Some(item) = payload.get("item").and_then(|v| v.as_object()) {
                        self.activity = activity_for_item(item);
                    }
                }
                "token_count" => {
                    self.consume_token_count(payload, event_at);
                }
                "thread_settings_applied" => {
                    let service_tier = payload
                        .get("thread_settings")
                        .and_then(|v| v.get("service_tier"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    self.service_tier = service_tier;
                }
                _ => {}
            }
        } else if outer_type == "response_item" && self.phase == "run" {
            let item_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match item_type {
                "custom_tool_call" | "function_call" => {
                    self.activity = payload
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool")
                        .to_string();
                }
                "reasoning" => self.activity = "thinking".to_string(),
                "message" => self.activity = "responding".to_string(),
                _ => {}
            }
        }
    }

    pub fn consume_token_count(
        &mut self,
        payload: &serde_json::Map<String, Value>,
        event_at: Option<f64>,
    ) {
        if let Some(info) = payload.get("info").and_then(|v| v.as_object()) {
            if let Some(total_tokens) = info
                .get("total_token_usage")
                .and_then(|v| v.get("total_tokens"))
                .and_then(|v| v.as_i64())
            {
                self.total_tokens = Some(total_tokens);
            }
        }

        let rate_limits = match payload.get("rate_limits").and_then(|v| v.as_object()) {
            Some(r) => r,
            None => return,
        };
        let limit_id = rate_limits.get("limit_id").and_then(|v| v.as_str());
        self.consume_five_hour_limit(rate_limits, event_at);
        for key in ["primary", "secondary", "individual_limit"] {
            let limit = match rate_limits.get(key).and_then(|v| v.as_object()) {
                Some(l) => l,
                None => continue,
            };
            let window_minutes = limit.get("window_minutes").and_then(|v| v.as_i64());
            if window_minutes != Some(7 * 24 * 60) {
                continue;
            }
            if let Some(used_percent) = limit.get("used_percent").and_then(|v| v.as_f64()) {
                self.weekly_used_percent = Some(used_percent);
                self.weekly_sample_at = Some(event_at.unwrap_or(self.last_event_at));
                self.weekly_limit_id = limit_id.map(|s| s.to_string());
            }
            if let Some(resets_at) = limit.get("resets_at").and_then(|v| v.as_f64()) {
                self.weekly_resets_at = Some(resets_at);
            }
            break;
        }
    }

    /// Codex currently reports only the weekly window, but a 5-hour window
    /// may be imposed at any time. Each rate_limits event replaces the
    /// previous 5h sample wholesale, so a window that disappears again is
    /// dropped rather than lingering from an older event.
    fn consume_five_hour_limit(&mut self, rate_limits: &serde_json::Map<String, Value>, event_at: Option<f64>) {
        let five_hour = ["primary", "secondary", "individual_limit"].iter().find_map(|key| {
            let limit = rate_limits.get(*key)?.as_object()?;
            if limit.get("window_minutes").and_then(|v| v.as_i64()) != Some(FIVE_HOUR_WINDOW_MINUTES) {
                return None;
            }
            Some(limit)
        });
        self.five_hour_used_percent = five_hour.and_then(|l| l.get("used_percent")).and_then(|v| v.as_f64());
        self.five_hour_resets_at = five_hour.and_then(|l| l.get("resets_at")).and_then(|v| v.as_f64());
        self.five_hour_sample_at = self
            .five_hour_used_percent
            .map(|_| event_at.unwrap_or(self.last_event_at));
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(unix)]
fn inode_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(not(unix))]
fn inode_of(_metadata: &std::fs::Metadata) -> u64 {
    0
}

pub fn activity_for_item(item: &serde_json::Map<String, Value>) -> String {
    let item_type = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if item_type.contains("command") {
        if let Some(command) = item.get("command").and_then(|v| v.as_array()) {
            if let Some(first) = command.first().and_then(|v| v.as_str()) {
                return Path::new(first)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| first.to_string());
            }
        }
        return "command".to_string();
    }
    if item_type.contains("reason") {
        return "thinking".to_string();
    }
    if item_type.contains("message") {
        return "responding".to_string();
    }
    if item_type.contains("tool") {
        return item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("tool")
            .to_string();
    }
    if item_type.is_empty() {
        "working".to_string()
    } else {
        item_type
    }
}

/// Reject lines that cannot affect lifecycle/activity/token state before
/// JSON-decoding them (rollout files can contain multi-megabyte command
/// output records). Mirrors `relevant_event_line`.
pub fn relevant_event_line(raw_line: &[u8]) -> bool {
    let prefix = &raw_line[..raw_line.len().min(768)];
    if contains(prefix, b"\"type\":\"event_msg\"") {
        return [
            "\"type\":\"task_started\"",
            "\"type\":\"task_complete\"",
            "\"type\":\"turn_aborted\"",
            "\"type\":\"task_aborted\"",
            "\"type\":\"shutdown_complete\"",
            "\"type\":\"agent_message\"",
            "\"type\":\"user_message\"",
            "\"type\":\"item_started\"",
            "\"type\":\"token_count\"",
            "\"type\":\"thread_settings_applied\"",
        ]
        .iter()
        .any(|marker| contains(prefix, marker.as_bytes()));
    }
    if contains(prefix, b"\"type\":\"response_item\"") {
        return [
            "\"type\":\"custom_tool_call\"",
            "\"type\":\"function_call\"",
            "\"type\":\"reasoning\"",
            "\"type\":\"message\"",
        ]
        .iter()
        .any(|marker| contains(prefix, marker.as_bytes()));
    }
    false
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate_limits(json: &str) -> serde_json::Map<String, Value> {
        let payload: Value = serde_json::from_str(&format!(r#"{{"rate_limits":{}}}"#, json)).unwrap();
        payload.as_object().unwrap().clone()
    }

    #[test]
    fn five_hour_window_is_read_when_reported_and_dropped_when_it_disappears() {
        let mut state = SessionState::new("/nonexistent".to_string());

        // today's Codex payload: weekly only
        let weekly_only = rate_limits(
            r#"{"limit_id":"codex","primary":{"used_percent":88.0,"window_minutes":10080,"resets_at":1790661697},"secondary":null}"#,
        );
        state.consume_token_count(&weekly_only, Some(100.0));
        assert_eq!(state.weekly_used_percent, Some(88.0));
        assert_eq!(state.five_hour_used_percent, None);

        // a 5-hour window in either slot is picked up alongside weekly
        let both = rate_limits(
            r#"{"limit_id":"codex","primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1790000000},"secondary":{"used_percent":89.0,"window_minutes":10080,"resets_at":1790661697}}"#,
        );
        state.consume_token_count(&both, Some(200.0));
        assert_eq!(state.weekly_used_percent, Some(89.0));
        assert_eq!(state.five_hour_used_percent, Some(12.5));
        assert_eq!(state.five_hour_resets_at, Some(1790000000.0));
        assert_eq!(state.five_hour_sample_at, Some(200.0));

        // lifted again: the old 5h sample must not linger
        state.consume_token_count(&weekly_only, Some(300.0));
        assert_eq!(state.five_hour_used_percent, None);
        assert_eq!(state.five_hour_sample_at, None);
    }
}
