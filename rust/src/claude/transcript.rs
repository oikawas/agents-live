use crate::model::{claude_usage_total_tokens, parse_timestamp, READ_CHUNK_SIZE};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::SystemTime;

/// Matches each <task-notification>...<tool-use-id>ID</tool-use-id>...</task-notification>
/// block delivered in a "queued_command" attachment when a background
/// Agent/Task subagent actually finishes.
static TASK_NOTIFICATION_TOOL_USE_ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?s)<task-notification>.*?<tool-use-id>([^<]+)</tool-use-id>.*?</task-notification>")
        .unwrap()
});

/// The task id a launch receipt hands out -- "... in background with ID:
/// b6oe7ee7f." for a Bash, "agentId: a481062ae6ff9f937" for an agent, "...
/// moved to the background (ID: bogq2y1km)." for a foreground Bash that
/// timed out. It is the id a later TaskStop names.
static LAUNCH_TASK_ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:in background with ID: |agentId: |moved to the background \(ID: )([A-Za-z0-9]+)").unwrap()
});

/// A foreground Bash that outran its timeout and kept running as a
/// background task. Anchored at the start of the result: in real data the
/// notice always opens it, while a mere mention (a grep hit, a diff, a
/// quoted spec) sits mid-output and must not start a child.
static MOVED_TO_BACKGROUND_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^Command did not complete within its \d+s timeout and was moved to the background \(ID: [A-Za-z0-9]+\)").unwrap()
});

fn is_moved_to_background(tool_result_block: &Value) -> bool {
    tool_result_texts(tool_result_block)
        .first()
        .is_some_and(|text| MOVED_TO_BACKGROUND_RE.is_match(text.trim_start()))
}

/// Reject lines that cannot be a Claude Code user/assistant turn (or a
/// completed background subagent's task-notification) before decoding.
/// Mirrors `claude_relevant_line`.
pub fn claude_relevant_line(raw_line: &[u8]) -> bool {
    let prefix = &raw_line[..raw_line.len().min(256)];
    contains(prefix, b"\"role\":\"user\"")
        || contains(prefix, b"\"role\":\"assistant\"")
        || contains(prefix, b"\"attachment\":{\"type\":\"queued_command\"")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

/// True if a tool_result is just a "launched in the background" receipt.
/// Claude Code's async Agent/Task tool returns this immediately, long
/// before the subagent actually finishes; its real completion is a
/// separate task-notification, so this placeholder must not be mistaken
/// for the subagent being done. A background Bash (`run_in_background`)
/// gets the same kind of receipt.
fn is_async_launch_ack(tool_result_block: &Value) -> bool {
    const MARKERS: [&str; 2] = ["Async agent launched successfully", "Command running in background with ID:"];
    is_moved_to_background(tool_result_block)
        || tool_result_texts(tool_result_block)
            .iter()
            .any(|text| MARKERS.iter().any(|m| text.contains(m)))
}

/// A tool_result's text: its content is either one string or a list of
/// parts, some of them text.
fn tool_result_texts(tool_result_block: &Value) -> Vec<&str> {
    match tool_result_block.get("content") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
            .collect(),
        _ => Vec::new(),
    }
}

/// True if a TaskStop call's tool_result says it stopped the task.
fn is_successful_task_stop(tool_result_block: &Value) -> bool {
    tool_result_block.get("is_error").and_then(|v| v.as_bool()) != Some(true)
        && tool_result_texts(tool_result_block)
            .iter()
            .any(|text| text.contains("Successfully stopped task"))
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub description: Option<String>,
    pub subagent_type: Option<String>,
    pub started_at: f64,
    pub done_at: Option<f64>,
}

