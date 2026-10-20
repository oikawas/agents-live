//! Shared data structures and pure helper functions used by both the Codex
//! and Claude Code acquisition modules and by the renderer. Mirrors the
//! dataclasses and free functions near the top of the Python script.

use std::path::Path;
use unicode_width::UnicodeWidthChar;

pub const USAGE_WINDOW_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;
/// Shared bounded read-chunk size for every incremental tail reader (live
/// session state and 30-day usage state alike). Reading in fixed chunks
/// (rather than one `read_to_end`/`handle.read()` covering the whole
/// remaining file) bounds peak memory to roughly one chunk plus the
/// longest single line, regardless of how large the file has grown -- an
/// actively-growing rollout/transcript can reach hundreds of MB to low GB
/// (a 680MB rollout was observed live on 2026-09-07), and reading it whole
/// on the first poll after startup was the dominant contributor to a
/// multi-hundred-MB peak memory spike.
pub const READ_CHUNK_SIZE: usize = 1024 * 1024;
/// 30-day usage totals are bucketed by hour rather than kept as one entry
/// per individual usage event, for the same reason: an account with
/// billions of cumulative tokens can log millions of events over 30 days.
/// A bucket is wholly kept or wholly evicted by comparing its key against
/// cutoff's own bucket, so totals can be off by at most about one hour's
/// worth of activity out of the 30-day window -- negligible, and the
/// tradeoff this feature accepts in exchange for bounding memory to
/// roughly (30*24) buckets per file instead of one entry per event.
pub const USAGE_BUCKET_SECONDS: f64 = 60.0 * 60.0;

pub fn usage_bucket_key(timestamp: f64) -> i64 {
    (timestamp / USAGE_BUCKET_SECONDS).floor() as i64
}
pub const LONG_CONTEXT_INPUT_TOKENS: i64 = 272_000;
pub const WEEKLY_WINDOW_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
pub const WEEKLY_RESET_EMOJI: &str = "\u{1F504}"; // 🔄

/// (input, cached input, output) USD per 1M tokens, from OpenAI's published
/// standard-tier pricing. A model absent here contributes to the internal
/// total but not to the displayed token count or cost, so leaving one
/// out silently shrinks both.
pub fn codex_model_pricing(model: &str) -> Option<(f64, f64, f64)> {
    let model = if model == "gpt-5.6" { "gpt-5.6-sol" } else { model };
    match model {
        "gpt-6-astra" => Some((10.00, 1.00, 50.00)),
        "gpt-6.1-sol" => Some((2.00, 0.10, 10.00)),
        "gpt-6-sol" => Some((2.00, 0.20, 10.00)),
        "gpt-6-luna" => Some((0.10, 0.01, 0.50)),
        "gpt-5.6-sol" => Some((4.00, 0.40, 20.00)),
        "gpt-5.6-terra" => Some((2.00, 0.20, 12.00)),
        "gpt-5.6-luna" => Some((0.20, 0.02, 1.20)),
        "gpt-5.5" => Some((5.00, 0.50, 30.00)),
        "gpt-5.4" => Some((2.50, 0.25, 15.00)),
        // PROVISIONAL, not from OpenAI's pricing tables: codex-auto-review
        // (the Guardian model, often a large share of Codex tokens) is not
        // published, so this uses the rate third-party aggregators list for
        // it, which happens to equal gpt-5.4's. Treat the resulting cost as
        // weaker evidence than the rest.
        "codex-auto-review" => Some((2.50, 0.25, 15.00)),
        // Still unpriced: gpt-5.3-codex-spark is absent from the pricing
        // tables too, but at 0.00B over 30 days it is not worth a guess.
        _ => None,
    }
}

