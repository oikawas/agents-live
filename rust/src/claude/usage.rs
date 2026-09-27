use crate::claude::sessions::glob_paths;
use crate::claude::transcript::claude_relevant_line;
use crate::model::{
    claude_api_equivalent_cost, claude_usage_total_tokens, parse_timestamp, usage_bucket_key,
    UsageBucket, UsageTotals, READ_CHUNK_SIZE,
};
use crate::usage_tracker::{FileState, UsageSource};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

pub struct ClaudeUsageSource {
    pub claude_home: PathBuf,
}

impl UsageSource for ClaudeUsageSource {
    fn list_sources(&self, cutoff: f64) -> Result<Vec<(String, String, Option<String>)>, String> {
        // A subagent's API calls are recorded only in its own transcript
        // under <session>/subagents/, never in the parent's, so both are
        // needed and neither double counts.
        let projects = self.claude_home.join("projects");
        let patterns = [
            projects.join("*").join("*.jsonl"),
            projects.join("*").join("*").join("subagents").join("*.jsonl"),
        ];
        let mut rows = Vec::new();
        for path in patterns.iter().flat_map(|p| glob_paths(&p.to_string_lossy())) {
            let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let mtime_secs = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if mtime_secs < cutoff {
                continue;
            }
            rows.push((path.clone(), path, None));
        }
        Ok(rows)
    }

    fn make_state(&self, path: String, _model: Option<String>) -> Box<dyn FileState> {
        Box::new(ClaudeUsageFileState::new(path))
    }
}

pub struct ClaudeUsageFileState {
    path: String,
    inode: Option<u64>,
    offset: u64,
    partial: Vec<u8>,
    seen_request_ids: HashSet<String>,
    buckets: HashMap<i64, UsageBucket>,
}

impl ClaudeUsageFileState {
    pub fn new(path: String) -> Self {
        ClaudeUsageFileState {
            path,
            inode: None,
            offset: 0,
            partial: Vec::new(),
            seen_request_ids: HashSet::new(),
            buckets: HashMap::new(),
        }
    }

    fn consume_line(&mut self, raw_line: &[u8], cutoff: f64) {
        if raw_line.is_empty() || !claude_relevant_line(raw_line) {
            return;
        }
        let event: Value = match serde_json::from_slice(raw_line) {
            Ok(v) => v,
            Err(_) => return,
        };
        if event.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            return;
        }
        let message = match event.get("message") {
            Some(m) => m,
            None => return,
        };
        let usage = match message.get("usage") {
            Some(u) if u.is_object() => u,
            _ => return,
        };
        let request_id = event
            .get("requestId")
            .and_then(|v| v.as_str())
            .or_else(|| event.get("uuid").and_then(|v| v.as_str()));
        let request_id = match request_id {
            Some(id) => id,
            None => return,
        };
        if self.seen_request_ids.contains(request_id) {
            return;
        }
        self.seen_request_ids.insert(request_id.to_string());
        let total_tokens = claude_usage_total_tokens(usage);
        if total_tokens <= 0 {
            return;
        }
        let event_at = match parse_timestamp(event.get("timestamp").and_then(|v| v.as_str())) {
            Some(t) if t >= cutoff => t,
            _ => return,
        };
        let model = message.get("model").and_then(|v| v.as_str());
        let cost = claude_api_equivalent_cost(model, usage);
        self.buckets
            .entry(usage_bucket_key(event_at))
            .or_default()
            .add(total_tokens, cost);
    }

    fn feed_chunk(&mut self, chunk: &[u8], cutoff: f64) {
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(chunk);
        let mut lines: Vec<Vec<u8>> = data.split(|&b| b == b'\n').map(|s| s.to_vec()).collect();
        self.partial = lines.pop().unwrap_or_default();
        for line in lines {
            self.consume_line(&line, cutoff);
        }
    }
}

impl FileState for ClaudeUsageFileState {
    fn path(&self) -> &str {
        &self.path
    }

    fn set_default_model(&mut self, _model: Option<String>) {
        // Claude usage lines carry their own model; unused.
    }

