use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Discover Codex thread ids whose writer lock file is currently held by a
/// live `codex` process, via `lsof -Fpcn` on every `*.lock` file. Mirrors
/// `held_thread_ids` in the Python script.
pub fn held_thread_ids(lock_dir: &Path) -> (HashMap<String, i64>, Option<String>) {
    let lock_files: Vec<PathBuf> = match std::fs::read_dir(lock_dir) {
        Ok(entries) => {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|ext| ext == "lock").unwrap_or(false))
                .collect();
            files.sort();
            files
        }
        Err(_) => Vec::new(),
    };
    if lock_files.is_empty() {
        return (HashMap::new(), None);
    }

    let mut cmd = Command::new("lsof");
    cmd.arg("-n").arg("-Fpcn");
    for f in &lock_files {
        cmd.arg(f);
    }

    let output = match run_with_timeout(cmd, Duration::from_secs(5)) {
        Ok(Ok(out)) => out,
        Ok(Err(_not_found)) => {
            return (
                HashMap::new(),
                Some("lsof not found; install lsof to discover live Codex processes".to_string()),
            );
        }
        Err(err) => return (HashMap::new(), Some(format!("lsof failed: {}", err))),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut mapping: HashMap<String, i64> = HashMap::new();
    let mut pid: Option<i64> = None;
    let mut command = String::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            if let Ok(p) = rest.parse::<i64>() {
                pid = Some(p);
                command.clear();
            }
        } else if let Some(rest) = line.strip_prefix('c') {
            command = rest.to_string();
        } else if let Some(rest) = line.strip_prefix('n') {
            if pid.is_some() && command.to_lowercase().starts_with("codex") {
                if let Some(name) = Path::new(rest).file_name().and_then(|s| s.to_str()) {
                    if let Some(id) = name.strip_suffix(".lock") {
                        mapping.insert(id.to_string(), pid.unwrap());
                    }
                }
            }
        }
    }

    let code = output.status.code().unwrap_or(-1);
    if mapping.is_empty() && code != 0 && code != 1 {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("lsof exited {}", code)
        } else {
            stderr
        };
        return (HashMap::new(), Some(message));
    }
    (mapping, None)
}

fn run_with_timeout(
    mut cmd: Command,
    timeout: Duration,
) -> Result<Result<std::process::Output, std::io::Error>, String> {
    // std::process has no built-in timeout; spawn + poll to approximate the
    // Python `subprocess.run(..., timeout=5)` behavior.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Err(e)),
        Err(e) => return Err(e.to_string()),
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let output = child.wait_with_output().map_err(|e| e.to_string())?;
                return Ok(Ok(output));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}
