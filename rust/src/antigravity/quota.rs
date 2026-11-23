//! Antigravity's usage limits. Like Claude Code, the Antigravity CLI writes
//! them to no local file: it pipes them to its statusLine command's stdin on
//! every render, as named buckets ("gemini-weekly", "gemini-5h",
//! "3p-weekly", ...) each with a remaining fraction and a reset time. The
//! shared `agents-live statusline` hook caches them here per conversation.

use serde_json::{json, Value};
use std::path::Path;
use std::time::SystemTime;

pub const ANTIGRAVITY_STATUSLINE_CACHE_DIRNAME: &str = "agents-live-statusline-cache";

/// Cache files left by conversations that stopped rendering are pruned after
/// this long.
const MAX_CACHE_AGE_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn is_antigravity_payload(payload: &Value) -> bool {
    payload.get("product").and_then(|v| v.as_str()) == Some("antigravity")
}

/// Writes the payload's quota buckets (and agent state) for its conversation.
/// Payloads from before a conversation exists, or without quota, are skipped.
pub fn cache_quota(home: &Path, payload: &Value) {
    let conversation_id = match payload.get("conversation_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() && !id.contains('/') => id,
        _ => return,
    };
    let quota = match payload.get("quota") {
        Some(q) if q.as_object().is_some_and(|o| !o.is_empty()) => q,
        _ => return,
    };
    let cache_dir = home.join(ANTIGRAVITY_STATUSLINE_CACHE_DIRNAME);
    if std::fs::create_dir_all(&cache_dir).is_err() {
        return;
    }
    let now = now_secs();
    let record = json!({
        "conversation_id": conversation_id,
        "quota": quota,
        "agent_state": payload.get("agent_state"),
        "captured_at": now,
    });
    // Write then rename, so the dashboard never reads a half-written file.
    let tmp = cache_dir.join(format!(".tmp-{}-{}", std::process::id(), conversation_id));
    let target = cache_dir.join(format!("{}.json", conversation_id));
    if std::fs::write(&tmp, record.to_string()).is_err() || std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    prune_stale(&cache_dir, now);
}

fn prune_stale(cache_dir: &Path, now: f64) {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") || name == crate::claude::statusline::SIDECAR_NAME {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64());
        if modified.is_some_and(|m| now - m > MAX_CACHE_AGE_SECONDS) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// One limit as the dashboard shows it: used percent and reset time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimitSample {
    pub used_percent: f64,
    pub resets_at: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaSample {
    pub captured_at: f64,
    pub weekly: Option<LimitSample>,
    pub five_hour: Option<LimitSample>,
}

/// The tightest bucket whose name ends with `suffix` ("-weekly", "-5h"):
/// least remaining, then soonest reset. Gemini and third-party models (and
/// some single models) have buckets of their own; the one closest to running
/// out is the one that stops work.
fn tightest(quota: &Value, suffix: &str) -> Option<LimitSample> {
    let buckets = quota.as_object()?;
    buckets
        .iter()
        .filter(|(name, _)| name.ends_with(suffix))
        .filter_map(|(_, bucket)| {
            let remaining = bucket.get("remaining_fraction")?.as_f64()?;
            let resets_at = bucket
                .get("reset_time")
                .and_then(|v| v.as_str())
                .and_then(|s| crate::model::parse_timestamp(Some(s)));
            Some(LimitSample { used_percent: ((1.0 - remaining) * 100.0).clamp(0.0, 100.0), resets_at })
        })
        .min_by(|a, b| {
            b.used_percent
                .total_cmp(&a.used_percent)
                .then(a.resets_at.unwrap_or(f64::MAX).total_cmp(&b.resets_at.unwrap_or(f64::MAX)))
        })
}

/// The freshest cached quota of any conversation: the limits belong to the
/// account, not to one conversation.
pub fn latest_quota(home: &Path) -> Option<QuotaSample> {
    let entries = std::fs::read_dir(home.join(ANTIGRAVITY_STATUSLINE_CACHE_DIRNAME)).ok()?;
    let mut best: Option<(f64, Value)> = None;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") || name.starts_with('.') || name == crate::claude::statusline::SIDECAR_NAME {
            continue;
        }
        let data: Value = match std::fs::read_to_string(entry.path()).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            Some(v) => v,
            None => continue,
        };
        let captured_at = match data.get("captured_at").and_then(|v| v.as_f64()) {
            Some(t) => t,
            None => continue,
        };
        if best.as_ref().is_none_or(|(t, _)| captured_at > *t) {
            best = Some((captured_at, data));
        }
    }
    let (captured_at, data) = best?;
    let quota = data.get("quota")?;
    Some(QuotaSample {
        captured_at,
        weekly: tightest(quota, "-weekly"),
        five_hour: tightest(quota, "-5h"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = r#"{"product":"antigravity","conversation_id":"conv-1","agent_state":"working",
        "quota":{"3p-5h":{"remaining_fraction":1,"reset_time":"2026-11-23T06:39:05Z"},
                 "3p-weekly":{"remaining_fraction":1,"reset_time":"2026-11-30T01:39:05Z"},
                 "gemini-5h":{"remaining_fraction":0.75,"reset_time":"2026-11-23T03:30:38Z"},
                 "gemini-weekly":{"remaining_fraction":0.9,"reset_time":"2026-11-29T22:30:38Z"}}}"#;

    #[test]
    fn caches_the_quota_and_reads_back_the_tightest_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert_eq!(None, latest_quota(home));

        let payload: Value = serde_json::from_str(PAYLOAD).unwrap();
        assert!(is_antigravity_payload(&payload));
        cache_quota(home, &payload);
        let sample = latest_quota(home).unwrap();
        let weekly = sample.weekly.unwrap();
        assert!((weekly.used_percent - 10.0).abs() < 1e-6, "{:?}", weekly);
        assert_eq!(crate::model::parse_timestamp(Some("2026-11-29T22:30:38Z")), weekly.resets_at);
        let five_hour = sample.five_hour.unwrap();
        assert!((five_hour.used_percent - 25.0).abs() < 1e-6, "{:?}", five_hour);
    }

    #[test]
    fn untouched_buckets_tie_on_the_soonest_reset_and_empty_payloads_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // before a conversation exists the CLI sends no id and no quota
        cache_quota(home, &serde_json::json!({"product":"antigravity","conversation_id":"","quota":{}}));
        cache_quota(home, &serde_json::json!({"product":"antigravity","conversation_id":"c","quota":{}}));
        assert_eq!(None, latest_quota(home));

        let quota = serde_json::json!({
            "a-weekly":{"remaining_fraction":1,"reset_time":"2026-11-30T01:00:00Z"},
            "b-weekly":{"remaining_fraction":1,"reset_time":"2026-11-29T01:00:00Z"}
        });
        let weekly = tightest(&quota, "-weekly").unwrap();
        assert_eq!(0.0, weekly.used_percent);
        assert_eq!(crate::model::parse_timestamp(Some("2026-11-29T01:00:00Z")), weekly.resets_at);
        assert_eq!(None, tightest(&quota, "-5h"));
    }
}