/// Incrementally tails one Claude Code session JSONL transcript. Tracks the
/// running model/effort/token total for the main thread and, for each
/// "Agent"/"Task" tool_use block seen, whether it has completed yet.
/// Mirrors `ClaudeSessionState`.
#[derive(Debug, Clone)]
pub struct ClaudeSessionState {
    pub path: String,
    inode: Option<u64>,
    offset: u64,
    partial: Vec<u8>,
    pub current_model: Option<String>,
    pub current_effort: Option<String>,
    pub total_tokens: i64,
    seen_request_ids: HashSet<String>,
    pub activity: String,
    pub last_event_at: f64,
    pub first_message: Option<String>,
    pub agents: HashMap<String, AgentInfo>,
    /// Background task id (from its launch receipt) -> the tool_use id of
    /// the child it belongs to, so a TaskStop naming the task can close it.
    task_tool_ids: HashMap<String, String>,
    /// TaskStop calls awaiting their result: tool_use id -> task id.
    pending_task_stops: HashMap<String, String>,
    /// Foreground Bash calls awaiting their result, as the child they
    /// become if the result says they were moved to the background.
    pending_foreground_bash: HashMap<String, AgentInfo>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl ClaudeSessionState {
    pub fn new(path: String) -> Self {
        ClaudeSessionState {
            path,
            inode: None,
            offset: 0,
            partial: Vec::new(),
            current_model: None,
            current_effort: None,
            total_tokens: 0,
            seen_request_ids: HashSet::new(),
            activity: "starting".to_string(),
            last_event_at: now_secs(),
            first_message: None,
            agents: HashMap::new(),
            task_tool_ids: HashMap::new(),
            pending_task_stops: HashMap::new(),
            pending_foreground_bash: HashMap::new(),
        }
    }

    pub fn refresh(&mut self) {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let inode = inode_of(&metadata);
        let size = metadata.len();
        if self.inode != Some(inode) || size < self.offset {
            self.inode = Some(inode);
            self.offset = 0;
            self.partial.clear();
        }
        if size == self.offset {
            return;
        }
        // See the matching comment in codex/rollout.rs: read in bounded
        // chunks rather than one `read_to_end` covering the whole
        // remaining file, so an actively-growing multi-hundred-MB
        // transcript doesn't spike peak memory on the first poll.
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
            if raw_line.is_empty() || !claude_relevant_line(&raw_line) {
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
        let event_at = match parse_timestamp(event.get("timestamp").and_then(|v| v.as_str())) {
            Some(t) => {
                self.last_event_at = t;
                t
            }
            None => self.last_event_at,
        };

        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "user" => {
                if let Some(message) = event.get("message") {
                    self.consume_user(message, event_at);
                }
            }
            "assistant" => {
                if let Some(message) = event.get("message") {
                    self.consume_assistant(event, message, event_at);
                }
            }
            "attachment" => {
                if let Some(attachment) = event.get("attachment") {
                    self.consume_attachment(attachment, event_at);
                }
            }
            _ => {}
        }
    }

    fn consume_attachment(&mut self, attachment: &Value, event_at: f64) {
        if let Some(text) = attachment.get("prompt").and_then(|v| v.as_str()) {
            self.consume_task_notifications(text, event_at);
        }
    }

    /// A background agent's completion notice. Claude Code records it either
    /// as a queued_command attachment's prompt or, as seen in 2.1.280 bg
    /// sessions, as a plain user message whose content is the notice itself
    /// -- both carry the same <task-notification> markup.
    fn consume_task_notifications(&mut self, text: &str, event_at: f64) {
        if !text.contains("<task-notification>") {
            return;
        }
        for cap in TASK_NOTIFICATION_TOOL_USE_ID_RE.captures_iter(text) {
            let tool_use_id = cap[1].trim();
            if let Some(info) = self.agents.get_mut(tool_use_id) {
                if info.done_at.is_none() {
                    info.done_at = Some(event_at);
                }
            }
        }
    }

    fn consume_user(&mut self, message: &Value, event_at: f64) {
        match message.get("content") {
            Some(Value::String(content)) => {
                self.consume_task_notifications(content, event_at);
                if !content.trim().is_empty() {
                    self.activity = "reading prompt".to_string();
                    if self.first_message.is_none() {
                        self.first_message = Some(content.trim().to_string());
                    }
                }
                return;
            }
            Some(Value::Array(blocks)) => {
                let mut has_tool_result = false;
                let mut text_parts: Vec<String> = Vec::new();
                for block in blocks {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == "tool_result" {
                        has_tool_result = true;
                        if let Some(tool_use_id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                            // A timed-out foreground Bash becomes a background
                            // task from here on.
                            if let Some(child) = self.pending_foreground_bash.remove(tool_use_id) {
                                if is_moved_to_background(block) {
                                    self.agents.entry(tool_use_id.to_string()).or_insert(child);
                                }
                            }
                            let is_ack = is_async_launch_ack(block);
                            if let Some(info) = self.agents.get_mut(tool_use_id) {
                                if info.done_at.is_none() && !is_ack {
                                    info.done_at = Some(event_at);
                                }
                                if is_ack {
                                    let task_id = tool_result_texts(block)
                                        .iter()
                                        .find_map(|text| LAUNCH_TASK_ID_RE.captures(text))
                                        .map(|cap| cap[1].to_string());
                                    if let Some(task_id) = task_id {
                                        self.task_tool_ids.insert(task_id, tool_use_id.to_string());
                                    }
                                }
                            }
                            // A stopped task never gets a task-notification,
                            // so the stop itself ends it.
                            if let Some(task_id) = self.pending_task_stops.remove(tool_use_id) {
                                if is_successful_task_stop(block) {
                                    let stopped = self.task_tool_ids.get(&task_id).and_then(|id| self.agents.get_mut(id));
                                    if let Some(info) = stopped {
                                        if info.done_at.is_none() {
                                            info.done_at = Some(event_at);
                                        }
                                    }
                                }
                            }
                        }
                    } else if block_type == "text" {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            self.consume_task_notifications(text, event_at);
                        }
                        text_parts.push(
                            block
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        );
                    }
                }
                if has_tool_result {
                    return;
                }
                self.activity = "reading prompt".to_string();
                let joined = text_parts
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                let joined = joined.trim().to_string();
                if !joined.is_empty() && self.first_message.is_none() {
                    self.first_message = Some(joined);
                }
            }
            _ => {}
        }
    }