/// (input, cache read, output) USD per 1M tokens, from Anthropic's
/// first-party API pricing. The cache-read rate is listed per model rather
/// than derived from input: it is 0.1x on most models but 0.05x on Opus 5.5
/// and Sonnet 5.5 and 0.025x on Fable 5.1. Haiku 5.5's rates here are for
/// prompts up to 100K tokens; see `claude_long_prompt_multiplier`. As with Codex, a model absent here drops out of
/// both the displayed token count and the cost -- claude-opus-5-5 was
/// missing and ~80% of 30-day usage vanished that way.
///
/// A dated snapshot id such as "claude-haiku-4-5-20251001" is the same model
/// at the same price as its alias, so the date is dropped before matching.
pub fn claude_model_pricing(model: &str) -> Option<(f64, f64, f64)> {
    match strip_snapshot_date(model) {
        "claude-fable-5-1" | "claude-mythos-5-1" => Some((10.00, 0.25, 50.00)),
        "claude-fable-5" | "claude-mythos-5" => Some((10.00, 1.00, 50.00)),
        "claude-opus-5-5" => Some((4.00, 0.20, 20.00)),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" | "claude-opus-4-5" => {
            Some((5.00, 0.50, 25.00))
        }
        "claude-sonnet-5-5" => Some((2.00, 0.10, 10.00)),
        "claude-sonnet-5" => Some((2.00, 0.20, 10.00)),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" => Some((3.00, 0.30, 15.00)),
        "claude-haiku-5-5" => Some((0.10, 0.01, 0.50)),
        "claude-haiku-4-5" => Some((1.00, 0.10, 5.00)),
        _ => None,
    }
}

/// "claude-haiku-4-5-20251001" -> "claude-haiku-4-5": a trailing
/// "-YYYYMMDD" is a snapshot date, not part of the model's name.
fn strip_snapshot_date(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((name, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => model,
    }
}

/// Haiku 5.5 prices a request whose prompt (input plus cache reads and
/// writes) is over 100K tokens at 5x every rate, cache hits included. Every
/// other current model bills its full context window at standard rates.
pub const CLAUDE_HAIKU_5_5_LONG_PROMPT_TOKENS: i64 = 100_000;

fn claude_long_prompt_multiplier(model: &str, prompt_tokens: i64) -> f64 {
    if strip_snapshot_date(model) == "claude-haiku-5-5" && prompt_tokens > CLAUDE_HAIKU_5_5_LONG_PROMPT_TOKENS {
        5.0
    } else {
        1.0
    }
}

pub const CLAUDE_CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;
pub const CLAUDE_CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;

/// Text display width, treating East-Asian Wide/Fullwidth characters as 2
/// columns and everything else as 1 (mirrors unicodedata.east_asian_width).
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Truncate/pad `text` to a display width of `width`, appending "…" if
/// truncated. When `pad` is false, no trailing padding is added.
pub fn fit(text: &str, width: i64, pad: bool) -> String {
    let text: String = text.chars().map(|c| match c {
        '\n' | '\r' | '\t' => ' ',
        other => other,
    }).collect();
    if width <= 0 {
        return String::new();
    }
    let width = width as usize;
    let dw = display_width(&text);
    if dw <= width {
        if pad {
            return text + &" ".repeat(width - dw);
        }
        return text;
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w > width - 1 {
            break;
        }
        result.push(ch);
        used += w;
    }
    result.push('…');
    if pad {
        let remaining = width.saturating_sub(used + 1);
        result.push_str(&" ".repeat(remaining));
    }
    result
}

pub fn clip_and_pad(text: &str, width: i64) -> String {
    if width <= 0 {
        return String::new();
    }
    let width = width as usize;
    let mut result = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w > width {
            break;
        }
        result.push(ch);
        used += w;
    }
    result.push_str(&" ".repeat(width.saturating_sub(used)));
    result
}

pub fn age(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as i64;
    if seconds < 60 {
        return format!("{}s", seconds);
    }
    if seconds < 3600 {
        return format!("{}m{:02}s", seconds / 60, seconds % 60);
    }
    if seconds < 86400 {
        return format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60);
    }
    format!("{}d{:02}h", seconds / 86400, (seconds % 86400) / 3600)
}

fn trim_trailing_zero(s: String) -> String {
    if s.contains('.') {
        let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        s
    } else {
        s
    }
}

pub fn compact_tokens(value: Option<i64>) -> String {
    let value = match value {
        None => return "-- tok".to_string(),
        Some(v) => v,
    };
    if value < 1_000 {
        return format!("{} tok", value);
    }
    if value < 1_000_000 {
        let amount = trim_trailing_zero(format!("{:.1}", value as f64 / 1_000.0));
        return format!("{}K tok", amount);
    }
    let amount = trim_trailing_zero(format!("{:.1}", value as f64 / 1_000_000.0));
    format!("{}M tok", amount)
}

pub fn weekly_remaining_percent(used_percent: Option<f64>) -> Option<f64> {
    used_percent.map(|u| (100.0 - u).clamp(0.0, 100.0))
}

pub const WEEKLY_HIGH_REMAINING_PERCENT: f64 = 95.0;
pub const WEEKLY_CRITICAL_REMAINING_PERCENT: f64 = 5.0;

