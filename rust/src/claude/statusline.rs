//! The optional Claude Code statusLine hook behind Claude's "Weekly nn%" and
//! "5h nn%" figures: `agents-live statusline` and
//! `agents-live install-statusline`.
//!
//! Claude Code writes its rate-limit percentages to no local file; it only
//! pipes them as JSON to the statusLine command's stdin on every render. The
//! hook is installed as that command: it caches the payload's rate limits
//! per session for the dashboard (see `weekly.rs`), then runs the statusLine
//! command that was configured before, with the same stdin, so the visible
//! status line is unchanged.

use crate::claude::weekly::CLAUDE_STATUSLINE_CACHE_DIRNAME;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

/// Written once at install to remember the statusLine config that was there
/// before, for --uninstall. It is never touched again, so it must be spared
/// by the age-based pruning below.
pub const SIDECAR_NAME: &str = "original-statusline.json";

/// Cache entries older than this are pruned, so a session that stopped
/// rendering (closed, crashed) does not leave data behind forever.
const MAX_CACHE_AGE_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

/// The subcommand name the installed statusLine command runs.
pub const HOOK_SUBCOMMAND: &str = "statusline";

/// Marks the retired Python hook, still found in settings written by earlier
/// versions; installing over it migrates to the Rust hook.
const LEGACY_WRAPPER_NAME: &str = "claude-statusline-wrapper";

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---- the hook: `agents-live statusline [ORIGINAL_COMMAND]` ----

/// Runs as Claude Code's statusLine command. Never fails the status line
/// over its own caching: a bad payload or an unwritable cache is skipped,
/// and the original command still runs. Returns the exit code to use.
pub fn run_hook(claude_home: &Path, original_command: &str, stdin: &[u8], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if let Ok(payload) = serde_json::from_slice::<Value>(stdin) {
        cache_rate_limits(claude_home, &payload);
    }
    if original_command.trim().is_empty() {
        return 0;
    }
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(original_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return 0,
    };
    if let Some(mut child_stdin) = child.stdin.take() {
        // A command that exits without reading its stdin closes the pipe;
        // that is its business, not an error of ours.
        let _ = child_stdin.write_all(stdin);
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let _ = out.write_all(&output.stdout);
    let _ = err.write_all(&output.stderr);
    output.status.code().unwrap_or(1)
}

fn cache_rate_limits(claude_home: &Path, payload: &Value) {
    let session_id = match payload.get("session_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() && !id.contains('/') => id,
        _ => return,
    };
    let rate_limits = match payload.get("rate_limits") {
        Some(v) if v.is_object() => v,
        _ => return,
    };
    let cache_dir = claude_home.join(CLAUDE_STATUSLINE_CACHE_DIRNAME);
    if std::fs::create_dir_all(&cache_dir).is_err() {
        return;
    }
    let now = now_secs();
    let record = json!({"session_id": session_id, "rate_limits": rate_limits, "captured_at": now});
    // Write then rename, so the dashboard never reads a half-written file.
    let tmp = cache_dir.join(format!(".tmp-{}-{}", std::process::id(), session_id));
    let target = cache_dir.join(format!("{}.json", session_id));
    if std::fs::write(&tmp, record.to_string()).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, &target).is_err() {
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
        if !name.ends_with(".json") || name == SIDECAR_NAME {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64());
        if let Some(modified) = modified {
            if now - modified > MAX_CACHE_AGE_SECONDS {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

// ---- the installer: `agents-live install-statusline [--uninstall]` ----

/// POSIX shell quoting, as Python's `shlex.quote`: bare when every character
/// is safe, otherwise single-quoted with embedded quotes spliced in.
pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let safe = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// The statusLine command that runs the hook through `exe`, then
/// `original_command`. `--claude-home` is spelled out only when it is not the
/// default, so the hook caches where the dashboard will look.
pub fn hook_command(exe: &Path, claude_home_arg: Option<&str>, original_command: &str) -> String {
    let mut parts = vec![shell_quote(&exe.to_string_lossy())];
    if let Some(home) = claude_home_arg {
        parts.push("--claude-home".to_string());
        parts.push(shell_quote(home));
    }
    parts.push(HOOK_SUBCOMMAND.to_string());
    parts.push(shell_quote(original_command));
    parts.join(" ")
}

fn status_line_command(status_line: Option<&Value>) -> Option<&str> {
    status_line.and_then(|v| v.get("command")).and_then(|v| v.as_str())
}

/// True if the statusLine already runs an agents-live hook: this binary's
/// subcommand (from any install location) or the retired Python wrapper.
fn is_hooked(status_line: Option<&Value>) -> bool {
    let command = match status_line_command(status_line) {
        Some(c) => c,
        None => return false,
    };
    // The subcommand is a bare word of its own; the original command is one
    // quoted word, so a path like '~/.claude/statusline.py' never matches.
    let runs_hook = command.contains("agents-live") && command.split_whitespace().any(|word| word == HOOK_SUBCOMMAND);
    runs_hook || command.contains(LEGACY_WRAPPER_NAME)
}

fn load_json(path: &Path) -> Result<Option<Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{} is not valid JSON: {}", path.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {}", path.display(), e)),
    }
}