    fn consume_assistant(&mut self, event: &Value, message: &Value, event_at: f64) {
        if let Some(model) = message.get("model").and_then(|v| v.as_str()) {
            self.current_model = Some(model.to_string());
        }
        if let Some(effort) = event.get("effort").and_then(|v| v.as_str()) {
            self.current_effort = Some(effort.to_string());
        }

        if let Some(usage) = message.get("usage") {
            let request_id = event
                .get("requestId")
                .and_then(|v| v.as_str())
                .or_else(|| event.get("uuid").and_then(|v| v.as_str()));
            if let Some(request_id) = request_id {
                if !self.seen_request_ids.contains(request_id) {
                    self.seen_request_ids.insert(request_id.to_string());
                    self.total_tokens += claude_usage_total_tokens(usage);
                }
            }
        }

        let content = match message.get("content").and_then(|v| v.as_array()) {
            Some(c) => c,
            None => return,
        };
        for block in content {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match block_type {
                "thinking" => self.activity = "thinking".to_string(),
                "text" => self.activity = "responding".to_string(),
                "tool_use" => {
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
                    self.activity = name.clone();
                    if name == "Bash" {
                        let tool_input = block.get("input");
                        let background = tool_input
                            .and_then(|v| v.get("run_in_background"))
                            .and_then(|v| v.as_bool())
                            == Some(true);
                        if let Some(tool_id) = block.get("id").and_then(|v| v.as_str()) {
                            let child = bash_child(tool_input, event_at);
                            if background {
                                // Tracked like an async subagent: it outlives the
                                // turn and ends with a task-notification.
                                self.agents.entry(tool_id.to_string()).or_insert(child);
                            } else {
                                self.pending_foreground_bash.insert(tool_id.to_string(), child);
                            }
                        }
                    }
                    if name == "TaskStop" {
                        let task_id = block.get("input").and_then(|v| v.get("task_id")).and_then(|v| v.as_str());
                        if let (Some(tool_id), Some(task_id)) = (block.get("id").and_then(|v| v.as_str()), task_id) {
                            self.pending_task_stops.insert(tool_id.to_string(), task_id.to_string());
                        }
                    }
                    if name == "Agent" || name == "Task" {
                        let tool_input = block.get("input");
                        if let Some(tool_id) = block.get("id").and_then(|v| v.as_str()) {
                            if !self.agents.contains_key(tool_id) {
                                self.agents.insert(
                                    tool_id.to_string(),
                                    AgentInfo {
                                        description: tool_input
                                            .and_then(|v| v.get("description"))
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string()),
                                        subagent_type: tool_input
                                            .and_then(|v| v.get("subagent_type"))
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string()),
                                        started_at: event_at,
                                        done_at: None,
                                    },
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The child row a Bash becomes once it runs in the background, named by
/// its description or else its command's first line.
fn bash_child(tool_input: Option<&Value>, started_at: f64) -> AgentInfo {
    AgentInfo {
        description: tool_input
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                tool_input
                    .and_then(|v| v.get("command"))
                    .and_then(|v| v.as_str())
                    .and_then(|c| c.lines().next())
            })
            .map(|s| s.to_string()),
        subagent_type: Some("bash".to_string()),
        started_at,
        done_at: None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_use_event(tool_id: &str, name: &str, timestamp: &str) -> Value {
        json!({
            "type": "assistant",
            "timestamp": timestamp,
            "message": {
                "model": "claude-sonnet-5",
                "content": [
                    {"type": "tool_use", "id": tool_id, "name": name, "input": {"description": "d", "subagent_type": "t"}}
                ]
            }
        })
    }

    #[test]
    fn claude_relevant_line_matches_role_and_queued_command() {
        assert!(claude_relevant_line(br#"{"type":"user","message":{"role":"user"}}"#));
        assert!(claude_relevant_line(br#"{"type":"assistant","message":{"role":"assistant"}}"#));
        assert!(claude_relevant_line(br#"{"type":"attachment","attachment":{"type":"queued_command"}}"#));
        assert!(!claude_relevant_line(br#"{"type":"summary"}"#));
    }

    #[test]
    fn background_agent_stays_running_until_task_notification_arrives() {
        // A background (async) Agent/Task tool's immediate tool_result is
        // only a launch acknowledgment; the subagent must stay "running"
        // (done_at unset) until the real completion arrives later as a
        // queued_command attachment's task-notification.
        let mut state = ClaudeSessionState::new("dummy.jsonl".to_string());
        state.consume(&tool_use_event("tool-1", "Agent", "2026-01-01T00:00:00Z"));
        assert!(state.agents.contains_key("tool-1"));
        assert!(state.agents["tool-1"].done_at.is_none());

        let launch_ack = json!({
            "type": "user",
            "timestamp": "2026-01-01T00:00:03Z",
            "message": {
                "content": [
                    {"type": "tool_result", "tool_use_id": "tool-1", "content": "Async agent launched successfully. agentId: abc"}
                ]
            }
        });
        state.consume(&launch_ack);
        assert!(
            state.agents["tool-1"].done_at.is_none(),
            "a launch acknowledgment must not be mistaken for completion"
        );

        let notification = json!({
            "type": "attachment",
            "timestamp": "2026-01-01T00:01:19Z",
            "attachment": {
                "prompt": "<task-notification><tool-use-id>tool-1</tool-use-id><status>completed</status></task-notification>"
            }
        });
        state.consume(&notification);
        assert!(state.agents["tool-1"].done_at.is_some());
    }

    #[test]
    fn task_notification_as_plain_user_message_completes_the_agent() {
        // Seen in a real 2.1.280 bg session: the notice arrived as a user
        // message with string content, not as a queued_command attachment.
        let mut state = ClaudeSessionState::new("dummy.jsonl".to_string());
        state.consume(&tool_use_event("tool-9", "Agent", "2026-01-01T00:00:00Z"));
        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:03:00Z",
            "message": {"role": "user", "content": "<task-notification>\n<task-id>abc</task-id>\n<tool-use-id>tool-9</tool-use-id>\n<status>completed</status>\n</task-notification>"}
        }));
        assert!(state.agents["tool-9"].done_at.is_some());
    }

    #[test]
    fn background_bash_runs_until_its_task_notification() {
        let mut state = ClaudeSessionState::new("dummy.jsonl".to_string());
        state.consume(&json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"content": [{"type": "tool_use", "id": "sh-1", "name": "Bash",
                "input": {"command": "cargo test", "description": "run tests", "run_in_background": true}}]}
        }));
        // a foreground Bash is not a background task
        state.consume(&json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:00:01Z",
            "message": {"content": [{"type": "tool_use", "id": "sh-2", "name": "Bash", "input": {"command": "ls"}}]}
        }));
        assert!(!state.agents.contains_key("sh-2"));
        assert_eq!(state.agents["sh-1"].subagent_type.as_deref(), Some("bash"));
        assert_eq!(state.agents["sh-1"].description.as_deref(), Some("run tests"));

        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:00:02Z",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "sh-1",
                "content": "Command running in background with ID: b51aq31rn. Output is being written to: /tmp/x"}]}
        }));
        assert!(state.agents["sh-1"].done_at.is_none(), "the launch receipt is not completion");

        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:05:00Z",
            "message": {"content": "<task-notification>\n<task-id>b51aq31rn</task-id>\n<tool-use-id>sh-1</tool-use-id>\n<status>completed</status>\n</task-notification>"}
        }));
        assert!(state.agents["sh-1"].done_at.is_some());
    }

    #[test]
    fn a_foreground_bash_moved_to_the_background_runs_until_its_task_notification() {
        // Real shape: the Bash had no run_in_background, outran its 120s
        // timeout, and its later notice names the original tool_use id.
        let mut state = ClaudeSessionState::new("dummy.jsonl".to_string());
        state.consume(&json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"content": [
                {"type": "tool_use", "id": "sh-1", "name": "Bash",
                 "input": {"command": "cargo build --release", "description": "release build"}},
                {"type": "tool_use", "id": "sh-2", "name": "Bash", "input": {"command": "ls"}}
            ]}
        }));
        assert!(state.agents.is_empty(), "a foreground Bash is not a child yet");

        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:02:00Z",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "sh-1",
                 "content": "Command did not complete within its 120s timeout and was moved to the background (ID: bogq2y1km). Output is being written to: /tmp/bogq2y1km.output. You will be notified when it completes."},
                {"type": "tool_result", "tool_use_id": "sh-2", "content": "Cargo.toml\nsrc"}
            ]}
        }));
        assert!(!state.agents.contains_key("sh-2"), "a Bash that simply finished stays untracked");
        let child = &state.agents["sh-1"];
        assert_eq!(child.subagent_type.as_deref(), Some("bash"));
        assert_eq!(child.description.as_deref(), Some("release build"));
        assert_eq!(child.started_at, parse_timestamp(Some("2026-01-01T00:00:00Z")).unwrap());
        assert!(child.done_at.is_none(), "being moved to the background is not completion");
        assert!(state.pending_foreground_bash.is_empty());

        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:07:00Z",
            "message": {"content": "<task-notification>\n<task-id>bogq2y1km</task-id>\n<tool-use-id>sh-1</tool-use-id>\n<status>completed</status>\n</task-notification>"}
        }));
        assert!(state.agents["sh-1"].done_at.is_some());
    }

    #[test]
    fn output_that_merely_quotes_the_moved_notice_is_not_a_background_move() {
        // Seen live: a grep over transcripts and a `git diff` of this very
        // file each printed the notice mid-output and showed up as RUN.
        let mut state = ClaudeSessionState::new("dummy.jsonl".to_string());
        state.consume(&json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"content": [
                {"type": "tool_use", "id": "sh-1", "name": "Bash", "input": {"command": "git diff"}},
                {"type": "tool_use", "id": "ag-1", "name": "Agent", "input": {"description": "survey"}}
            ]}
        }));
        let quoted = "--- a/x\n+/// \"Command did not complete within its 120s timeout and was moved to the background (ID: X)\"";
        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:00:05Z",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "sh-1", "content": quoted},
                {"type": "tool_result", "tool_use_id": "ag-1", "content": [{"type": "text", "text": quoted}]}
            ]}
        }));
        assert!(!state.agents.contains_key("sh-1"));
        assert!(state.agents["ag-1"].done_at.is_some(), "a synchronous agent's quoted notice is its completion, not a launch receipt");
    }

    #[test]
    fn task_stop_ends_a_foreground_bash_that_was_moved_to_the_background() {
        let mut state = ClaudeSessionState::new("dummy.jsonl".to_string());
        state.consume(&json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"content": [{"type": "tool_use", "id": "sh-1", "name": "Bash", "input": {"command": "sleep 999"}}]}
        }));
        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:02:00Z",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "sh-1",
                "content": "Command did not complete within its 120s timeout and was moved to the background (ID: bx1). Output is being written to: /tmp/bx1.output."}]}
        }));
        state.consume(&json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:03:00Z",
            "message": {"content": [{"type": "tool_use", "id": "stop-1", "name": "TaskStop", "input": {"task_id": "bx1"}}]}
        }));
        assert!(state.agents["sh-1"].done_at.is_none());
        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:03:01Z",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "stop-1",
                "content": "{\"message\":\"Successfully stopped task: bx1 (sleep 999)\",\"task_id\":\"bx1\"}"}]}
        }));
        assert!(state.agents["sh-1"].done_at.is_some());
    }

    #[test]
    fn task_stop_ends_the_background_child_it_names() {
        // A stopped task gets no task-notification, only TaskStop's own
        // result -- seen in a real session where a killed Bash stayed RUN.
        let mut state = ClaudeSessionState::new("dummy.jsonl".to_string());
        state.consume(&json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"content": [
                {"type": "tool_use", "id": "sh-1", "name": "Bash",
                 "input": {"command": "cargo test", "run_in_background": true}},
                {"type": "tool_use", "id": "sh-2", "name": "Bash",
                 "input": {"command": "cargo clippy", "run_in_background": true}}
            ]}
        }));
        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:00:01Z",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "sh-1", "content": "Command running in background with ID: b6oe7ee7f. Output is being written to: /tmp/a"},
                {"type": "tool_result", "tool_use_id": "sh-2", "content": "Command running in background with ID: bpjwxwqic. Output is being written to: /tmp/b"}
            ]}
        }));
        let stop = |id: &str, task_id: &str, at: &str| {
            json!({
                "type": "assistant",
                "timestamp": at,
                "message": {"content": [{"type": "tool_use", "id": id, "name": "TaskStop", "input": {"task_id": task_id}}]}
            })
        };
        // a stop that failed leaves the task running
        state.consume(&stop("stop-1", "bpjwxwqic", "2026-01-01T00:01:00Z"));
        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:01:01Z",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "stop-1", "is_error": true, "content": "No task found with ID: bpjwxwqic"}]}
        }));
        assert!(state.agents["sh-2"].done_at.is_none());

        state.consume(&stop("stop-2", "b6oe7ee7f", "2026-01-01T00:02:00Z"));
        state.consume(&json!({
            "type": "user",
            "timestamp": "2026-01-01T00:02:01Z",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "stop-2",
                "content": "{\"message\":\"Successfully stopped task: b6oe7ee7f (cargo test)\",\"task_id\":\"b6oe7ee7f\"}"}]}
        }));
        assert!(state.agents["sh-1"].done_at.is_some());
        assert!(state.agents["sh-2"].done_at.is_none(), "only the named task stops");
    }

    #[test]
    fn synchronous_subagent_tool_result_is_its_own_completion() {
        let mut state = ClaudeSessionState::new("dummy.jsonl".to_string());
        state.consume(&tool_use_event("tool-2", "Task", "2026-01-01T00:00:00Z"));
        let result = json!({
            "type": "user",
            "timestamp": "2026-01-01T00:00:05Z",
            "message": {
                "content": [
                    {"type": "tool_result", "tool_use_id": "tool-2", "content": "done"}
                ]
            }
        });
        state.consume(&result);
        assert!(state.agents["tool-2"].done_at.is_some());
    }
}