pub fn weekly_label(used_percent: Option<f64>) -> String {
    format!("Weekly {}", remaining_percent_value(used_percent))
}

/// Remaining percentage as shown in a summary line ("86%", "86.5%", "--").
pub fn remaining_percent_value(used_percent: Option<f64>) -> String {
    match weekly_remaining_percent(used_percent) {
        None => "--".to_string(),
        Some(remaining) => {
            let rounded = (remaining * 10.0).round() / 10.0;
            let amount = if rounded.fract() == 0.0 {
                format!("{}", rounded as i64)
            } else {
                format!("{:.1}", rounded)
            };
            format!("{}%", amount)
        }
    }
}

/// Window length, in minutes, of the rolling 5-hour rate limit.
pub const FIVE_HOUR_WINDOW_MINUTES: i64 = 5 * 60;

pub fn five_hour_label(used_percent: Option<f64>) -> String {
    format!("5h {}", remaining_percent_value(used_percent))
}

/// A 5-hour sample whose window has already reset no longer describes the
/// current window, which starts empty -- so it reads as 0% used. This
/// matters for 5h (unlike weekly) because an idle session's last sample
/// routinely outlives its window.
pub fn five_hour_effective_used(used_percent: Option<f64>, resets_at: Option<f64>, now: f64) -> Option<f64> {
    match (used_percent, resets_at) {
        (Some(_), Some(resets_at)) if resets_at <= now => Some(0.0),
        (used, _) => used,
    }
}

/// Elapsed time since the current 7-day weekly-limit window began.
/// `resets_at` is the *next* reset time the API reports; the window began
/// exactly 7 days before that. Formatted as DdHHhMMm -- only hour and
/// minute are zero-padded; the day count is unpadded and uncapped.
pub fn weekly_reset_elapsed_label(resets_at: Option<f64>, now: f64) -> String {
    let resets_at = match resets_at {
        None => return "-d--h--m".to_string(),
        Some(v) => v,
    };
    let window_started_at = resets_at - WEEKLY_WINDOW_SECONDS;
    let elapsed = ((now - window_started_at).max(0.0)) as i64;
    let days = elapsed / 86400;
    let remainder = elapsed % 86400;
    let hours = remainder / 3600;
    let minutes = (remainder % 3600) / 60;
    format!("{}d{:02}h{:02}m", days, hours, minutes)
}

pub fn estimated_cost_label(cost_usd: Option<f64>) -> String {
    match cost_usd {
        None => "$--".to_string(),
        Some(cost) => {
            let rounded = (cost + 0.5).max(0.0) as i64;
            format!("${}", group_thousands(rounded))
        }
    }
}

fn group_thousands(value: i64) -> String {
    let s = value.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Share of 30-day tokens from models missing from the pricing table at
/// which the header flags its figures as incomplete. Below it,
/// deliberately unpriced trickles (gpt-5.3-codex-spark) stay unflagged.
pub const UNPRICED_MARKER_MIN_SHARE: f64 = 0.01;

/// "+?" when enough tokens came from unpriced models that the displayed
/// token count and cost understate real usage -- the one visible trace of
/// a model missing from the pricing table.
pub fn unpriced_marker(priced_tokens: Option<i64>, unpriced_tokens: Option<i64>) -> &'static str {
    let (priced, unpriced) = match (priced_tokens, unpriced_tokens) {
        (Some(p), Some(u)) => (p.max(0), u.max(0)),
        _ => return "",
    };
    let total = priced + unpriced;
    if total > 0 && unpriced as f64 / total as f64 >= UNPRICED_MARKER_MIN_SHARE {
        "+?"
    } else {
        ""
    }
}

