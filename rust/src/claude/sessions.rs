use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// Enumerate live Claude Code processes via ~/.claude/sessions/<pid>.json.
/// Unlike Codex, Claude Code writes one small JSON file per running process
/// with its own pid, cwd, name, and live status, so no lsof lookup is
/// needed: a file is trusted only after confirming its pid is still alive.
/// Mirrors `discover_claude_sessions`.
pub fn discover_claude_sessions(claude_home: &Path) -> (Vec<Value>, Option<String>) {
    let sessions_dir = claude_home.join("sessions");
    let mut entries: Vec<std::path::PathBuf> = match std::fs::read_dir(&sessions_dir) {
        Ok(iter) => iter.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
        Err(e) => {
            return (
                Vec::new(),
                Some(format!("claude sessions dir unavailable: {}", e)),
            )
        }
    };
    entries.sort();

    let mut sessions = Vec::new();
    for entry in entries {
        let name = match entry.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !is_pid_json(name) {
            continue;
        }
        let text = match std::fs::read_to_string(&entry) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let data: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let pid = match data.get("pid").and_then(|v| v.as_i64()) {
            Some(p) => p,
            None => continue,
        };
        if data.get("sessionId").and_then(|v| v.as_str()).is_none() {
            continue;
        }
        if !pid_is_alive(pid) {
            continue;
        }
        sessions.push(data);
    }
    (sessions, None)
}

fn is_pid_json(name: &str) -> bool {
    match name.strip_suffix(".json") {
        Some(stem) if !stem.is_empty() => stem.bytes().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

#[cfg(unix)]
fn pid_is_alive(pid: i64) -> bool {
    // kill(pid, 0) with EPERM means the process exists but is owned by
    // someone else -- still alive. Only ESRCH means it's gone.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(0);
    errno == libc::EPERM
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i64) -> bool {
    false
}

/// Find the `<sessionId>.jsonl` transcript path for a Claude Code session,
/// caching the resolved path per session id. Mirrors `find_claude_jsonl`.
pub fn find_claude_jsonl(
    claude_home: &Path,
    session_id: &str,
    cache: &mut HashMap<String, String>,
) -> Option<String> {
    if let Some(cached) = cache.get(session_id) {
        if Path::new(cached).exists() {
            return Some(cached.clone());
        }
    }
    let pattern = claude_home
        .join("projects")
        .join("*")
        .join(format!("{}.jsonl", session_id));
    let matches = glob_paths(&pattern.to_string_lossy());
    let first = matches.into_iter().next()?;
    cache.insert(session_id.to_string(), first.clone());
    Some(first)
}

/// Minimal single-`*`-per-segment glob, matching the subset of patterns
/// this module actually needs (`.../projects/*/<id>.jsonl` and
/// `.../projects/*/*.jsonl`), without depending on the `glob` crate.
pub fn glob_paths(pattern: &str) -> Vec<String> {
    let parts: Vec<&str> = pattern.split('/').collect();
    let mut current: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from("/")];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let mut next = Vec::new();
        for base in &current {
            if part.contains('*') {
                if let Ok(iter) = std::fs::read_dir(base) {
                    for entry in iter.filter_map(|e| e.ok()) {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if glob_match(part, &name) {
                            next.push(base.join(name));
                        }
                    }
                }
            } else {
                next.push(base.join(part));
            }
        }
        current = next;
        if i == parts.len() - 1 {
            break;
        }
    }
    let mut result: Vec<String> = current
        .into_iter()
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    result.sort();
    result
}

fn glob_match(pattern: &str, name: &str) -> bool {
    // Only supports a single trailing/leading/whole '*' segment, which is
    // all this module's callers use.
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    pattern == name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_skips_dead_pids_and_files_that_are_not_named_after_a_pid() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let sessions = home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        // this test process is by definition alive
        let live_pid = std::process::id() as i64;
        std::fs::write(
            sessions.join(format!("{}.json", live_pid)),
            format!(
                r#"{{"pid":{},"sessionId":"live-session","cwd":"/tmp/p","status":"busy"}}"#,
                live_pid
            ),
        )
        .unwrap();

        // a pid that cannot be running (pid 0 is never a user process)
        std::fs::write(
            sessions.join("2147483646.json"),
            r#"{"pid":2147483646,"sessionId":"dead-session","cwd":"/tmp/p"}"#,
        )
        .unwrap();

        // the sidecar key files Claude Code also writes here are not sessions
        std::fs::write(sessions.join("1234.abcdef.key"), "irrelevant").unwrap();
        std::fs::write(sessions.join("notapid.json"), r#"{"pid":1,"sessionId":"x"}"#).unwrap();

        let (found, error) = discover_claude_sessions(home);
        assert!(error.is_none());
        let ids: Vec<&str> = found
            .iter()
            .map(|v| v["sessionId"].as_str().unwrap())
            .collect();
        assert_eq!(vec!["live-session"], ids);
    }

    #[test]
    fn transcript_lookup_globs_one_project_level_and_caches_the_hit() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let project = home.join("projects").join("-tmp-some-project");
        std::fs::create_dir_all(&project).unwrap();
        let transcript = project.join("sess-1.jsonl");
        std::fs::write(&transcript, "").unwrap();

        let mut cache = HashMap::new();
        let found = find_claude_jsonl(home, "sess-1", &mut cache).unwrap();
        assert_eq!(transcript.to_string_lossy(), found);
        assert_eq!(1, cache.len());

        // a second lookup is served from the cache
        assert_eq!(Some(found), find_claude_jsonl(home, "sess-1", &mut cache));

        // an unknown session resolves to nothing rather than a stale neighbour
        assert_eq!(None, find_claude_jsonl(home, "sess-missing", &mut cache));
    }

    #[test]
    fn a_missing_sessions_dir_is_reported_as_a_warning_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let (found, error) = discover_claude_sessions(&dir.path().join("no-such-home"));
        assert!(found.is_empty());
        assert!(error.unwrap().contains("claude sessions dir unavailable"));
    }
}
