use crate::codex::db::list_usage_sources;
use crate::model::{
    api_equivalent_cost, parse_timestamp, usage_bucket_key, TokenUsage, UsageBucket, UsageTotals,
    READ_CHUNK_SIZE,
};
use crate::usage_tracker::{FileState, UsageSource};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

pub struct CodexUsageSource {
    pub db_path: PathBuf,
}

impl UsageSource for CodexUsageSource {
    fn list_sources(&self, cutoff: f64) -> Result<Vec<(String, String, Option<String>)>, String> {
        list_usage_sources(&self.db_path, cutoff).map_err(|e| e.to_string())
    }

    fn make_state(&self, path: String, model: Option<String>) -> Box<dyn FileState> {
        Box::new(CodexUsageFileState::new(path, model))
    }
}

pub struct CodexUsageFileState {
    path: String,
    default_model: Option<String>,
    inode: Option<u64>,
    offset: u64,
    partial: Vec<u8>,
    current_model: Option<String>,
    previous_total: Option<TokenUsage>,
    buckets: HashMap<i64, UsageBucket>,
}

impl CodexUsageFileState {
    pub fn new(path: String, default_model: Option<String>) -> Self {
        CodexUsageFileState {
            path,
            default_model,
            inode: None,
            offset: 0,
            partial: Vec::new(),
            current_model: None,
            previous_total: None,
            buckets: HashMap::new(),
        }
    }

    fn consume_line(&mut self, raw_line: &[u8], cutoff: f64) {
        let prefix = &raw_line[..raw_line.len().min(1024)];
        let is_tokens = contains(prefix, b"\"type\":\"token_count\"");
        let is_settings = contains(prefix, b"\"type\":\"thread_settings_applied\"");
        if !is_tokens && !is_settings {
            return;
        }
        if is_settings {
            if let Some(model) = extract_settings_model(prefix) {
                self.current_model = Some(model);
                return;
            }
        }
        let event: Value = match serde_json::from_slice(raw_line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let payload = match event.get("payload").and_then(|v| v.as_object()) {
            Some(p) => p,
            None => return,
        };
        if payload.get("type").and_then(|v| v.as_str()) == Some("thread_settings_applied") {
            if let Some(model) = payload
                .get("thread_settings")
                .and_then(|v| v.get("model"))
                .and_then(|v| v.as_str())
            {
                self.current_model = Some(model.to_string());
            }
            return;
        }
        if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") {
            return;
        }
        let info = match payload.get("info").and_then(|v| v.as_object()) {
            Some(i) => i,
            None => return,
        };
        let total = info
            .get("total_token_usage")
            .and_then(TokenUsage::from_value);
        let last = info.get("last_token_usage").and_then(TokenUsage::from_value);

        let mut usage: Option<TokenUsage> = None;
        if let Some(total) = total {
            usage = match &self.previous_total {
                None => Some(total),
                Some(prev) => total.delta_from(prev),
            };
            self.previous_total = Some(total);
        }
        let usage = usage.or(last);
        let usage = match usage {
            Some(u) if u.total_tokens > 0 => u,
            _ => return,
        };
        let event_at = parse_timestamp(event.get("timestamp").and_then(|v| v.as_str()));
        let event_at = match event_at {
            Some(t) if t >= cutoff => t,
            _ => return,
        };
        let model = self.current_model.as_deref().or(self.default_model.as_deref());
        let cost = api_equivalent_cost(model, &usage);
        self.buckets
            .entry(usage_bucket_key(event_at))
            .or_default()
            .add(usage.total_tokens, cost);
    }
}

impl FileState for CodexUsageFileState {
    fn path(&self) -> &str {
        &self.path
    }

    fn set_default_model(&mut self, model: Option<String>) {
        self.default_model = model;
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
            self.current_model = None;
            self.previous_total = None;
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

impl CodexUsageFileState {
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

fn extract_settings_model(prefix: &[u8]) -> Option<String> {
    let needle = b"\"thread_settings\":{\"model\":\"";
    let pos = find(prefix, needle)?;
    let start = pos + needle.len();
    let rest = &prefix[start..];
    let end = rest.iter().position(|&b| b == b'"')?;
    String::from_utf8(rest[..end].to_vec()).ok()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(haystack, needle)
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