pub fn billions_token_label(total_tokens: Option<i64>) -> String {
    match total_tokens {
        None => "--B".to_string(),
        Some(v) => format!("{:.1}B", v.max(0) as f64 / 1_000_000_000.0),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

impl TokenUsage {
    pub fn from_value(value: &serde_json::Value) -> Option<TokenUsage> {
        let obj = value.as_object()?;
        let get_nonneg_i64 = |key: &str| -> Option<i64> {
            match obj.get(key) {
                None => Some(0),
                Some(v) => {
                    let n = v.as_i64()?;
                    if n < 0 { None } else { Some(n) }
                }
            }
        };
        let input_tokens = get_nonneg_i64("input_tokens")?;
        let cached_input_tokens = get_nonneg_i64("cached_input_tokens")?;
        let cache_write_input_tokens = get_nonneg_i64("cache_write_input_tokens")?;
        let output_tokens = get_nonneg_i64("output_tokens")?;
        let total_tokens = match obj.get("total_tokens") {
            None => input_tokens + output_tokens,
            Some(v) => {
                let n = v.as_i64()?;
                if n < 0 {
                    return None;
                }
                n
            }
        };
        Some(TokenUsage {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            total_tokens,
        })
    }

    pub fn delta_from(&self, previous: &TokenUsage) -> Option<TokenUsage> {
        let input_tokens = self.input_tokens - previous.input_tokens;
        let cached_input_tokens = self.cached_input_tokens - previous.cached_input_tokens;
        let cache_write_input_tokens = self.cache_write_input_tokens - previous.cache_write_input_tokens;
        let output_tokens = self.output_tokens - previous.output_tokens;
        let total_tokens = self.total_tokens - previous.total_tokens;
        if input_tokens < 0
            || cached_input_tokens < 0
            || cache_write_input_tokens < 0
            || output_tokens < 0
            || total_tokens < 0
        {
            return None;
        }
        Some(TokenUsage {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            total_tokens,
        })
    }
}

pub fn api_equivalent_cost(model: Option<&str>, usage: &TokenUsage) -> Option<f64> {
    let pricing = codex_model_pricing(model?)?;
    let (input_rate, cached_rate, output_rate) = pricing;
    let cached = usage.cached_input_tokens.min(usage.input_tokens);
    let cache_write = usage
        .cache_write_input_tokens
        .min((usage.input_tokens - cached).max(0));
    let uncached = (usage.input_tokens - cached - cache_write).max(0);
    let long_input_multiplier = if usage.input_tokens > LONG_CONTEXT_INPUT_TOKENS { 2.0 } else { 1.0 };
    let long_output_multiplier = if usage.input_tokens > LONG_CONTEXT_INPUT_TOKENS { 1.5 } else { 1.0 };
    let input_cost = (uncached as f64 * input_rate
        + cached as f64 * cached_rate
        + cache_write as f64 * input_rate * 1.25)
        * long_input_multiplier;
    let output_cost = usage.output_tokens as f64 * output_rate * long_output_multiplier;
    Some((input_cost + output_cost) / 1_000_000.0)
}

pub fn claude_usage_total_tokens(usage: &serde_json::Value) -> i64 {
    let mut total = 0i64;
    for key in [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
    ] {
        if let Some(v) = usage.get(key).and_then(|v| v.as_i64()) {
            if v > 0 {
                total += v;
            }
        }
    }
    total
}

pub fn claude_api_equivalent_cost(model: Option<&str>, usage: &serde_json::Value) -> Option<f64> {
    let model = model?;
    let (input_rate, cache_read_rate, output_rate) = claude_model_pricing(model)?;
    let nonneg = |key: &str| -> i64 {
        usage.get(key).and_then(|v| v.as_i64()).filter(|v| *v > 0).unwrap_or(0)
    };
    let input_tokens = nonneg("input_tokens");
    let cache_read = nonneg("cache_read_input_tokens");
    let output_tokens = nonneg("output_tokens");

    let mut write_1h = 0i64;
    let mut write_5m = 0i64;
    if let Some(cache_creation) = usage.get("cache_creation").and_then(|v| v.as_object()) {
        write_1h = cache_creation
            .get("ephemeral_1h_input_tokens")
            .and_then(|v| v.as_i64())
            .filter(|v| *v > 0)
            .unwrap_or(0);
        write_5m = cache_creation
            .get("ephemeral_5m_input_tokens")
            .and_then(|v| v.as_i64())
            .filter(|v| *v > 0)
            .unwrap_or(0);
    }
    if write_1h == 0 && write_5m == 0 {
        let total_write = nonneg("cache_creation_input_tokens");
        if total_write > 0 {
            write_1h = total_write;
        }
    }

    let cost = input_tokens as f64 * input_rate
        + cache_read as f64 * cache_read_rate
        + write_1h as f64 * input_rate * CLAUDE_CACHE_WRITE_1H_MULTIPLIER
        + write_5m as f64 * input_rate * CLAUDE_CACHE_WRITE_5M_MULTIPLIER
        + output_tokens as f64 * output_rate;
    let prompt_tokens = input_tokens + cache_read + write_1h + write_5m;
    Some(cost * claude_long_prompt_multiplier(model, prompt_tokens) / 1_000_000.0)
}

/// One hour's worth of aggregated 30-day usage for a single rollout/
/// transcript file. See the `USAGE_BUCKET_SECONDS` doc comment above for
/// why this replaces keeping one entry per individual usage event.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageBucket {
    pub total_tokens: i64,
    pub priced_tokens: i64,
    pub unpriced_tokens: i64,
    pub cost_usd: f64,
}

impl UsageBucket {
    pub fn add(&mut self, total_tokens: i64, cost_usd: Option<f64>) {
        self.total_tokens += total_tokens;
        match cost_usd {
            None => self.unpriced_tokens += total_tokens,
            Some(cost) => {
                self.priced_tokens += total_tokens;
                self.cost_usd += cost;
            }
        }
    }
}

/// A single file's aggregated totals across all its retained buckets,
/// as returned by `FileState::totals()` for `ThirtyDayUsageTracker` to sum.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageTotals {
    pub total_tokens: i64,
    pub priced_tokens: i64,
    pub unpriced_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ThirtyDayUsage {
    pub total_tokens: i64,
    pub priced_tokens: i64,
    pub unpriced_tokens: i64,
    pub cost_usd: f64,
    pub sampled_at: f64,
}

/// `source_info`: classify a Codex thread's `source` JSON column.
/// Returns (kind, spawn_object) where kind is one of "main"-caller-provided,
/// "spawn", "guardian", or "other".
pub fn source_info(source: &str) -> (&'static str, serde_json::Value) {
    let value: serde_json::Value = match serde_json::from_str(source) {
        Ok(v) => v,
        Err(_) => return ("main", serde_json::Value::Null),
    };
    if let Some(subagent) = value.get("subagent") {
        if let Some(thread_spawn) = subagent.get("thread_spawn") {
            if thread_spawn.is_object() {
                return ("spawn", thread_spawn.clone());
            }
        }
        if subagent.get("other").and_then(|v| v.as_str()) == Some("guardian") {
            return ("guardian", subagent.clone());
        }
    }
    if value.is_object() {
        ("other", value)
    } else {
        ("other", serde_json::Value::Object(Default::default()))
    }
}

#[derive(Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub pid: i64,
    pub rollout_path: String,
    #[allow(dead_code)] // kept for parity with the Python `Thread.source`; not read after classification
    pub source: String,
    pub cwd: String,
    pub title: String,
    pub session_name: Option<String>,
    pub model: Option<String>,
    pub tokens_used: Option<i64>,
    pub nickname: Option<String>,
    #[allow(dead_code)]
    pub role: Option<String>,
    pub agent_path: Option<String>,
    pub created_at_ms: i64,
    pub reasoning_effort: Option<String>,
    pub parent_id: Option<String>,
    pub kind: String, // "main" | "spawn" | "guardian" | "other"
    pub depth: i64,
    pub origin: String, // "codex" | "claude"
}

