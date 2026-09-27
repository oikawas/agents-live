//! Runs the real binary the way Claude Code runs a statusLine command:
//! a shell command line, with the JSON payload on stdin.

use std::io::Write;
use std::process::{Command, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_agents-live");

fn run(args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(EXE)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn installed_command_line_caches_limits_and_keeps_the_original_status_line() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{"statusLine": {"type": "command", "command": "cat >/dev/null; printf %s original-line"}}"#,
    )
    .unwrap();

    let install = run(&["--claude-home", &home, "install-statusline"], "");
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));

    // Claude Code runs the configured command through a shell
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.json")).unwrap()).unwrap();
    let command = settings["statusLine"]["command"].as_str().unwrap().to_string();
    assert!(command.contains(EXE) && command.contains("--claude-home"), "{}", command);

    let payload = r#"{"session_id":"sess-9","rate_limits":{"seven_day":{"used_percentage":12.5,"resets_at":1788609600}}}"#;
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!("original-line", String::from_utf8_lossy(&output.stdout));
    let cached = std::fs::read_to_string(dir.path().join("agents-live-statusline-cache/sess-9.json")).unwrap();
    assert!(cached.contains("12.5"), "{}", cached);

    let uninstall = run(&["--claude-home", &home, "install-statusline", "--uninstall"], "");
    assert!(uninstall.status.success());
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.json")).unwrap()).unwrap();
    assert_eq!("cat >/dev/null; printf %s original-line", settings["statusLine"]["command"]);
}

#[test]
fn the_hook_never_starts_the_dashboard() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_string_lossy().to_string();
    let output = run(&["--claude-home", &home, "statusline"], "not json");
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "{}", String::from_utf8_lossy(&output.stdout));
}