    fn refresh(&mut self, cutoff: f64) {
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
            self.seen_request_ids.clear();
            self.buckets.clear();
        }
        if size > self.offset {
            if let Ok(mut file) = File::open(&self.path) {
                if file.seek(SeekFrom::Start(self.offset)).is_ok() {
                    let mut buf = vec![0u8; READ_CHUNK_SIZE];
                    loop {
                        let n = match file.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        self.feed_chunk(&buf[..n], cutoff);
                    }
                    if let Ok(pos) = file.stream_position() {
                        self.offset = pos;
                    }
                }
            }
        }
        let cutoff_bucket = usage_bucket_key(cutoff);
        self.buckets.retain(|&key, _| key >= cutoff_bucket);
        self.buckets.shrink_to_fit();
    }

    fn totals(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for bucket in self.buckets.values() {
            totals.total_tokens += bucket.total_tokens;
            totals.priced_tokens += bucket.priced_tokens;
            totals.unpriced_tokens += bucket.unpriced_tokens;
            totals.cost_usd += bucket.cost_usd;
        }
        totals
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
    use crate::usage_tracker::FileState;
    use std::io::Write;

    fn line(request_id: &str, ts: &str, input: i64, output: i64) -> String {
        format!(
            // "role":"assistant" is what the line prefilter keys on,
            // so a realistic fixture must carry it.
            r#"{{"type":"assistant","requestId":"{}","timestamp":"{}","message":{{"role":"assistant","model":"claude-sonnet-5","usage":{{"input_tokens":{},"output_tokens":{}}}}}}}"#,
            request_id, ts, input, output
        )
    }

    #[test]
    fn one_api_response_split_across_content_blocks_is_counted_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Same requestId arriving as three lines (thinking / text / tool_use
        // blocks of one response) must be counted once, not three times.
        for _ in 0..3 {
            writeln!(f, "{}", line("req-1", "2026-09-10T00:00:00Z", 100, 10)).unwrap();
        }
        writeln!(f, "{}", line("req-2", "2026-09-10T01:00:00Z", 200, 20)).unwrap();
        f.flush().unwrap();

        let mut state = ClaudeUsageFileState::new(path.to_string_lossy().to_string());
        state.refresh(0.0);
        let totals = state.totals();
        assert_eq!(330, totals.total_tokens);

        // refreshing again reads no new bytes and must not double count
        state.refresh(0.0);
        assert_eq!(330, state.totals().total_tokens);
    }

    #[test]
    fn events_older_than_the_cutoff_are_dropped_by_whole_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", line("old", "2026-08-01T00:00:00Z", 1000, 100)).unwrap();
        writeln!(f, "{}", line("new", "2026-09-10T00:00:00Z", 100, 10)).unwrap();
        f.flush().unwrap();

        // cutoff between the two events
        let cutoff = crate::model::parse_timestamp(Some("2026-09-01T00:00:00Z")).unwrap();
        let mut state = ClaudeUsageFileState::new(path.to_string_lossy().to_string());
        state.refresh(cutoff);
        assert_eq!(110, state.totals().total_tokens);
    }

    #[test]
    fn subagent_transcripts_are_tracked_alongside_session_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("projects").join("-Users-x-proj");
        let subagents = project.join("sess-1").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(project.join("sess-1.jsonl"), "").unwrap();
        std::fs::write(subagents.join("agent-a1.jsonl"), "").unwrap();
        // not a transcript
        std::fs::write(subagents.join("agent-a1.meta.json"), "").unwrap();

        let source = ClaudeUsageSource { claude_home: home.path().to_path_buf() };
        let mut paths: Vec<String> = source.list_sources(0.0).unwrap().into_iter().map(|row| row.1).collect();
        paths.sort();
        assert_eq!(2, paths.len(), "{:?}", paths);
        assert!(paths[0].ends_with("sess-1.jsonl"));
        assert!(paths[1].ends_with("subagents/agent-a1.jsonl"));
    }

    #[test]
    fn a_truncated_or_rotated_file_restarts_tracking_instead_of_desyncing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, format!("{}\n", line("req-1", "2026-09-10T00:00:00Z", 100, 10)))
            .unwrap();

        let mut state = ClaudeUsageFileState::new(path.to_string_lossy().to_string());
        state.refresh(0.0);
        assert_eq!(110, state.totals().total_tokens);

        // rewritten shorter: offset must reset rather than read past the end
        std::fs::write(&path, format!("{}\n", line("req-9", "2026-09-10T02:00:00Z", 1, 1))).unwrap();
        state.refresh(0.0);
        assert_eq!(2, state.totals().total_tokens);
    }
}