impl Thread {
    pub fn project(&self) -> String {
        let name = Path::new(&self.cwd)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            self.cwd.clone()
        } else {
            name
        }
    }

    pub fn name(&self) -> String {
        if self.kind == "main" {
            return "MAIN".to_string();
        }
        if let Some(nick) = &self.nickname {
            return nick.clone();
        }
        if let Some(path) = &self.agent_path {
            return Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "AGENT".to_string());
        }
        "AGENT".to_string()
    }

    pub fn task(&self) -> String {
        if let Some(path) = &self.agent_path {
            return path.strip_prefix("/root/").unwrap_or(path).to_string();
        }
        if let Some(name) = &self.session_name {
            if !name.trim().is_empty() {
                return name.trim().to_string();
            }
        }
        let trimmed = self.title.trim();
        if !trimmed.is_empty() {
            if let Some(first_line) = trimmed.lines().next() {
                if !first_line.is_empty() {
                    return first_line.to_string();
                }
            }
        }
        self.id.chars().take(8).collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentRowState {
    pub phase: String,
    pub phase_at: f64,
    pub activity: String,
    pub total_tokens: Option<i64>,
    pub weekly_used_percent: Option<f64>,
    pub weekly_sample_at: Option<f64>,
    pub weekly_limit_id: Option<String>,
    pub weekly_resets_at: Option<f64>,
    pub five_hour_used_percent: Option<f64>,
    pub five_hour_sample_at: Option<f64>,
    pub five_hour_resets_at: Option<f64>,
    pub service_tier: Option<String>,
}

pub fn service_tier_badge(service_tier: Option<&str>) -> &'static str {
    match service_tier {
        Some("fast") | Some("priority") => "\u{26A1}",
        Some("ultrafast") => "\u{26A1}\u{26A1}",
        _ => "",
    }
}