fn backup(path: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let name = format!(
        "{}.agents-live-statusline-backup-{}",
        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        stamp
    );
    let backup_path = path.with_file_name(name);
    std::fs::copy(path, &backup_path).map_err(|e| format!("cannot back up {}: {}", path.display(), e))?;
    Ok(Some(backup_path))
}

/// Same layout as the Python installer wrote: two-space indent, non-ASCII
/// kept as is, trailing newline, and the file's own key order.
fn save_settings(path: &Path, settings: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(path, text + "\n").map_err(|e| format!("cannot write {}: {}", path.display(), e))
}

/// Points settings.json's statusLine at the hook. Idempotent: an install
/// already running this very command is left alone. One made by another
/// install location or by the retired Python wrapper is re-pointed, keeping
/// the original command recorded at the first install.
pub fn install(claude_home: &Path, exe: &Path, claude_home_arg: Option<&str>) -> Result<Vec<String>, String> {
    let settings_path = claude_home.join("settings.json");
    let cache_dir = claude_home.join(CLAUDE_STATUSLINE_CACHE_DIRNAME);
    let sidecar_path = cache_dir.join(SIDECAR_NAME);
    let mut messages = Vec::new();

    let mut settings = load_json(&settings_path)?.unwrap_or_else(|| json!({}));
    if !settings.is_object() {
        return Err(format!("{} is not a JSON object", settings_path.display()));
    }
    let current = settings.get("statusLine").cloned();

    let original_status_line = if is_hooked(current.as_ref()) {
        let original = match load_json(&sidecar_path)? {
            Some(v) => v,
            None if sidecar_path.exists() => Value::Null,
            None => {
                return Err(format!(
                    "statusLine already runs an agents-live hook, but {} is missing, so the \
                     original statusLine is unknown; restore it by hand before installing",
                    sidecar_path.display()
                ))
            }
        };
        let command = hook_command(exe, claude_home_arg, status_line_command(Some(&original)).unwrap_or(""));
        if status_line_command(current.as_ref()) == Some(command.as_str()) {
            messages.push("Already installed: statusLine already runs the agents-live hook.".to_string());
            return Ok(messages);
        }
        original
    } else {
        std::fs::create_dir_all(&cache_dir).map_err(|e| format!("cannot create {}: {}", cache_dir.display(), e))?;
        let original = current.clone().unwrap_or(Value::Null);
        std::fs::write(&sidecar_path, original.to_string())
            .map_err(|e| format!("cannot write {}: {}", sidecar_path.display(), e))?;
        original
    };

    let original_command = status_line_command(Some(&original_status_line)).unwrap_or("").to_string();
    settings["statusLine"] = json!({
        "type": "command",
        "command": hook_command(exe, claude_home_arg, &original_command),
    });
    let backup_path = backup(&settings_path)?;
    save_settings(&settings_path, &settings)?;

    messages.push(format!(
        "Installed: {} now runs its statusLine through {} {}",
        settings_path.display(),
        exe.display(),
        HOOK_SUBCOMMAND
    ));
    if let Some(path) = backup_path {
        messages.push(format!("Previous settings backed up to {}", path.display()));
    }
    if original_command.is_empty() {
        messages.push("No statusLine command was configured before; nothing is shown, the limits are only cached.".to_string());
    } else {
        messages.push(format!("The original statusLine command ({:?}) still runs, unchanged.", original_command));
    }
    messages.push("Open a new Claude Code session (or wait for the next status-line render in an".to_string());
    messages.push("existing one) for the limit cache to start filling in.".to_string());
    messages.push("Run `agents-live install-statusline --uninstall` to restore the previous statusLine.".to_string());
    Ok(messages)
}

