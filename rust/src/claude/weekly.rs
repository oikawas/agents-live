use std::path::Path;

/// Directory the `agents-live statusline` hook writes to; see statusline.rs.
pub const CLAUDE_STATUSLINE_CACHE_DIRNAME: &str = "agents-live-statusline-cache";

/// Read the 7-day rate-limit percentage cached by the statusline hook.
///
/// Claude Code exposes this only as a one-shot JSON payload piped to the
/// statusLine command's stdin on each render; there is no file agents-live
/// can tail passively for it. If the optional wrapper has not been
/// installed, or this session's statusLine has not rendered yet, no cache
/// file exists and this returns (None, None, None), which displays as
/// "Weekly --" -- the tool works fine without it.
///
/// Returns (used_percent, sample_at, resets_at). Mirrors `claude_weekly_sample`.
pub fn claude_weekly_sample(
    claude_home: &Path,
    session_id: &str,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    claude_rate_limit_sample(claude_home, session_id, "seven_day")
}

/// Same as `claude_weekly_sample`, for the rolling 5-hour window.
pub fn claude_five_hour_sample(
    claude_home: &Path,
    session_id: &str,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    claude_rate_limit_sample(claude_home, session_id, "five_hour")
}

fn claude_rate_limit_sample(
    claude_home: &Path,
    session_id: &str,
    window: &str,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let path = claude_home
        .join(CLAUDE_STATUSLINE_CACHE_DIRNAME)
        .join(format!("{}.json", session_id));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return (None, None, None),
    };
    let data: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return (None, None, None),
    };
    let limit = data.get("rate_limits").and_then(|v| v.get(window));
    let used_percent = limit.and_then(|v| v.get("used_percentage")).and_then(|v| v.as_f64());
    let used_percent = match used_percent {
        Some(v) => v,
        None => return (None, None, None),
    };
    let sample_at = data.get("captured_at").and_then(|v| v.as_f64());
    let resets_at = limit.and_then(|v| v.get("resets_at")).and_then(|v| v.as_f64());
    (Some(used_percent), sample_at, resets_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cache(home: &Path, session_id: &str, body: &str) {
        let dir = home.join(CLAUDE_STATUSLINE_CACHE_DIRNAME);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{}.json", session_id)), body).unwrap();
    }

    #[test]
    fn reads_the_wrapper_cache_and_treats_every_absence_as_simply_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // nothing installed at all -> "Weekly --", never an error
        assert_eq!((None, None, None), claude_weekly_sample(home, "sess-1"));

        write_cache(
            home,
            "sess-1",
            r#"{"session_id":"sess-1","captured_at":1788600000,
                "rate_limits":{"seven_day":{"used_percentage":6.5,"resets_at":1788609600}}}"#,
        );
        let (used, sampled, resets) = claude_weekly_sample(home, "sess-1");
        assert_eq!(Some(6.5), used);
        assert_eq!(Some(1788600000.0), sampled);
        assert_eq!(Some(1788609600.0), resets);

        // malformed JSON, and a payload missing seven_day, both read as unknown
        write_cache(home, "sess-2", "{not json");
        assert_eq!((None, None, None), claude_weekly_sample(home, "sess-2"));
        write_cache(home, "sess-3", r#"{"rate_limits":{"five_hour":{"used_percentage":52}}}"#);
        assert_eq!((None, None, None), claude_weekly_sample(home, "sess-3"));
        // ...while the 5-hour reader picks that same window up
        assert_eq!((Some(52.0), None, None), claude_five_hour_sample(home, "sess-3"));
        assert_eq!((None, None, None), claude_five_hour_sample(home, "sess-1"));

        // a different session's cache is never substituted
        assert_eq!((None, None, None), claude_weekly_sample(home, "sess-unknown"));
    }
}