pub fn parse_timestamp(value: Option<&str>) -> Option<f64> {
    let value = value?;
    let normalized = value.replace('Z', "+00:00");
    chrono::DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|dt| dt.timestamp() as f64 + dt.timestamp_subsec_nanos() as f64 / 1e9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_treats_ambiguous_symbols_as_narrow() {
        // Mirrors Python's unicodedata.east_asian_width: only "W"/"F"
        // categories double; ambiguous glyphs like the status dots/diamond
        // used in the row prefix stay width 1, unlike unicode-width's
        // "cjk" mode which would (wrongly) widen them to 2.
        assert_eq!(display_width("\u{25C6}"), 1); // ◆
        assert_eq!(display_width("\u{25CF}"), 1); // ●
        assert_eq!(display_width("\u{2713}"), 1); // ✓
        assert_eq!(display_width("MAIN"), 4);
    }

    #[test]
    fn display_width_doubles_fullwidth_japanese_text() {
        assert_eq!(display_width("\u{63CF}\u{753B}"), 4); // 描画
    }

    #[test]
    fn fit_pads_and_truncates_by_display_width() {
        assert_eq!(fit("MAIN", 8, true), "MAIN    ");
        assert_eq!(fit("MAIN", 8, false), "MAIN");
        assert_eq!(fit("abcdefgh", 5, true), "abcd…");
        assert_eq!(fit("abcdefgh", 5, false), "abcd…");
    }

    #[test]
    fn weekly_label_formats_remaining_percent() {
        assert_eq!(weekly_label(None), "Weekly --");
        assert_eq!(weekly_label(Some(14.0)), "Weekly 86%");
        assert_eq!(weekly_label(Some(13.5)), "Weekly 86.5%");
        assert_eq!(weekly_label(Some(0.0)), "Weekly 100%");
        assert_eq!(weekly_label(Some(100.0)), "Weekly 0%");
    }

    #[test]
    fn five_hour_label_formats_remaining_percent_and_expires_reset_windows() {
        assert_eq!(five_hour_label(None), "5h --");
        assert_eq!(five_hour_label(Some(10.0)), "5h 90%");
        assert_eq!(five_hour_label(Some(52.5)), "5h 47.5%");
        // a sample whose window already reset reads as a fresh (empty) window
        assert_eq!(five_hour_effective_used(Some(80.0), Some(100.0), 100.0), Some(0.0));
        assert_eq!(five_hour_effective_used(Some(80.0), Some(101.0), 100.0), Some(80.0));
        assert_eq!(five_hour_effective_used(Some(80.0), None, 100.0), Some(80.0));
        assert_eq!(five_hour_effective_used(None, Some(1.0), 100.0), None);
    }

    #[test]
    fn weekly_reset_elapsed_label_formats_d_hh_mm() {
        let now = 1_000_000.0;
        // resets_at 3 days 4h05m in the future -> window started
        // (7 - 3)d 19h55m ago... just check the direct arithmetic instead.
        let resets_at = now + WEEKLY_WINDOW_SECONDS - (3.0 * 86400.0 + 4.0 * 3600.0 + 5.0 * 60.0);
        assert_eq!(weekly_reset_elapsed_label(Some(resets_at), now), "3d04h05m");
        assert_eq!(weekly_reset_elapsed_label(None, now), "-d--h--m");
        // A day count past 7 (single digit or not) is left unpadded and
        // uncapped, since it just means the cached resets_at is stale.
        let stale_resets_at = now - (12.0 * 86400.0 + 1.0 * 3600.0 + 2.0 * 60.0) + WEEKLY_WINDOW_SECONDS;
        assert_eq!(weekly_reset_elapsed_label(Some(stale_resets_at), now), "12d01h02m");
    }

    #[test]
    fn estimated_cost_and_billions_labels() {
        assert_eq!(estimated_cost_label(None), "$--");
        assert_eq!(estimated_cost_label(Some(12344.6)), "$12,345");
        assert_eq!(billions_token_label(None), "--B");
        assert_eq!(billions_token_label(Some(8_200_000_000)), "8.2B");
    }

    #[test]
    fn unpriced_marker_flags_only_a_meaningful_unpriced_share() {
        assert_eq!(unpriced_marker(None, None), "");
        assert_eq!(unpriced_marker(Some(0), Some(0)), "");
        assert_eq!(unpriced_marker(Some(100), Some(0)), "");
        // a trickle below 1% stays unflagged
        assert_eq!(unpriced_marker(Some(1_000), Some(9)), "");
        assert_eq!(unpriced_marker(Some(990), Some(10)), "+?");
        // claude-opus-5-5 before it was priced: 4.7B of 5.9B unpriced
        assert_eq!(unpriced_marker(Some(1_140_000_000), Some(4_680_000_000)), "+?");
        // nothing priced at all is still flagged
        assert_eq!(unpriced_marker(Some(0), Some(5)), "+?");
    }

    #[test]
    fn compact_tokens_formats_scale() {
        assert_eq!(compact_tokens(None), "-- tok");
        assert_eq!(compact_tokens(Some(500)), "500 tok");
        assert_eq!(compact_tokens(Some(1_500)), "1.5K tok");
        assert_eq!(compact_tokens(Some(2_000_000)), "2M tok");
    }

    #[test]
    fn source_info_classifies_spawn_guardian_and_other() {
        let (kind, spawn) = source_info(r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p1","depth":2}}}"#);
        assert_eq!(kind, "spawn");
        assert_eq!(spawn.get("parent_thread_id").unwrap(), "p1");

        let (kind, _) = source_info(r#"{"subagent":{"other":"guardian"}}"#);
        assert_eq!(kind, "guardian");

        let (kind, _) = source_info(r#"{"anything":"else"}"#);
        assert_eq!(kind, "other");

        let (kind, _) = source_info("not json");
        assert_eq!(kind, "main");
    }

    #[test]
    fn token_usage_delta_rejects_negative_deltas() {
        let previous = TokenUsage { input_tokens: 10, cached_input_tokens: 0, cache_write_input_tokens: 0, output_tokens: 5, total_tokens: 15 };
        let current = TokenUsage { input_tokens: 8, cached_input_tokens: 0, cache_write_input_tokens: 0, output_tokens: 5, total_tokens: 13 };
        assert!(current.delta_from(&previous).is_none());
    }

    #[test]
    fn every_model_seen_in_real_data_is_either_priced_or_deliberately_not() {
        // Rates as published by OpenAI. A wrong or missing entry here is
        // invisible in the UI: unpriced tokens drop out of both the token
        // figure and the cost, which is exactly how the 30-day total once
        // came to understate real usage by ~48%.
        for (model, expected) in [
            ("gpt-6-astra", (10.00, 1.00, 50.00)),
            // found by the "+?" flag the day it shipped
            // cached input is half of gpt-6-sol's; the rest is the same
            ("gpt-6.1-sol", (2.00, 0.10, 10.00)),
            ("gpt-6-sol", (2.00, 0.20, 10.00)),
            ("gpt-6-luna", (0.10, 0.01, 0.50)),
            ("gpt-5.6-sol", (4.00, 0.40, 20.00)),
            ("gpt-5.6", (4.00, 0.40, 20.00)), // documented alias
            ("gpt-5.6-terra", (2.00, 0.20, 12.00)),
            ("gpt-5.6-luna", (0.20, 0.02, 1.20)),
            ("gpt-5.5", (5.00, 0.50, 30.00)),
            ("gpt-5.4", (2.50, 0.25, 15.00)),
        ] {
            assert_eq!(Some(expected), codex_model_pricing(model), "{}", model);
        }

        // Provisional, third-party rate rather than a published one.
        // Asserted so it cannot be dropped or drift silently.
        assert_eq!(Some((2.50, 0.25, 15.00)), codex_model_pricing("codex-auto-review"));

        // Absent from the pricing tables and left unpriced on purpose.
        assert_eq!(None, codex_model_pricing("gpt-5.3-codex-spark"));
        assert_eq!(None, codex_model_pricing("some-model-openai-ships-next"));
    }

    #[test]
    fn every_claude_model_seen_in_real_data_is_priced() {
        // claude-opus-5-5 was missing, so ~80% of 30-day usage silently
        // dropped out of the header.
        for (model, expected) in [
            ("claude-opus-5-5", (4.00, 0.20, 20.00)),
            ("claude-opus-5", (5.00, 0.50, 25.00)),
            ("claude-sonnet-5-5", (2.00, 0.10, 10.00)),
            ("claude-sonnet-5", (2.00, 0.20, 10.00)),
            ("claude-fable-5-1", (10.00, 0.25, 50.00)),
            ("claude-haiku-5-5", (0.10, 0.01, 0.50)),
            ("claude-opus-4-8", (5.00, 0.50, 25.00)),
            ("claude-fable-5", (10.00, 1.00, 50.00)),
            ("claude-haiku-4-5", (1.00, 0.10, 5.00)),
            // Claude Code records Haiku by its dated snapshot id
            ("claude-haiku-4-5-20251001", (1.00, 0.10, 5.00)),
        ] {
            assert_eq!(Some(expected), claude_model_pricing(model), "{}", model);
        }
        // only an 8-digit date is dropped, so version numbers survive
        assert_eq!(None, claude_model_pricing("claude-sonnet-5-55"));
        assert_eq!(None, claude_model_pricing("claude-next-20251001"));
        // Claude Code's placeholder for locally generated turns carries
        // no real usage.
        assert_eq!(None, claude_model_pricing("<synthetic>"));
    }

    #[test]
    fn claude_cost_uses_each_models_own_cache_read_rate() {
        let usage = serde_json::json!({
            "input_tokens": 1_000_000,
            "cache_read_input_tokens": 10_000_000,
            "output_tokens": 1_000_000,
            "cache_creation": {"ephemeral_5m_input_tokens": 1_000_000, "ephemeral_1h_input_tokens": 1_000_000}
        });
        // 4 input + 10M * 0.20 + 5m write 4*1.25 + 1h write 4*2 + 20 output
        let cost = claude_api_equivalent_cost(Some("claude-opus-5-5"), &usage).unwrap();
        assert!((cost - (4.0 + 2.0 + 5.0 + 8.0 + 20.0)).abs() < 1e-9, "{}", cost);
        // Fable 5.1 reads at 0.025x, not the 0.1x of most models
        let cost = claude_api_equivalent_cost(Some("claude-fable-5-1"), &usage).unwrap();
        assert!((cost - (10.0 + 2.5 + 12.5 + 20.0 + 50.0)).abs() < 1e-9, "{}", cost);
        assert!(claude_api_equivalent_cost(Some("claude-next"), &usage).is_none());
    }

    #[test]
    fn haiku_5_5_prompts_over_100k_tokens_pay_5x_every_rate() {
        // 60K input + 40K cache read = exactly 100K: still the base rates
        let at_limit = serde_json::json!({
            "input_tokens": 60_000, "cache_read_input_tokens": 40_000, "output_tokens": 10_000
        });
        let base = (60_000.0 * 0.10 + 40_000.0 * 0.01 + 10_000.0 * 0.50) / 1_000_000.0;
        let cost = claude_api_equivalent_cost(Some("claude-haiku-5-5"), &at_limit).unwrap();
        assert!((cost - base).abs() < 1e-12, "{}", cost);
        // one cache-write token more pushes the whole request over
        let over = serde_json::json!({
            "input_tokens": 60_000, "cache_read_input_tokens": 40_000, "output_tokens": 10_000,
            "cache_creation": {"ephemeral_5m_input_tokens": 1, "ephemeral_1h_input_tokens": 0}
        });
        let expected = 5.0 * (base + 0.10 * 1.25 / 1_000_000.0);
        let cost = claude_api_equivalent_cost(Some("claude-haiku-5-5"), &over).unwrap();
        assert!((cost - expected).abs() < 1e-12, "{}", cost);
        // other models bill the full context window at standard rates
        let big = serde_json::json!({"input_tokens": 500_000, "output_tokens": 0});
        let cost = claude_api_equivalent_cost(Some("claude-sonnet-5-5"), &big).unwrap();
        assert!((cost - 1.0).abs() < 1e-12, "{}", cost);
    }

    #[test]
    fn api_equivalent_cost_applies_long_context_multiplier() {
        let usage = TokenUsage {
            input_tokens: LONG_CONTEXT_INPUT_TOKENS + 1,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 1_000_000,
            total_tokens: LONG_CONTEXT_INPUT_TOKENS + 1 + 1_000_000,
        };
        let cost = api_equivalent_cost(Some("gpt-5.6-sol"), &usage).unwrap();
        assert!(cost > 0.0);
        assert!(api_equivalent_cost(Some("unknown-model"), &usage).is_none());
    }
}