/// Restores the statusLine recorded at install (removing the key if there
/// was none). Works on an install made by the retired Python wrapper too.
pub fn uninstall(claude_home: &Path) -> Result<Vec<String>, String> {
    let settings_path = claude_home.join("settings.json");
    let sidecar_path = claude_home.join(CLAUDE_STATUSLINE_CACHE_DIRNAME).join(SIDECAR_NAME);
    let mut messages = Vec::new();

    let mut settings = match load_json(&settings_path)? {
        Some(v) if v.is_object() => v,
        _ => {
            messages.push("Not installed: statusLine does not run the agents-live hook; nothing to do.".to_string());
            return Ok(messages);
        }
    };
    if !is_hooked(settings.get("statusLine")) {
        messages.push("Not installed: statusLine does not run the agents-live hook; nothing to do.".to_string());
        return Ok(messages);
    }

    let original = load_json(&sidecar_path)?.unwrap_or(Value::Null);
    let backup_path = backup(&settings_path)?;
    let map = settings.as_object_mut().expect("checked above");
    if original.is_null() {
        map.remove("statusLine");
    } else {
        map.insert("statusLine".to_string(), original);
    }
    save_settings(&settings_path, &settings)?;
    let _ = std::fs::remove_file(&sidecar_path);

    messages.push(format!("Restored the previous statusLine configuration in {}", settings_path.display()));
    if let Some(path) = backup_path {
        messages.push(format!("Hooked settings backed up to {}", path.display()));
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = r#"{"session_id":"sess-1","rate_limits":{"five_hour":{"used_percentage":52,"resets_at":1788595200},"seven_day":{"used_percentage":6,"resets_at":1788609600}}}"#;

    fn hook(home: &Path, original: &str, stdin: &str) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_hook(home, original, stdin.as_bytes(), &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn hook_caches_rate_limits_and_forwards_the_original_command_output() {
        let dir = tempfile::tempdir().unwrap();
        let (code, out, err) = hook(dir.path(), "cat >/dev/null; printf %s hello-status", PAYLOAD);
        assert_eq!(0, code, "{}", err);
        assert_eq!("hello-status", out);

        let cached: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(CLAUDE_STATUSLINE_CACHE_DIRNAME).join("sess-1.json")).unwrap(),
        )
        .unwrap();
        assert_eq!("sess-1", cached["session_id"]);
        assert_eq!(6, cached["rate_limits"]["seven_day"]["used_percentage"]);
        assert!(cached["captured_at"].as_f64().unwrap() > 0.0);
        // and the dashboard reads it back
        let (used, _, resets) = crate::claude::weekly::claude_weekly_sample(dir.path(), "sess-1");
        assert_eq!((Some(6.0), Some(1788609600.0)), (used, resets));
    }

    #[test]
    fn hook_passes_the_same_stdin_through_and_keeps_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let (code, out, err) = hook(dir.path(), "cat; echo oops >&2; exit 3", PAYLOAD);
        assert_eq!(3, code);
        assert_eq!(PAYLOAD, out);
        assert_eq!("oops\n", err);
    }

    #[test]
    fn hook_caches_without_an_original_command_and_prints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!((0, String::new(), String::new()), hook(dir.path(), "", PAYLOAD));
        assert!(dir.path().join(CLAUDE_STATUSLINE_CACHE_DIRNAME).join("sess-1.json").exists());
    }

    #[test]
    fn hook_survives_malformed_stdin_and_odd_session_ids() {
        let dir = tempfile::tempdir().unwrap();
        let (code, out, _) = hook(dir.path(), "printf %s still-here", "not json at all");
        assert_eq!((0, "still-here"), (code, out.as_str()));
        assert!(!dir.path().join(CLAUDE_STATUSLINE_CACHE_DIRNAME).exists());

        hook(dir.path(), "", r#"{"session_id":"../escape","rate_limits":{}}"#);
        assert!(!dir.path().join(CLAUDE_STATUSLINE_CACHE_DIRNAME).exists());
    }

    #[test]
    fn stale_cache_entries_are_pruned_but_the_sidecar_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CLAUDE_STATUSLINE_CACHE_DIRNAME);
        std::fs::create_dir_all(&cache).unwrap();
        for name in ["old.json", SIDECAR_NAME] {
            std::fs::write(cache.join(name), "{}").unwrap();
        }
        let long_ago = SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 60 * 60);
        for name in ["old.json", SIDECAR_NAME] {
            let file = std::fs::File::options().write(true).open(cache.join(name)).unwrap();
            file.set_modified(long_ago).unwrap();
        }
        hook(dir.path(), "", PAYLOAD);
        assert!(!cache.join("old.json").exists());
        assert!(cache.join(SIDECAR_NAME).exists());
        assert!(cache.join("sess-1.json").exists());
    }

    #[test]
    fn shell_quote_matches_python_shlex_quote() {
        assert_eq!("''", shell_quote(""));
        assert_eq!("/usr/local/bin/agents-live", shell_quote("/usr/local/bin/agents-live"));
        assert_eq!("'~/.claude/statusline.py'", shell_quote("~/.claude/statusline.py"));
        assert_eq!("'it'\"'\"'s'", shell_quote("it's"));
    }

    fn settings(home: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(home.join("settings.json")).unwrap()).unwrap()
    }

    fn backups(home: &Path) -> usize {
        std::fs::read_dir(home)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.json.agents-live-statusline-backup-")
            })
            .count()
    }

    #[test]
    fn install_wraps_then_uninstall_restores_the_existing_statusline() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let exe = Path::new("/opt/bin/agents-live");
        let original = json!({"type": "command", "command": "~/.claude/statusline.py", "padding": 0});
        std::fs::write(
            home.join("settings.json"),
            r#"{"zeta": 1, "statusLine": {"type": "command", "command": "~/.claude/statusline.py", "padding": 0}, "model": "keep-me"}"#,
        )
        .unwrap();

        install(home, exe, None).unwrap();
        let installed = settings(home);
        assert_eq!(
            "/opt/bin/agents-live statusline '~/.claude/statusline.py'",
            installed["statusLine"]["command"]
        );
        assert_eq!("keep-me", installed["model"]);
        // the user's key order survives the rewrite
        let keys: Vec<&String> = installed.as_object().unwrap().keys().collect();
        assert_eq!(vec!["zeta", "statusLine", "model"], keys);
        assert_eq!(1, backups(home));

        // running it again is a no-op, not a double wrap
        let messages = install(home, exe, None).unwrap();
        assert!(messages[0].starts_with("Already installed"), "{:?}", messages);
        assert_eq!(installed, settings(home));
        assert_eq!(1, backups(home));

        uninstall(home).unwrap();
        let restored = settings(home);
        assert_eq!(original, restored["statusLine"]);
        assert_eq!("keep-me", restored["model"]);
        assert!(!home.join(CLAUDE_STATUSLINE_CACHE_DIRNAME).join(SIDECAR_NAME).exists());
    }

    #[test]
    fn uninstall_removes_the_statusline_key_when_none_existed() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(home.join("settings.json"), r#"{"model": "keep-me"}"#).unwrap();

        install(home, Path::new("/opt/bin/agents-live"), None).unwrap();
        assert_eq!("/opt/bin/agents-live statusline ''", settings(home)["statusLine"]["command"]);

        uninstall(home).unwrap();
        let restored = settings(home);
        assert!(restored.get("statusLine").is_none());
        assert_eq!("keep-me", restored["model"]);

        // and a second uninstall has nothing to do
        assert!(uninstall(home).unwrap()[0].starts_with("Not installed"));
    }

    #[test]
    fn install_migrates_the_retired_python_wrapper_keeping_the_original_command() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // what the Python installer left behind
        std::fs::write(
            home.join("settings.json"),
            r#"{"statusLine": {"type": "command", "command": "/src/agents-live/claude-statusline-wrapper '~/.claude/statusline.py'"}}"#,
        )
        .unwrap();
        let cache = home.join(CLAUDE_STATUSLINE_CACHE_DIRNAME);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(SIDECAR_NAME), r#"{"type": "command", "command": "~/.claude/statusline.py"}"#).unwrap();

        install(home, Path::new("/opt/bin/agents-live"), None).unwrap();
        assert_eq!(
            "/opt/bin/agents-live statusline '~/.claude/statusline.py'",
            settings(home)["statusLine"]["command"]
        );
        // the sidecar still holds the true original, so uninstall restores it
        uninstall(home).unwrap();
        assert_eq!("~/.claude/statusline.py", settings(home)["statusLine"]["command"]);
    }

    #[test]
    fn install_from_a_new_location_repoints_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(home.join("settings.json"), r#"{"statusLine": {"type": "command", "command": "~/s.sh"}}"#).unwrap();
        install(home, Path::new("/old/agents-live"), None).unwrap();
        install(home, Path::new("/new/agents-live"), None).unwrap();
        assert_eq!("/new/agents-live statusline '~/s.sh'", settings(home)["statusLine"]["command"]);
    }

    #[test]
    fn a_hooked_statusline_with_no_record_of_the_original_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(
            home.join("settings.json"),
            r#"{"statusLine": {"type": "command", "command": "/x/claude-statusline-wrapper '~/s.sh'"}}"#,
        )
        .unwrap();
        assert!(install(home, Path::new("/opt/bin/agents-live"), None).is_err());
        assert_eq!("/x/claude-statusline-wrapper '~/s.sh'", settings(home)["statusLine"]["command"]);
    }

    #[test]
    fn a_non_default_claude_home_is_passed_to_the_hook() {
        assert_eq!(
            "/opt/bin/agents-live --claude-home '/tmp/my claude' statusline ''",
            hook_command(Path::new("/opt/bin/agents-live"), Some("/tmp/my claude"), "")
        );
    }
}
