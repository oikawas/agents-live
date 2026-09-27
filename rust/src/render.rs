use crate::cli::Args;
use crate::claude::sessions::{discover_claude_sessions, find_claude_jsonl};
use crate::claude::transcript::ClaudeSessionState;
use crate::claude::usage::ClaudeUsageSource;
use crate::claude::weekly::{claude_five_hour_sample, claude_weekly_sample};
use crate::codex::db::load_threads;
use crate::codex::lock::held_thread_ids;
use crate::codex::rollout::SessionState;
use crate::codex::usage::CodexUsageSource;
use crate::model::*;
use crate::theme::{self, Palette};
use crate::theme::{NO_REVERSE, REVERSE, RESET};
use crate::usage_tracker::ThirtyDayUsageTracker;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

static SGR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());

pub struct Dashboard {
    pub args: Args,
    db_path: PathBuf,
    lock_dir: PathBuf,
    usage_tracker: ThirtyDayUsageTracker,
    trackers: HashMap<String, SessionState>,
    last_threads: Vec<Thread>,
    claude_home: PathBuf,
    claude_usage_tracker: ThirtyDayUsageTracker,
    claude_trackers: HashMap<String, ClaudeSessionState>,
    claude_path_cache: HashMap<String, String>,
    /// Codex lock-holder pids that are the app-server daemon rather than a
    /// session's own process (Codex 0.157+), memoized per pid.
    codex_daemon_pids: HashMap<i64, bool>,
    pub error: Option<String>,
    color: bool,
    palette: Palette,
    all_idle_style: &'static str,
    claude_palette: Palette,
}

impl Dashboard {
    pub fn new(args: Args) -> Self {
        let home = args.codex_home_path();
        let db_path = home.join("state_5.sqlite");
        let lock_dir = home.join("thread-writer-locks");
        let claude_home = args.claude_home_path();
        let usage_tracker = ThirtyDayUsageTracker::new(Arc::new(CodexUsageSource {
            db_path: db_path.clone(),
        }));
        let claude_usage_tracker = ThirtyDayUsageTracker::new(Arc::new(ClaudeUsageSource {
            claude_home: claude_home.clone(),
        }));
        let color = args.color == "always"
            || (args.color == "auto" && atty_stdout());
        let palette = theme::theme(&args.theme);
        let all_idle_style = theme::all_idle_bg_style(&args.all_idle_bgcolor);
        let claude_palette = theme::claude_theme();
        Dashboard {
            args,
            db_path,
            lock_dir,
            usage_tracker,
            trackers: HashMap::new(),
            last_threads: Vec::new(),
            claude_home,
            claude_usage_tracker,
            claude_trackers: HashMap::new(),
            claude_path_cache: HashMap::new(),
            codex_daemon_pids: HashMap::new(),
            error: None,
            color,
            palette,
            all_idle_style,
            claude_palette,
        }
    }

    fn palette_for(&self, origin: &str) -> &Palette {
        if origin == "claude" {
            &self.claude_palette
        } else {
            &self.palette
        }
    }

    fn painted(&self, text: &str, style: &str, origin: &str) -> String {
        if !self.color {
            return text.to_string();
        }
        let color = self.palette_for(origin).get(style).copied().unwrap_or("");
        format!("{}{}{}", color, text, RESET)
    }

    fn painted_header(&self, text: &str, style: &str) -> String {
        self.painted(text, style, "codex")
    }

    fn reversed_row(&self, text: &str, plain_text: &str, width: i64) -> String {
        if !self.color {
            return text.to_string();
        }
        let padding = " ".repeat((width as usize).saturating_sub(1).saturating_sub(display_width(plain_text)));
        let reversed_text = format!("{}{}", text, padding).replace(RESET, &format!("{}{}", RESET, REVERSE));
        format!("{}{}{}", REVERSE, reversed_text, RESET)
    }

    pub fn snapshot(&mut self) -> (Vec<Thread>, HashMap<String, AgentRowState>) {
        let mut threads: Vec<Thread> = Vec::new();
        let mut states: HashMap<String, AgentRowState> = HashMap::new();
        let mut errors: Vec<String> = Vec::new();

        if self.args.codex == "on" {
            self.usage_tracker.refresh(!self.args.once);
            let (held, lock_error) = held_thread_ids(&self.lock_dir);
            if let Some(err) = lock_error {
                errors.push(err);
            }
            let live_pids: std::collections::HashSet<i64> = held.values().copied().collect();
            self.codex_daemon_pids.retain(|pid, _| live_pids.contains(pid));
            let unknown: Vec<i64> = live_pids.iter().copied().filter(|p| !self.codex_daemon_pids.contains_key(p)).collect();
            if !unknown.is_empty() {
                let kinds = codex_app_server_pids(&unknown);
                for pid in unknown {
                    self.codex_daemon_pids.insert(pid, kinds.contains(&pid));
                }
            }
            let codex_threads = match load_threads(&self.db_path, &held) {
                Ok(t) => {
                    self.last_threads = t.clone();
                    t
                }
                Err(err) => {
                    errors.push(format!("state database unavailable: {}", err));
                    self.last_threads.clone()
                }
            };

            let active_ids: std::collections::HashSet<&String> =
                codex_threads.iter().map(|t| &t.id).collect();
            self.trackers.retain(|id, _| active_ids.contains(id));
            for thread in &codex_threads {
                let tracker = self
                    .trackers
                    .entry(thread.id.clone())
                    .or_insert_with(|| SessionState::new(thread.rollout_path.clone()));
                if tracker.path != thread.rollout_path {
                    *tracker = SessionState::new(thread.rollout_path.clone());
                }
                tracker.refresh();
                states.insert(
                    thread.id.clone(),
                    AgentRowState {
                        phase: tracker.phase.clone(),
                        phase_at: tracker.phase_at,
                        activity: tracker.activity.clone(),
                        total_tokens: tracker.total_tokens,
                        weekly_used_percent: tracker.weekly_used_percent,
                        weekly_sample_at: tracker.weekly_sample_at,
                        weekly_limit_id: tracker.weekly_limit_id.clone(),
                        weekly_resets_at: tracker.weekly_resets_at,
                        five_hour_used_percent: tracker.five_hour_used_percent,
                        five_hour_sample_at: tracker.five_hour_sample_at,
                        five_hour_resets_at: tracker.five_hour_resets_at,
                        service_tier: tracker.service_tier.clone(),
                    },
                );
            }
            threads.extend(codex_threads);
        }

        if self.args.claude == "on" {
            self.claude_usage_tracker.refresh(!self.args.once);
            let (claude_threads, claude_states, claude_error) = self.snapshot_claude();
            if let Some(err) = claude_error {
                errors.push(err);
            }
            threads.extend(claude_threads);
            states.extend(claude_states);
        }

        self.error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
        (threads, states)
    }

    fn snapshot_claude(
        &mut self,
    ) -> (Vec<Thread>, HashMap<String, AgentRowState>, Option<String>) {
        let (sessions, error) = discover_claude_sessions(&self.claude_home);
        let mut threads = Vec::new();
        let mut states = HashMap::new();
        let mut active_session_ids: std::collections::HashSet<String> = Default::default();

        for session in &sessions {
            let session_id = match session.get("sessionId").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let pid = session.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);
            let path = match find_claude_jsonl(&self.claude_home, &session_id, &mut self.claude_path_cache) {
                Some(p) => p,
                None => continue,
            };
            active_session_ids.insert(session_id.clone());

            let tracker = self
                .claude_trackers
                .entry(session_id.clone())
                .or_insert_with(|| ClaudeSessionState::new(path.clone()));
            if tracker.path != path {
                *tracker = ClaudeSessionState::new(path.clone());
            }
            tracker.refresh();

            let status = session.get("status").and_then(|v| v.as_str());
            let busy = claude_status_is_running(status);
            let status_at_ms = session.get("statusUpdatedAt").and_then(|v| v.as_f64());
            let phase_at = status_at_ms.map(|v| v / 1000.0).unwrap_or(tracker.last_event_at);
            let started_at_ms = session.get("startedAt").and_then(|v| v.as_f64());
            let name = session.get("name").and_then(|v| v.as_str());
            let name_source = session.get("nameSource").and_then(|v| v.as_str());
            let explicit_name = match name {
                Some(n) if !n.trim().is_empty() && name_source != Some("derived") => {
                    Some(n.trim().to_string())
                }
                _ => None,
            };
            let cwd = session.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string();

            let main_thread = Thread {
                id: session_id.clone(),
                pid,
                rollout_path: path.clone(),
                source: String::new(),
                cwd: cwd.clone(),
                title: tracker.first_message.clone().unwrap_or_default(),
                session_name: explicit_name,
                model: tracker.current_model.clone(),
                tokens_used: Some(tracker.total_tokens),
                nickname: None,
                role: None,
                agent_path: None,
                created_at_ms: started_at_ms.map(|v| v as i64).unwrap_or(0),
                reasoning_effort: tracker.current_effort.clone(),
                parent_id: None,
                kind: "main".to_string(),
                depth: 0,
                origin: "claude".to_string(),
            };
            let (weekly_used_percent, weekly_sample_at, weekly_resets_at) =
                claude_weekly_sample(&self.claude_home, &session_id);
            let (five_hour_used_percent, five_hour_sample_at, five_hour_resets_at) =
                claude_five_hour_sample(&self.claude_home, &session_id);
            states.insert(
                main_thread.id.clone(),
                AgentRowState {
                    phase: if busy { "run".to_string() } else { "done".to_string() },
                    phase_at,
                    activity: if busy {
                        tracker.activity.clone()
                    } else if status == Some("shell") {
                        // Idle, but a background shell it started is still
                        // alive -- worth a hint, not a RUN.
                        "bg shell".to_string()
                    } else {
                        "complete".to_string()
                    },
                    total_tokens: Some(tracker.total_tokens),
                    weekly_used_percent,
                    weekly_sample_at,
                    weekly_limit_id: None,
                    weekly_resets_at,
                    five_hour_used_percent,
                    five_hour_sample_at,
                    five_hour_resets_at,
                    service_tier: None,
                },
            );
            threads.push(main_thread);

            let process_started_at = started_at_ms.map(|v| v / 1000.0);
            for (tool_id, info) in tracker.agents.iter_mut() {
                let started_at = info.started_at;
                // Kept on the tracker: an end inferred from an idle status
                // must survive the session going busy again.
                let done_at = claude_child_done_at(info, status, phase_at, process_started_at);
                info.done_at = done_at;
                let spawn_id = format!("{}:{}", session_id, tool_id);
                let spawn_thread = Thread {
                    id: spawn_id.clone(),
                    pid,
                    rollout_path: path.clone(),
                    source: String::new(),
                    cwd: cwd.clone(),
                    title: info.description.clone().unwrap_or_default(),
                    session_name: None,
                    model: None,
                    tokens_used: None,
                    nickname: info.subagent_type.clone(),
                    role: None,
                    agent_path: None,
                    created_at_ms: (started_at * 1000.0) as i64,
                    reasoning_effort: None,
                    parent_id: Some(session_id.clone()),
                    kind: "spawn".to_string(),
                    depth: 1,
                    origin: "claude".to_string(),
                };
                states.insert(
                    spawn_thread.id.clone(),
                    AgentRowState {
                        phase: if done_at.is_some() { "done".to_string() } else { "run".to_string() },
                        phase_at: done_at.unwrap_or(started_at),
                        activity: if done_at.is_some() { "complete".to_string() } else { "running".to_string() },
                        total_tokens: None,
                        weekly_used_percent: None,
                        weekly_sample_at: None,
                        weekly_limit_id: None,
                        weekly_resets_at: None,
                        five_hour_used_percent: None,
                        five_hour_sample_at: None,
                        five_hour_resets_at: None,
                        service_tier: None,
                    },
                );
                threads.push(spawn_thread);
            }
        }

        self.claude_trackers.retain(|id, _| active_session_ids.contains(id));
        (threads, states, error)
    }

    fn status(&self, thread: &Thread, state: &AgentRowState) -> (&'static str, &'static str, &'static str) {
        match state.phase.as_str() {
            "run" => ("RUN", "\u{25CF}", "run"),
            "done" => {
                if thread.kind == "main" {
                    ("IDLE", "\u{25CC}", "idle")
                } else {
                    ("DONE", "\u{2713}", "done")
                }
            }
            "stop" => ("STOP", "!", "stop"),
            _ => ("START", "\u{25D0}", "start"),
        }
    }

    fn sources_label(&self) -> String {
        let mut labels = Vec::new();
        if self.args.codex == "on" {
            labels.push("Codex");
        }
        if self.args.claude == "on" {
            labels.push("Claude");
        }
        if labels.is_empty() {
            "Codex/Claude".to_string()
        } else {
            labels.join(" or ")
        }
    }

    fn header_title(&self) -> &'static str {
        if self.args.codex == "on" && self.args.claude == "on" {
            "AGENTS LIVE"
        } else if self.args.claude == "on" {
            "CLAUDE AGENTS LIVE"
        } else {
            "CODEX AGENTS LIVE"
        }
    }

    fn all_idle_banner_title(&self) -> &'static str {
        if self.args.codex == "on" && self.args.claude == "on" {
            "ALL AGENTS IDLE"
        } else if self.args.claude == "on" {
            "ALL CLAUDE SESSIONS IDLE"
        } else {
            "ALL CODEX SESSIONS IDLE"
        }
    }

    fn usage_for(&self, origin: &str) -> (Option<i64>, Option<f64>, Option<i64>) {
        let tracker = if origin == "claude" { &self.claude_usage_tracker } else { &self.usage_tracker };
        (tracker.priced_tokens(), tracker.cost_usd(), tracker.unpriced_tokens())
    }

    fn source_counts(visible: &[&Thread], states: &HashMap<String, AgentRowState>, origin: &str) -> (i64, i64, i64) {
        let subset: Vec<&&Thread> = visible.iter().filter(|t| t.origin == origin).collect();
        let running = subset.iter().filter(|t| states[&t.id].phase == "run").count() as i64;
        let child_count = subset.iter().filter(|t| t.kind == "spawn").count() as i64;
        let process_count = subset
            .iter()
            .map(|t| t.pid)
            .collect::<std::collections::HashSet<_>>()
            .len() as i64;
        (running, child_count, process_count)
    }

    /// The weekly-limit sample to show for one source (percent + reset
    /// time). Codex can report more than one concurrently active
    /// rate-limit pool (`rate_limits.limit_id`); this takes the freshest
    /// sample within each pool first, then shows whichever pool has the
    /// least remaining. Mirrors `Dashboard.latest_weekly_state`.
    fn latest_weekly_state<'a>(
        visible: &[&'a Thread],
        states: &'a HashMap<String, AgentRowState>,
        origin: &str,
    ) -> Option<&'a AgentRowState> {
        Self::latest_limit_state(visible, states, origin, |s| (s.weekly_used_percent, s.weekly_sample_at))
    }

    /// Same selection as `latest_weekly_state`, for the 5-hour window.
    fn latest_five_hour_state<'a>(
        visible: &[&'a Thread],
        states: &'a HashMap<String, AgentRowState>,
        origin: &str,
    ) -> Option<&'a AgentRowState> {
        Self::latest_limit_state(visible, states, origin, |s| (s.five_hour_used_percent, s.five_hour_sample_at))
    }

    /// `sample` picks one limit window's (used_percent, sample_at) out of a
    /// row state; rows without a used_percent for that window are skipped.
    fn latest_limit_state<'a>(
        visible: &[&'a Thread],
        states: &'a HashMap<String, AgentRowState>,
        origin: &str,
        sample: fn(&AgentRowState) -> (Option<f64>, Option<f64>),
    ) -> Option<&'a AgentRowState> {
        let candidates: Vec<&AgentRowState> = visible
            .iter()
            .filter(|t| t.origin == origin)
            .map(|t| &states[&t.id])
            .filter(|s| sample(s).0.is_some())
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let mut freshest_per_pool: HashMap<Option<String>, &AgentRowState> = HashMap::new();
        for state in candidates {
            let pool = state.weekly_limit_id.clone();
            let sample_at = sample(state).1.unwrap_or(f64::NEG_INFINITY);
            let replace = match freshest_per_pool.get(&pool) {
                None => true,
                Some(current) => {
                    let current_at = sample(current).1.unwrap_or(f64::NEG_INFINITY);
                    sample_at > current_at
                }
            };
            if replace {
                freshest_per_pool.insert(pool, state);
            }
        }
        freshest_per_pool
            .into_values()
            .min_by(|a, b| {
                let ra = weekly_remaining_percent(sample(a).0).unwrap_or(0.0);
                let rb = weekly_remaining_percent(sample(b).0).unwrap_or(0.0);
                ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    fn latest_weekly_percent(
        visible: &[&Thread],
        states: &HashMap<String, AgentRowState>,
        origin: &str,
    ) -> Option<f64> {
        Self::latest_weekly_state(visible, states, origin).and_then(|s| s.weekly_used_percent)
    }

    /// "  5h nn%" to follow the weekly figure. Claude always shows it
    /// ("5h --" when the statusline cache has no sample yet), like its
    /// weekly figure. Codex currently imposes no 5-hour window, so it is
    /// shown only once some visible thread actually reports one.
    fn five_hour_summary_part(
        &self,
        origin: &str,
        visible: &[&Thread],
        states: &HashMap<String, AgentRowState>,
    ) -> String {
        let state = Self::latest_five_hour_state(visible, states, origin);
        if state.is_none() && origin != "claude" {
            return String::new();
        }
        let used = state.and_then(|s| five_hour_effective_used(s.five_hour_used_percent, s.five_hour_resets_at, now_secs()));
        format!("  {}", five_hour_label(used))
    }

    fn source_summary_line(
        &self,
        origin: &str,
        width: i64,
        visible: &[&Thread],
        states: &HashMap<String, AgentRowState>,
    ) -> String {
        let source_tag = if origin == "claude" { "CLAUDE" } else { "CODEX" };
        let weekly_used_percent = Self::latest_weekly_percent(visible, states, origin);
        let (priced_tokens, priced_cost, unpriced_tokens) = self.usage_for(origin);
        let tokens_label = billions_token_label(priced_tokens);
        let cost_label = format!(
            "{}{}",
            estimated_cost_label(priced_cost),
            unpriced_marker(priced_tokens, unpriced_tokens)
        );
        let (running, child_count, process_count) = Self::source_counts(visible, states, origin);
        let counts = format!("{} running \u{b7} {} subagents \u{b7} {} processes", running, child_count, process_count);
        let weekly_text = weekly_label(weekly_used_percent);
        let five_hour_text = self.five_hour_summary_part(origin, visible, states);
        let summary = format!(
            "{}  {}{}  {} {}   {}",
            source_tag, weekly_text, five_hour_text, tokens_label, cost_label, counts
        );
        let fitted = fit(&summary, width, false);

        let weekly_remaining = weekly_remaining_percent(weekly_used_percent);
        let is_high = self.color && weekly_remaining.map(|r| r >= WEEKLY_HIGH_REMAINING_PERCENT).unwrap_or(false);
        let is_critical = self.color && weekly_remaining.map(|r| r < WEEKLY_CRITICAL_REMAINING_PERCENT).unwrap_or(false);
        let style = if is_high { "weekly_high" } else { "header" };

        if !self.color {
            return fitted;
        }

        let palette_color = self.palette_for(origin).get(style).copied().unwrap_or("");
        // fit() only ever truncates from the end (appending "…"), and
        // weekly_value sits near the start of the summary, so a missing
        // match only happens at pathologically narrow widths -- fall back
        // to the plain (unhighlighted) line rather than mis-render.
        let weekly_value = weekly_text.strip_prefix("Weekly ").unwrap_or(&weekly_text);
        if is_critical {
            // Below 5% remaining always gets a red highlight on just the
            // figure, regardless of --weekly-reverse -- this is a warning,
            // not a display preference, so it isn't optional.
            let critical_color = self.palette_for(origin).get("weekly_critical").copied().unwrap_or("");
            if let Some(index) = fitted.find(weekly_value) {
                let before = &fitted[..index];
                let after = &fitted[index + weekly_value.len()..];
                return format!(
                    "{}{}{}{}{}{}{}",
                    palette_color, before, critical_color, weekly_value, palette_color, after, RESET
                );
            }
            return format!("{}{}{}", palette_color, fitted, RESET);
        }
        // Reverse video is only for the figure itself (e.g. "86%" or "--"),
        // never the "Weekly " label, and never on top of the >=95%-remaining
        // green highlight -- that state already carries its own meaning, and
        // reversing it too would muddy rather than emphasize it.
        if self.args.weekly_reverse && style != "weekly_high" {
            if let Some(index) = fitted.find(weekly_value) {
                let before = &fitted[..index];
                let after = &fitted[index + weekly_value.len()..];
                return format!(
                    "{}{}{}{}{}{}{}",
                    palette_color, before, REVERSE, weekly_value, NO_REVERSE, after, RESET
                );
            }
        }
        format!("{}{}{}", palette_color, fitted, RESET)
    }

    pub fn render(&self, threads: &[Thread], states: &HashMap<String, AgentRowState>) -> String {
        let (cols, rows) = terminal_size();
        self.render_sized(threads, states, cols, rows)
    }

    /// Renders at an explicit terminal size. `render()` is this with the real
    /// terminal size; tests use this directly because `terminal_size()` reads
    /// process-global `COLUMNS`/`LINES`, which cannot be set safely from
    /// tests running in parallel threads. (The Python version got the same
    /// seam by mocking `shutil.get_terminal_size`.)
    pub fn render_sized(
        &self,
        threads: &[Thread],
        states: &HashMap<String, AgentRowState>,
        cols: u16,
        rows: u16,
    ) -> String {
        let now = now_secs();
        let width = (cols as i64).max(72);
        let height = (rows as i64).max(1);
        let show_tokens = self.args.tokens == "on";

        let mut visible: Vec<&Thread> = threads
            .iter()
            .filter(|t| self.args.show_guardians || t.kind != "guardian")
            .collect();
        if let Some(process) = self.args.process {
            visible.retain(|t| t.pid == process);
        }
        if self.args.hide_done {
            visible.retain(|t| states[&t.id].phase != "done" || t.kind == "main");
        }

        // "Nothing is working" rather than "everything finished cleanly":
        // a stopped or aborted thread is not going to do anything further
        // on its own, so it counts as settled here alongside done. Only
        // run and start keep the banner away.
        let all_idle = !visible.is_empty()
            && visible.iter().any(|t| t.kind == "main")
            && visible
                .iter()
                .all(|t| matches!(states[&t.id].phase.as_str(), "done" | "stop"));
        let show_all_idle_banner = all_idle && self.args.all_idle_banner == "on";
        let stamp = local_time_hms();
        let title = self.header_title();

        let header = if show_all_idle_banner {
            let banner_title = self.all_idle_banner_title();
            let summary = format!("\u{2588}\u{2588}\u{2588}\u{2588}  {}  \u{2588}\u{2588}\u{2588}\u{2588}  {}", banner_title, stamp);
            let fitted = fit(&summary, width - 1, true);
            if self.color {
                format!("{}{}{}", self.all_idle_style, fitted, RESET)
            } else {
                fitted
            }
        } else {
            // Left to right in the same order as the source groups below
            // (--group-order), so each reset figure lines up with its list.
            let origins: [(&str, &str); 2] = if self.args.group_order == "claude-first" {
                [("claude", "Claude"), ("codex", "Codex")]
            } else {
                [("codex", "Codex"), ("claude", "Claude")]
            };
            let mut parts = vec![format!("{}  {}", title, stamp)];
            for (origin, label) in origins {
                let enabled = if origin == "claude" { &self.args.claude } else { &self.args.codex };
                if enabled != "on" {
                    continue;
                }
                let resets_at = Self::latest_weekly_state(&visible, states, origin).and_then(|s| s.weekly_resets_at);
                parts.push(format!("{}{}: {}", label, WEEKLY_RESET_EMOJI, weekly_reset_elapsed_label(resets_at, now)));
            }
            let summary = parts.join("  ");
            self.painted_header(&fit(&summary, width, false), "title")
        };
        let mut lines = vec![self.painted_header(&"\u{2500}".repeat(width as usize), "border")];

        if visible.is_empty() {
            let sources = self.sources_label();
            lines.push(String::new());
            lines.push(self.painted_header(&format!("  No live {} agent threads found.", sources), "dim"));
            if let Some(err) = &self.error {
                lines.push(self.painted_header(&format!("  {}", fit(err, width - 2, false)), "stop"));
            }
            lines.push(String::new());
            lines.push(self.painted_header(&format!("  Waiting for {}\u{2026}", sources), "dim"));
            let output = self.fit_height(&header, &lines, height, width);
            return self.all_idle_red_screen(&output, show_all_idle_banner, height, width);
        }

        // One block per session: a process and the root thread it runs. A
        // Codex CLI process holds one root, but the Codex app-server daemon
        // holds every session's threads under its own pid.
        let parents: HashMap<&str, Option<&str>> =
            threads.iter().map(|t| (t.id.as_str(), t.parent_id.as_deref())).collect();
        let root_of = |thread: &Thread| -> String {
            let mut current = thread.id.as_str();
            let mut steps = 0;
            while let Some(Some(parent)) = parents.get(current) {
                if !parents.contains_key(parent) || steps > 64 {
                    break;
                }
                current = parent;
                steps += 1;
            }
            current.to_string()
        };
        let mut by_pid: Vec<(GroupKey, Vec<&Thread>)> = Vec::new();
        {
            let mut map: HashMap<GroupKey, Vec<&Thread>> = HashMap::new();
            let mut order: Vec<GroupKey> = Vec::new();
            for thread in &visible {
                let key = (thread.origin.clone(), thread.pid, root_of(thread));
                if !map.contains_key(&key) {
                    order.push(key.clone());
                }
                map.entry(key).or_default().push(thread);
            }
            for key in order {
                let group = map.remove(&key).unwrap();
                by_pid.push((key, group));
            }
        }
        by_pid.sort_by(|a, b| self.group_sort_key(&a.0, &a.1).cmp(&self.group_sort_key(&b.0, &b.1)));

        let compact_groups = self.args.compact_groups == "on";
        let mut current_origin: Option<String> = None;
        for (group_index, ((origin, pid, _root), group)) in by_pid.iter().enumerate() {
            if current_origin.as_deref() != Some(origin.as_str()) {
                if current_origin.is_some() && !compact_groups {
                    lines.push(String::new());
                }
                lines.push(self.source_summary_line(origin, width, &visible, states));
                current_origin = Some(origin.clone());
            } else if group_index != 0 && !compact_groups {
                lines.push(String::new());
            }

            let group_ids: std::collections::HashSet<&str> = group.iter().map(|t| t.id.as_str()).collect();
            let roots: Vec<&&Thread> = group
                .iter()
                .filter(|t| t.parent_id.is_none() || !group_ids.contains(t.parent_id.as_deref().unwrap_or("")))
                .collect();
            let project = roots.first().map(|t| t.project()).unwrap_or_else(|| group[0].project());
            let root_id: String = roots
                .first()
                .map(|t| t.id.clone())
                .unwrap_or_else(|| group[0].id.clone())
                .chars()
                .take(8)
                .collect();
            let group_running = group.iter().filter(|t| states[&t.id].phase == "run").count();
            let source_tag = if *origin == "claude" { "CLAUDE" } else { "CODEX" };
            let process_label = if *origin == "codex" && self.codex_daemon_pids.get(pid) == Some(&true) {
                "app-server"
            } else {
                "PID"
            };
            let group_title = format!(
                "{}  {} {}  {}  {}  {} running",
                source_tag, process_label, pid, project, root_id, group_running
            );
            lines.push(self.painted(&format!("\u{256D}\u{2500} {}", fit(&group_title, width - 3, false)), "border", origin));

            let ordered = order_group(group);
            let badge_width = group
                .iter()
                .map(|t| display_width(service_tier_badge(states[&t.id].service_tier.as_deref())))
                .max()
                .unwrap_or(0) as i64;

            for thread in &ordered {
                let state = &states[&thread.id];
                let (status, symbol, style) = self.status(thread, state);
                let tree = if thread.kind == "main" {
                    "\u{25C6}".to_string()
                } else {
                    format!("{}\u{2514}\u{2500}", "  ".repeat((thread.depth - 1).max(0) as usize))
                };
                let elapsed = age(now - state.phase_at);
                let name_width = display_width_bound(group, |t| t.name(), 8, 14);
                let model_width = display_width_bound(group, |t| t.model.clone().unwrap_or_else(|| "--".to_string()), 9, 16);
                let effort_width = display_width_bound(group, |t| t.reasoning_effort.clone().unwrap_or_else(|| "--".to_string()), 2, 8);
                let mut token_width = 0i64;
                let mut tokens = String::new();
                if show_tokens {
                    tokens = compact_tokens(state.total_tokens.or(thread.tokens_used));
                    token_width = group
                        .iter()
                        .map(|t| {
                            let tt = states[&t.id].total_tokens.or(t.tokens_used);
                            display_width(&compact_tokens(tt)) as i64
                        })
                        .max()
                        .unwrap_or(9)
                        .max(9)
                        .min(12);
                }

                let prefix_plain = format!("\u{2502} {} {}", symbol, fit(status, 5, true));
                let tree_plain = fit(&tree, 4, true);
                let name_plain = fit(&thread.name(), name_width, true);
                let activity_plain = fit(&state.activity, 9, true);
                let age_plain = fit(&elapsed, 9, true);
                let badge = service_tier_badge(state.service_tier.as_deref());
                let badge_plain = if badge_width > 0 { fit(badge, badge_width, true) } else { String::new() };
                let badge_padding = if badge_width > 0 {
                    " ".repeat((badge_width as usize).saturating_sub(display_width(badge)))
                } else {
                    String::new()
                };
                let model_plain = fit(&thread.model.clone().unwrap_or_else(|| "--".to_string()), model_width, true);
                let effort_plain = fit(&thread.reasoning_effort.clone().unwrap_or_else(|| "--".to_string()), effort_width, true);
                let tokens_plain = if show_tokens { fit(&tokens, token_width, true) } else { String::new() };

                let common_width = display_width(&prefix_plain)
                    + display_width(&tree_plain)
                    + display_width(&name_plain)
                    + display_width(&activity_plain)
                    + display_width(&age_plain)
                    + 4;
                let badge_segment_width = if badge_width > 0 { display_width(&badge_plain) + 1 } else { 0 };
                let wide_width = common_width
                    + badge_segment_width
                    + display_width(&model_plain)
                    + display_width(&effort_plain)
                    + (if show_tokens { display_width(&tokens_plain) } else { 0 })
                    + (if show_tokens { 3 } else { 2 });
                let wide_layout = width - wide_width as i64 - 1 >= 12;
                let task_width = (width - (if wide_layout { wide_width as i64 + 1 } else { common_width as i64 + 1 })).max(12);

                let prefix = self.painted(&prefix_plain, style, &thread.origin);
                let tree_text = self.painted(&tree_plain, "border", &thread.origin);
                let name_text = self.painted(&name_plain, "name", &thread.origin);
                let activity_style = if state.phase != "done" { style } else { "done" };
                let activity_text = self.painted(&activity_plain, activity_style, &thread.origin);
                let age_text = self.painted(&age_plain, "dim", &thread.origin);
                let badge_text = if badge_width > 0 {
                    let mut badge_glyph_text = if !badge.is_empty() {
                        self.painted(badge, "header", &thread.origin)
                    } else {
                        String::new()
                    };
                    if state.phase == "run" && self.color && !badge.is_empty() {
                        badge_glyph_text = format!("{}{}", NO_REVERSE, badge_glyph_text);
                    }
                    let badge_padding_text = if !badge_padding.is_empty() {
                        self.painted(&badge_padding, "header", &thread.origin)
                    } else {
                        String::new()
                    };
                    format!("{}{}", badge_glyph_text, badge_padding_text)
                } else {
                    String::new()
                };
                let model_value = thread.model.clone().unwrap_or_else(|| "--".to_string());
                let model_text = if let (Some(model_style), true) =
                    (theme::model_style(&model_value), self.color)
                {
                    if model_plain.starts_with(&model_value) {
                        let rest: String =
                            model_plain.chars().skip(model_value.chars().count()).collect();
                        let mut text = format!(
                            "{}{}",
                            self.painted(&model_value, model_style, &thread.origin),
                            self.painted(&rest, "name", &thread.origin)
                        );
                        if state.phase == "run" {
                            text = format!("{}{}", NO_REVERSE, text);
                        }
                        text
                    } else {
                        self.painted(&model_plain, "name", &thread.origin)
                    }
                } else {
                    self.painted(&model_plain, "name", &thread.origin)
                };
                let effort_value = thread.reasoning_effort.clone().unwrap_or_else(|| "--".to_string());
                let effort_text = if let (Some(effort_style), true) = (theme::effort_style(&effort_value), self.color) {
                    let rest: String = effort_plain.chars().skip(effort_value.chars().count()).collect();
                    let mut text = format!(
                        "{}{}",
                        self.painted(&effort_value, effort_style, &thread.origin),
                        self.painted(&rest, "dim", &thread.origin)
                    );
                    if state.phase == "run" {
                        text = format!("{}{}", NO_REVERSE, text);
                    }
                    text
                } else {
                    self.painted(&effort_plain, "dim", &thread.origin)
                };
                let tokens_text = if show_tokens { self.painted(&tokens_plain, "dim", &thread.origin) } else { String::new() };
                let task_text = fit(&thread.task(), task_width, false);
                let model_detail = if badge_width > 0 { format!("{} {}", badge_text, model_text) } else { model_text.clone() };
                let model_detail_plain = if badge_width > 0 { format!("{} {}", badge_plain, model_plain) } else { model_plain.clone() };
                let usage_text = if show_tokens {
                    format!("{} {} {}", model_detail, effort_text, tokens_text)
                } else {
                    format!("{} {}", model_detail, effort_text)
                };
                let usage_plain = if show_tokens {
                    format!("{} {} {}", model_detail_plain, effort_plain, tokens_plain)
                } else {
                    format!("{} {}", model_detail_plain, effort_plain)
                };

                if wide_layout {
                    let line = format!("{} {} {} {} {} {} {}", prefix, tree_text, name_text, activity_text, age_text, usage_text, task_text);
                    let plain_line = format!("{} {} {} {} {} {} {}", prefix_plain, tree_plain, name_plain, activity_plain, age_plain, usage_plain, task_text);
                    let line = if state.phase == "run" { self.reversed_row(&line, &plain_line, width) } else { line };
                    lines.push(line);
                } else {
                    let line = format!("{} {} {} {} {} {}", prefix, tree_text, name_text, activity_text, age_text, task_text);
                    let plain_line = format!("{} {} {} {} {} {}", prefix_plain, tree_plain, name_plain, activity_plain, age_plain, task_text);
                    let line = if state.phase == "run" { self.reversed_row(&line, &plain_line, width) } else { line };
                    lines.push(line);
                    if !self.args.no_wrap {
                        let indent_width = (display_width(&prefix_plain) + display_width(&tree_plain) + 1).max(8) as i64;
                        let detail_indent = fit("\u{2502}   \u{21B3}", indent_width, true);
                        let detail_line = format!("{} {}", self.painted(&detail_indent, "border", &thread.origin), usage_text);
                        let detail_plain = format!("{} {}", detail_indent, usage_plain);
                        let detail_line = if state.phase == "run" { self.reversed_row(&detail_line, &detail_plain, width) } else { detail_line };
                        lines.push(detail_line);
                    }
                }
            }
            lines.push(self.painted(&format!("\u{2570}{}", "\u{2500}".repeat((width - 1) as usize)), "border", origin));
        }

        if let Some(err) = &self.error {
            lines.push(self.painted_header(&fit(&format!("warning: {}", err), width, false), "stop"));
        }
        let output = self.fit_height(&header, &lines, height, width);
        self.all_idle_red_screen(&output, show_all_idle_banner, height, width)
    }

    fn all_idle_red_screen(&self, output: &str, banner_active: bool, height: i64, width: i64) -> String {
        if !(self.args.all_idle_red_screen && banner_active && self.color) {
            return output.to_string();
        }
        let mut rows: Vec<String> = output.lines().map(|s| s.to_string()).collect();
        while (rows.len() as i64) < height {
            rows.push(String::new());
        }
        rows.truncate(height as usize);
        rows.into_iter()
            .map(|row| {
                let stripped = SGR_RE.replace_all(&row, "").to_string();
                format!("{}{}{}", self.all_idle_style, clip_and_pad(&stripped, width - 1), RESET)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fit_height(&self, header: &str, body: &[String], height: i64, width: i64) -> String {
        let quit_hint = self.painted_header("q / Esc / Ctrl-C to stop", "dim");
        if (body.len() as i64) + 2 <= height {
            let mut out = vec![header.to_string()];
            out.extend(body.iter().cloned());
            out.push(quit_hint);
            return out.join("\n");
        }
        if height == 1 {
            return header.to_string();
        }
        let visible_count = (height - 3).max(0) as usize;
        let visible_body = &body[..visible_count.min(body.len())];
        let omitted = body.len() - visible_body.len();
        let marker = self.painted_header(&fit(&format!("\u{2026} {} more rows omitted", omitted), width, false), "dim");
        let mut result = vec![header.to_string()];
        result.extend(visible_body.iter().cloned());
        result.push(marker);
        if height >= 3 {
            result.push(quit_hint);
        }
        result.join("\n")
    }

    fn group_sort_key(&self, key: &GroupKey, group: &[&Thread]) -> (i64, String, i64, String, i64) {
        let (origin, pid, _root) = key;
        let origin_rank: i64 = if self.args.group_order == "claude-first" {
            if origin == "claude" { 0 } else { 1 }
        } else if origin == "codex" {
            0
        } else {
            1
        };
        let earliest = group.iter().min_by_key(|t| (t.created_at_ms, t.id.clone())).unwrap();
        if self.args.group_sort == "created" {
            return (origin_rank, String::new(), earliest.created_at_ms, String::new(), 0);
        }
        let ids: std::collections::HashSet<&str> = group.iter().map(|t| t.id.as_str()).collect();
        let roots: Vec<&&Thread> = group
            .iter()
            .filter(|t| t.parent_id.is_none() || !ids.contains(t.parent_id.as_deref().unwrap_or("")))
            .collect();
        let representative = if roots.is_empty() {
            group.iter().min_by_key(|t| (t.created_at_ms, t.id.clone())).unwrap()
        } else {
            roots.iter().min_by_key(|t| (t.created_at_ms, t.id.clone())).unwrap()
        };
        let label = representative.task();
        (origin_rank, label.to_lowercase(), representative.created_at_ms, origin.clone(), *pid)
    }
}

/// When a background agent/shell child ended. Besides its own
/// task-notification:
/// - children die with the Claude process, so one started before the
///   current process (the session was resumed since) is over;
/// - Claude Code reports "idle"/"waiting" only when no background Bash is
///   left (else it says "shell"), so a bash child that started before that
///   status and is still open lost its notification -- e.g. it went to a
///   session forked off this one. A child started after it is not covered:
///   the status file is read before the transcript catches up.
fn claude_child_done_at(
    info: &crate::claude::transcript::AgentInfo,
    status: Option<&str>,
    status_at: f64,
    process_started_at: Option<f64>,
) -> Option<f64> {
    info.done_at
        .or_else(|| process_started_at.filter(|&p| info.started_at < p))
        .or_else(|| {
            let no_live_shell = matches!(status, Some("idle") | Some("waiting"));
            (info.subagent_type.as_deref() == Some("bash") && no_live_shell && info.started_at <= status_at)
                .then_some(status_at)
        })
}

/// Claude Code's session status is one of busy/shell/idle/waiting. Only
/// "busy" means the model is working: "shell" is an idle session that
/// still has a background shell alive, which can sit there for hours after
/// the turn ended.
fn claude_status_is_running(status: Option<&str>) -> bool {
    status == Some("busy")
}

fn display_width_bound<F: Fn(&Thread) -> String>(group: &[&Thread], f: F, min: i64, max: i64) -> i64 {
    let widest = group.iter().map(|t| display_width(&f(t)) as i64).max().unwrap_or(min);
    widest.max(min).min(max)
}

/// A display block: (origin, lock-holder pid, root thread id).
type GroupKey = (String, i64, String);

/// Which of `pids` are the Codex app-server daemon (`codex app-server`),
/// judged from `ps` since lsof reports only the command name "codex".
fn codex_app_server_pids(pids: &[i64]) -> std::collections::HashSet<i64> {
    let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let output = match std::process::Command::new("ps").args(["-o", "pid=,command=", "-p", &list]).output() {
        Ok(o) => o,
        Err(_) => return Default::default(),
    };
    parse_app_server_pids(&String::from_utf8_lossy(&output.stdout))
}

fn parse_app_server_pids(ps_output: &str) -> std::collections::HashSet<i64> {
    ps_output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, command) = line.split_once(char::is_whitespace)?;
            // ".../bin/codex app-server --listen ...": match on the pair so a
            // path with spaces cannot shift the words.
            let is_daemon = command.starts_with("codex app-server") || command.contains("/codex app-server");
            if is_daemon { pid.parse().ok() } else { None }
        })
        .collect()
}

fn order_group(group: &[&Thread]) -> Vec<Thread> {
    let ids: std::collections::HashSet<&str> = group.iter().map(|t| t.id.as_str()).collect();
    let mut by_parent: HashMap<String, Vec<Thread>> = HashMap::new();
    let mut roots: Vec<Thread> = Vec::new();
    for thread in group {
        if let Some(parent) = &thread.parent_id {
            if ids.contains(parent.as_str()) {
                by_parent.entry(parent.clone()).or_default().push((*thread).clone());
                continue;
            }
        }
        roots.push((*thread).clone());
    }
    roots.sort_by_key(|t| t.created_at_ms);
    for children in by_parent.values_mut() {
        children.sort_by_key(|t| t.created_at_ms);
    }

    let mut ordered = Vec::new();
    fn visit(thread: Thread, by_parent: &HashMap<String, Vec<Thread>>, ordered: &mut Vec<Thread>) {
        let id = thread.id.clone();
        ordered.push(thread);
        if let Some(children) = by_parent.get(&id) {
            for child in children {
                visit(child.clone(), by_parent, ordered);
            }
        }
    }
    for root in roots {
        visit(root, &by_parent, &mut ordered);
    }
    ordered
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn local_time_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn atty_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Mirrors `shutil.get_terminal_size((120, 30))`: prefer the `COLUMNS`/
/// `LINES` environment variables, then the actual stdout tty size, then the
/// (120, 30) fallback -- deliberately not falling back to `/dev/tty` (as
/// crossterm's `size()` alone does) when stdout itself isn't a terminal, so
/// piped/redirected output sizes the same way the Python version does.
pub fn terminal_size() -> (u16, u16) {
    let env_dim = |name: &str| -> Option<u16> {
        std::env::var(name).ok()?.trim().parse::<u16>().ok().filter(|v| *v > 0)
    };
    let columns = env_dim("COLUMNS");
    let lines = env_dim("LINES");
    if let (Some(c), Some(l)) = (columns, lines) {
        return (c, l);
    }
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        if let Ok((c, l)) = crossterm::terminal::size() {
            return (columns.unwrap_or(c), lines.unwrap_or(l));
        }
    }
    (columns.unwrap_or(120), lines.unwrap_or(30))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, origin: &str) -> Thread {
        Thread {
            id: id.to_string(),
            pid: 1,
            rollout_path: String::new(),
            source: String::new(),
            cwd: String::new(),
            title: String::new(),
            session_name: None,
            model: None,
            tokens_used: None,
            nickname: None,
            role: None,
            agent_path: None,
            created_at_ms: 0,
            reasoning_effort: None,
            parent_id: None,
            kind: "main".to_string(),
            depth: 0,
            origin: origin.to_string(),
        }
    }

    fn weekly_state(pool: Option<&str>, used_percent: f64, sample_at: f64) -> AgentRowState {
        AgentRowState {
            phase: "run".to_string(),
            phase_at: 0.0,
            activity: String::new(),
            weekly_used_percent: Some(used_percent),
            weekly_sample_at: Some(sample_at),
            weekly_limit_id: pool.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn latest_weekly_state_does_not_let_a_fresh_unrelated_pool_mask_the_real_one() {
        // A model-specific preview pool that just started (0% used, very
        // fresh sample) must not hide the standard plan pool that is
        // actually nearly exhausted and less fresh.
        let t1 = thread("t1", "codex");
        let t2 = thread("t2", "codex");
        let mut states = HashMap::new();
        states.insert("t1".to_string(), weekly_state(Some("codex"), 76.0, 100.0));
        states.insert("t2".to_string(), weekly_state(Some("codex_bengalfox"), 0.0, 200.0));
        let visible: Vec<&Thread> = vec![&t1, &t2];

        let chosen = Dashboard::latest_weekly_state(&visible, &states, "codex").unwrap();
        assert_eq!(chosen.weekly_used_percent, Some(76.0));
    }

    #[test]
    fn latest_weekly_state_takes_freshest_sample_within_a_single_pool() {
        let t1 = thread("t1", "codex");
        let t2 = thread("t2", "codex");
        let mut states = HashMap::new();
        states.insert("t1".to_string(), weekly_state(Some("codex"), 10.0, 100.0));
        states.insert("t2".to_string(), weekly_state(Some("codex"), 20.0, 200.0));
        let visible: Vec<&Thread> = vec![&t1, &t2];

        let chosen = Dashboard::latest_weekly_state(&visible, &states, "codex").unwrap();
        assert_eq!(chosen.weekly_used_percent, Some(20.0));
    }

    #[test]
    fn latest_weekly_state_returns_none_without_any_sample() {
        let t1 = thread("t1", "codex");
        let mut states = HashMap::new();
        states.insert("t1".to_string(), AgentRowState { phase: "run".to_string(), ..Default::default() });
        let visible: Vec<&Thread> = vec![&t1];
        assert!(Dashboard::latest_weekly_state(&visible, &states, "codex").is_none());
    }

    fn parse_args(extra: &[&str]) -> crate::cli::Args {
        use clap::Parser;
        let mut argv = vec!["agents-live", "--once"];
        argv.extend_from_slice(extra);
        crate::cli::Args::parse_from(argv)
    }

    /// Drops SGR escape sequences so a painted line can be measured by the
    /// same display width rules as the plain text it was built from.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// A thread carrying the fields the layout tests care about.
    fn row_thread(id: &str, title: &str) -> Thread {
        Thread {
            model: Some("gpt-5.6-sol".to_string()),
            reasoning_effort: Some("high".to_string()),
            tokens_used: Some(1),
            cwd: "/tmp/project".to_string(),
            title: title.to_string(),
            created_at_ms: 1,
            ..thread(id, "codex")
        }
    }

    fn row_state(phase: &str) -> AgentRowState {
        AgentRowState {
            phase: phase.to_string(),
            phase_at: now_secs() - 57.0,
            activity: "thinking".to_string(),
            total_tokens: Some(309_500),
            weekly_used_percent: Some(14.0),
            weekly_sample_at: Some(150.0),
            ..Default::default()
        }
    }

    fn states_of(pairs: &[(&Thread, &str)]) -> HashMap<String, AgentRowState> {
        let mut states = HashMap::new();
        for (t, phase) in pairs {
            states.insert(t.id.clone(), row_state(phase));
        }
        states
    }


    #[test]
    fn cost_is_flagged_when_a_model_missing_from_the_pricing_table_carries_real_usage() {
        // claude-opus-5-5 was once unpriced and ~80% of usage vanished
        // from the header with nothing on screen to hint at it.
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("projects").join("-Users-x-proj");
        std::fs::create_dir_all(&project).unwrap();
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let line = |request_id: &str, model: &str, input: i64| {
            format!(
                r#"{{"type":"assistant","requestId":"{}","timestamp":"{}","message":{{"role":"assistant","model":"{}","usage":{{"input_tokens":{},"output_tokens":0}}}}}}"#,
                request_id, now, model, input
            )
        };
        let transcript = project.join("s1.jsonl");
        let home_arg = home.path().to_string_lossy().to_string();
        let visible: Vec<&Thread> = Vec::new();
        let states = HashMap::new();

        std::fs::write(&transcript, format!("{}\n", line("r1", "claude-sonnet-5", 1_000_000))).unwrap();
        let dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude-home", &home_arg]));
        dashboard.claude_usage_tracker.refresh(false);
        let summary = dashboard.source_summary_line("claude", 120, &visible, &states);
        assert!(summary.contains("0.0B $2 "), "{}", summary);
        assert!(!summary.contains("+?"), "{}", summary);

        std::fs::write(
            &transcript,
            format!("{}\n{}\n", line("r1", "claude-sonnet-5", 1_000_000), line("r2", "claude-next", 1_000_000)),
        )
        .unwrap();
        let dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude-home", &home_arg]));
        dashboard.claude_usage_tracker.refresh(false);
        let summary = dashboard.source_summary_line("claude", 120, &visible, &states);
        assert!(summary.contains("0.0B $2+? "), "{}", summary);
    }

    #[test]
    fn five_hour_figure_follows_weekly_and_is_shown_for_codex_only_when_reported() {
        let t = thread("t1", "codex");
        let visible = vec![&t];
        let dashboard = Dashboard::new(parse_args(&["--color", "never"]));

        // Codex without a 5-hour window (the current state): no 5h at all
        let mut states = HashMap::new();
        states.insert("t1".to_string(), weekly_state(Some("codex"), 14.0, 100.0));
        let line = dashboard.source_summary_line("codex", 120, &visible, &states);
        assert!(line.starts_with("CODEX  Weekly 86%  --B $--"), "{}", line);
        assert!(!line.contains("5h"));

        // once imposed, it sits right after the weekly figure
        states.get_mut("t1").unwrap().five_hour_used_percent = Some(10.0);
        states.get_mut("t1").unwrap().five_hour_sample_at = Some(100.0);
        let line = dashboard.source_summary_line("codex", 120, &visible, &states);
        assert!(line.starts_with("CODEX  Weekly 86%  5h 90%  --B $--"), "{}", line);

        // a sample whose 5-hour window already reset reads as full
        states.get_mut("t1").unwrap().five_hour_resets_at = Some(1.0);
        let line = dashboard.source_summary_line("codex", 120, &visible, &states);
        assert!(line.starts_with("CODEX  Weekly 86%  5h 100%  --B $--"), "{}", line);

        // Claude always shows it, as "--" until the statusline cache has one
        let c = thread("c1", "claude");
        let visible = vec![&c];
        let mut states = HashMap::new();
        states.insert("c1".to_string(), AgentRowState { phase: "run".to_string(), ..Default::default() });
        let line = dashboard.source_summary_line("claude", 120, &visible, &states);
        assert!(line.starts_with("CLAUDE  Weekly --  5h --  --B $--"), "{}", line);
        states.get_mut("c1").unwrap().five_hour_used_percent = Some(48.0);
        let line = dashboard.source_summary_line("claude", 120, &visible, &states);
        assert!(line.starts_with("CLAUDE  Weekly --  5h 52%  --B $--"), "{}", line);
    }

    #[test]
    fn weekly_below_5_percent_uses_red_critical_highlight_regardless_of_reverse_flag() {
        let t1 = thread("t1", "codex");
        let mut states = HashMap::new();
        // weekly_used_percent=96.0 -> 4% remaining -> below the 5% critical
        // threshold.
        states.insert("t1".to_string(), weekly_state(None, 96.0, 100.0));
        let visible: Vec<&Thread> = vec![&t1];

        for extra in [
            &["--color", "always", "--claude", "off"][..],
            &["--color", "always", "--claude", "off", "--weekly-reverse"][..],
        ] {
            let dashboard = Dashboard::new(parse_args(extra));
            let line = dashboard.source_summary_line("codex", 120, &visible, &states);
            let critical = theme::theme("cool").get("weekly_critical").copied().unwrap();
            let header = theme::theme("cool").get("header").copied().unwrap();
            assert!(line.contains(&format!("{}4%{}", critical, header)));
            assert!(line.starts_with(header));
            assert!(line.ends_with(RESET));
            assert!(!line.contains(REVERSE));
        }

        // At exactly 5% remaining (not below it), no critical highlight.
        states.insert("t1".to_string(), weekly_state(None, 95.0, 100.0));
        let dashboard = Dashboard::new(parse_args(&["--color", "always", "--claude", "off"]));
        let line = dashboard.source_summary_line("codex", 120, &visible, &states);
        let critical = theme::theme("cool").get("weekly_critical").copied().unwrap();
        assert!(!line.contains(critical));

        // --color never never emits the highlight either.
        states.insert("t1".to_string(), weekly_state(None, 96.0, 100.0));
        let dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude", "off"]));
        let line = dashboard.source_summary_line("codex", 120, &visible, &states);
        assert!(!line.contains('\u{1b}'));
        assert!(line.contains("Weekly 4%"));
    }

    #[test]
    fn compact_groups_default_on_omits_blank_lines_between_pid_groups() {
        let t1 = Thread { pid: 1, created_at_ms: 1, ..thread("t1", "codex") };
        let t2 = Thread { pid: 2, created_at_ms: 2, ..thread("t2", "codex") };
        let threads = vec![t1, t2];
        let mut states = HashMap::new();
        for t in &threads {
            states.insert(t.id.clone(), AgentRowState { phase: "done".to_string(), ..Default::default() });
        }

        let default_dashboard = Dashboard::new(parse_args(&["--color", "never"]));
        assert_eq!(default_dashboard.args.compact_groups, "on");
        let default_output = default_dashboard.render(&threads, &states);
        assert!(!default_output.contains("\n\n"));

        let spaced_dashboard = Dashboard::new(parse_args(&["--color", "never", "--compact-groups", "off"]));
        let spaced_output = spaced_dashboard.render(&threads, &states);
        assert!(spaced_output.contains("\n\n"));
    }

    #[test]
    fn highlighted_model_name_uses_green_background_and_keeps_run_reverse_boundary() {
        let astra = Thread {
            model: Some("gpt-6-astra".to_string()),
            title: "astra-task".to_string(),
            ..thread("astra", "codex")
        };
        let sol = Thread {
            pid: 2,
            model: Some("gpt-5.6-sol".to_string()),
            title: "sol-task".to_string(),
            ..thread("sol", "codex")
        };
        let threads = vec![astra, sol];
        let mut states = HashMap::new();
        for t in &threads {
            states.insert(
                t.id.clone(),
                AgentRowState { phase: "run".to_string(), ..Default::default() },
            );
        }

        for name in ["cool", "codex", "cyberpunk"] {
            let dashboard =
                Dashboard::new(parse_args(&["--color", "always", "--theme", name, "--claude", "off"]));
            let output = dashboard.render(&threads, &states);
            let style = theme::theme(name).get("model_highlight").copied().unwrap();
            assert_eq!(style, "\x1b[1;30;48;5;46m");

            let row = output.lines().find(|l| l.contains("astra-task")).unwrap();
            assert!(row.contains(&format!("{}{}gpt-6-astra{}{}", NO_REVERSE, style, RESET, REVERSE)));

            // a model that is not on the list keeps the plain name styling
            let other = output.lines().find(|l| l.contains("sol-task")).unwrap();
            assert!(!other.contains(style));
        }

        // Not a run row: the highlight stays, the reverse boundary does not.
        for t in &threads {
            states.insert(
                t.id.clone(),
                AgentRowState { phase: "done".to_string(), ..Default::default() },
            );
        }
        let dashboard = Dashboard::new(parse_args(&["--color", "always", "--claude", "off"]));
        let output = dashboard.render(&threads, &states);
        let row = output.lines().find(|l| l.contains("astra-task")).unwrap();
        let style = theme::theme("cool").get("model_highlight").copied().unwrap();
        assert!(row.contains(&format!("{}gpt-6-astra{}", style, RESET)));
        assert!(!row.contains(NO_REVERSE));

        // --color never emits no escapes at all.
        let dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude", "off"]));
        let output = dashboard.render(&threads, &states);
        assert!(!output.contains('\u{1b}'));
    }

    // ---- ported from the retired Python suite ----

    #[test]
    fn usage_fields_are_ordered_and_wrap_to_a_continuation_row_when_narrow() {
        let t = row_thread("t", "\u{9577}\u{3044}\u{4f5c}\u{696d}\u{540d}");
        let states = states_of(&[(&t, "run")]);
        let dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude", "off"]));

        let wide = dashboard.render_sized(&[t.clone()], &states, 120, 30);
        let top = wide.lines().next().unwrap();
        assert!(!top.contains("Weekly"));
        assert!(!top.contains("running"));
        assert!(wide.contains("CODEX  Weekly 86%  --B $--   1 running"));
        assert_eq!(1, wide.matches("Weekly 86%").count());
        let row = wide.lines().find(|l| l.contains("RUN")).unwrap();
        assert!(!row.contains("Weekly 86%"));
        assert!(row.find("gpt-5.6-sol").unwrap() < row.find("high").unwrap());
        assert!(row.find("high").unwrap() < row.find("309.5K tok").unwrap());

        let narrow = dashboard.render_sized(&[t.clone()], &states, 72, 30);
        let lines: Vec<&str> = narrow.lines().collect();
        let idx = lines.iter().position(|l| l.contains("RUN")).unwrap();
        assert!(!lines[idx].contains("gpt-5.6-sol"));
        assert!(lines[idx + 1].contains("gpt-5.6-sol high 309.5K tok"));
    }

    #[test]
    fn tokens_option_defaults_on_and_removes_the_token_column_when_off() {
        let t = row_thread("t", "tokens-toggle-task");
        let states = states_of(&[(&t, "run")]);

        let on = Dashboard::new(parse_args(&["--color", "never", "--claude", "off"]));
        assert_eq!(on.args.tokens, "on");
        assert!(on.render_sized(&[t.clone()], &states, 120, 30).contains("309.5K tok"));

        let off = Dashboard::new(parse_args(&["--color", "never", "--claude", "off", "--tokens", "off"]));
        let output = off.render_sized(&[t.clone()], &states, 120, 30);
        assert!(output.contains("CODEX  Weekly 86%  --B $--"));
        assert!(!output.contains("309.5K tok"));
        let row = output.lines().find(|l| l.contains("RUN")).unwrap();
        assert!(row.find("gpt-5.6-sol").unwrap() < row.find("high").unwrap());
        assert!(row.find("high").unwrap() < row.find("tokens-toggle-task").unwrap());

        // the reclaimed width must not leave an empty token slot on the
        // narrow continuation row either
        let narrow = off.render_sized(&[t.clone()], &states, 72, 30);
        let lines: Vec<&str> = narrow.lines().collect();
        let idx = lines.iter().position(|l| l.contains("RUN")).unwrap();
        assert!(lines[idx].contains("tokens-toggle-task"));
        assert!(lines[idx + 1].contains("gpt-5.6-sol high"));
        assert!(!lines[idx + 1].contains("tok"));
    }

    #[test]
    fn no_wrap_suppresses_only_the_narrow_continuation_row() {
        let t = row_thread("t", "no-wrap-task");
        let states = states_of(&[(&t, "run")]);
        let wrapping = Dashboard::new(parse_args(&["--color", "never", "--claude", "off"]));
        let no_wrap = Dashboard::new(parse_args(&["--color", "never", "--claude", "off", "--no-wrap"]));

        let narrow_wrapped = wrapping.render_sized(&[t.clone()], &states, 72, 30);
        assert!(narrow_wrapped.contains("\u{21B3}"));
        assert!(narrow_wrapped.contains("gpt-5.6-sol"));

        let narrow_plain = no_wrap.render_sized(&[t.clone()], &states, 72, 30);
        assert!(!narrow_plain.contains("\u{21B3}"));
        assert!(narrow_plain.contains("no-wrap-task"));

        // at a comfortable width --no-wrap changes nothing: everything still
        // fits on the one row
        let wide = no_wrap.render_sized(&[t.clone()], &states, 120, 30);
        let row = wide.lines().find(|l| l.contains("RUN")).unwrap();
        assert!(row.contains("gpt-5.6-sol"));
        assert!(row.contains("309.5K tok"));
        assert!(row.contains("no-wrap-task"));
    }

    #[test]
    fn run_rows_are_reverse_video_across_the_row_and_its_continuation() {
        let t = row_thread("t", "run-reverse-task");
        let states = states_of(&[(&t, "run")]);
        let dashboard = Dashboard::new(parse_args(&["--color", "always", "--claude", "off"]));

        let wide = dashboard.render_sized(&[t.clone()], &states, 120, 30);
        let row = wide.lines().find(|l| l.contains("RUN")).unwrap();
        assert!(row.starts_with(REVERSE));
        assert!(row.ends_with(RESET));
        assert!(display_width(&strip_ansi(row)) >= 118);

        let narrow = dashboard.render_sized(&[t.clone()], &states, 72, 30);
        let detail = narrow.lines().find(|l| l.contains("\u{21B3}")).unwrap();
        assert!(detail.starts_with(REVERSE));

        // a row that is not running is never reversed
        // a main thread that finished shows IDLE and is never reversed
        let idle_states = states_of(&[(&t, "done")]);
        let idle = dashboard.render_sized(&[t.clone()], &idle_states, 120, 30);
        let idle_row = idle.lines().find(|l| l.contains("IDLE")).unwrap();
        assert!(!idle_row.contains(REVERSE));
    }

    #[test]
    fn fast_badge_glyphs_are_not_reversed_inside_run_rows() {
        let t = row_thread("t", "fast-task");
        let mut states = HashMap::new();
        states.insert(
            t.id.clone(),
            AgentRowState { service_tier: Some("priority".to_string()), ..row_state("run") },
        );
        let dashboard = Dashboard::new(parse_args(&["--color", "always", "--claude", "off"]));
        let output = dashboard.render_sized(&[t.clone()], &states, 120, 30);
        let row = output.lines().find(|l| l.contains("fast-task")).unwrap();
        let header = theme::theme("cool").get("header").copied().unwrap();
        let badge = format!("{}{}\u{26A1}{}{}", NO_REVERSE, header, RESET, REVERSE);
        assert!(row.contains(&badge));
        assert!(row.find(&badge).unwrap() < row.find("gpt-5.6-sol").unwrap());

        states.insert(
            t.id.clone(),
            AgentRowState { service_tier: Some("ultrafast".to_string()), ..row_state("run") },
        );
        let ultra = dashboard.render_sized(&[t.clone()], &states, 120, 30);
        assert!(ultra.contains(&format!("{}{}\u{26A1}\u{26A1}{}{}", NO_REVERSE, header, RESET, REVERSE)));

        // a normal tier shows no badge at all
        states.insert(t.id.clone(), row_state("run"));
        let plain = dashboard.render_sized(&[t.clone()], &states, 120, 30);
        assert!(!plain.contains("\u{26A1}"));
    }

    #[test]
    fn high_efforts_use_distinct_colors_and_preserve_the_run_reverse_boundary() {
        for (effort, expected) in [
            ("high", "\x1b[1;30;48;5;117m"),
            ("xhigh", "\x1b[1;30;48;5;226m"),
            ("ultra", "\x1b[1;97;48;5;196m"),
        ] {
            let t = Thread {
                reasoning_effort: Some(effort.to_string()),
                ..row_thread("t", &format!("{}-effort-task", effort))
            };
            let states = states_of(&[(&t, "run")]);

            for name in theme::THEME_NAMES {
                let dashboard = Dashboard::new(parse_args(&[
                    "--color", "always", "--theme", name, "--claude", "off",
                ]));
                let output = dashboard.render_sized(&[t.clone()], &states, 120, 30);
                let row = output.lines().find(|l| l.contains("-effort-task")).unwrap();
                let style_key = theme::effort_style(effort).unwrap();
                assert_eq!(expected, theme::theme(name).get(style_key).copied().unwrap());
                assert!(row.contains(&format!("{}{}{}{}{}", NO_REVERSE, expected, effort, RESET, REVERSE)));
            }

            // the same boundary applies on the narrow continuation row
            let dashboard = Dashboard::new(parse_args(&["--color", "always", "--claude", "off"]));
            let narrow = dashboard.render_sized(&[t.clone()], &states, 72, 30);
            let detail = narrow.lines().find(|l| l.contains("\u{21B3}")).unwrap();
            assert!(detail.contains(&format!("{}{}{}{}{}", NO_REVERSE, expected, effort, RESET, REVERSE)));

            // "medium" and friends are not highlighted at all
            assert!(theme::effort_style("medium").is_none());
        }
    }

    #[test]
    fn all_idle_banner_appears_only_when_every_visible_thread_is_finished() {
        let main = row_thread("main", "main");
        let child = Thread {
            parent_id: Some("main".to_string()),
            kind: "spawn".to_string(),
            depth: 1,
            nickname: Some("Gauss".to_string()),
            agent_path: Some("/root/child".to_string()),
            created_at_ms: 2,
            ..row_thread("child", "child")
        };
        let threads = vec![main.clone(), child.clone()];
        let dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude", "off"]));
        assert_eq!(dashboard.args.all_idle_banner, "on");
        assert_eq!(dashboard.args.all_idle_bgcolor, "blue");

        let mut states = states_of(&[(&main, "done"), (&child, "done")]);
        let idle = dashboard.render_sized(&threads, &states, 120, 30);
        let header = idle.lines().next().unwrap();
        assert!(header.starts_with("\u{2588}\u{2588}\u{2588}\u{2588}  ALL CODEX SESSIONS IDLE  \u{2588}\u{2588}\u{2588}\u{2588}"));
        assert_eq!(119, display_width(header));

        // only genuinely active work suppresses the banner
        for phase in ["run", "start"] {
            states.insert(child.id.clone(), row_state(phase));
            let active = dashboard.render_sized(&threads, &states, 120, 30);
            let header = active.lines().next().unwrap();
            assert!(header.starts_with("CODEX AGENTS LIVE"), "{}", phase);
            assert!(!header.contains("ALL CODEX SESSIONS IDLE"), "{}", phase);
        }

        // a stopped thread is settled, not active, so the banner still shows
        // -- including when every visible thread is stopped
        states.insert(child.id.clone(), row_state("stop"));
        let stopped = dashboard.render_sized(&threads, &states, 120, 30);
        assert!(stopped.lines().next().unwrap().contains("ALL CODEX SESSIONS IDLE"));
        states.insert(main.id.clone(), row_state("stop"));
        let all_stopped = dashboard.render_sized(&threads, &states, 120, 30);
        assert!(all_stopped.lines().next().unwrap().contains("ALL CODEX SESSIONS IDLE"));
        states.insert(main.id.clone(), row_state("done"));

        // but a stopped thread alongside a running one does not
        states.insert(child.id.clone(), row_state("run"));
        let mixed = dashboard.render_sized(&threads, &states, 120, 30);
        assert!(!mixed.lines().next().unwrap().contains("ALL CODEX SESSIONS IDLE"));

        // an empty screen is not "all idle" either
        let empty = dashboard.render_sized(&[], &HashMap::new(), 120, 30);
        assert!(empty.lines().next().unwrap().starts_with("CODEX AGENTS LIVE"));

        states.insert(child.id.clone(), row_state("done"));
        let off = Dashboard::new(parse_args(&[
            "--color", "never", "--claude", "off", "--all-idle-banner", "off",
        ]));
        let header = off.render_sized(&threads, &states, 120, 30);
        assert!(header.lines().next().unwrap().starts_with("CODEX AGENTS LIVE"));
        assert!(!header.contains("ALL CODEX SESSIONS IDLE"));
    }

    #[test]
    fn all_idle_banner_defaults_to_blue_and_honours_every_background_color() {
        let t = row_thread("main", "main");
        let states = states_of(&[(&t, "done")]);

        let expected = theme::all_idle_bg_style("blue");
        for name in theme::THEME_NAMES {
            let dashboard = Dashboard::new(parse_args(&[
                "--color", "always", "--theme", name, "--claude", "off",
            ]));
            let output = dashboard.render_sized(&[t.clone()], &states, 120, 30);
            let header = output.lines().next().unwrap();
            assert!(header.starts_with(expected), "{}", name);
            assert!(header.contains("ALL CODEX SESSIONS IDLE"));
            assert!(header.ends_with(RESET));
        }

        for color in theme::ALL_IDLE_BG_COLOR_NAMES {
            let dashboard = Dashboard::new(parse_args(&[
                "--color", "always", "--claude", "off", "--all-idle-bgcolor", color,
            ]));
            let output = dashboard.render_sized(&[t.clone()], &states, 120, 30);
            let header = output.lines().next().unwrap();
            assert!(header.starts_with(theme::all_idle_bg_style(color)), "{}", color);
            assert!(header.contains("ALL CODEX SESSIONS IDLE"));
        }
    }

    #[test]
    fn all_idle_red_screen_fills_the_viewport_only_when_enabled_and_colored() {
        let t = row_thread("main", "main-task");
        let states = states_of(&[(&t, "done")]);
        let style = theme::all_idle_bg_style("magenta");

        let filled = Dashboard::new(parse_args(&[
            "--color", "always", "--claude", "off",
            "--all-idle-red-screen", "--all-idle-bgcolor", "magenta",
        ]));
        let output = filled.render_sized(&[t.clone()], &states, 80, 10);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(10, lines.len());
        for line in &lines {
            assert!(line.starts_with(style));
            assert!(line.ends_with(RESET));
            assert_eq!(79, display_width(&strip_ansi(line)));
        }

        // without the flag the viewport is left alone
        let plain = Dashboard::new(parse_args(&[
            "--color", "always", "--claude", "off", "--all-idle-bgcolor", "magenta",
        ]));
        let output = plain.render_sized(&[t.clone()], &states, 80, 10);
        assert!(output.lines().count() < 10);

        // and --color never keeps the plain text form
        let uncolored = Dashboard::new(parse_args(&[
            "--color", "never", "--claude", "off",
            "--all-idle-red-screen", "--all-idle-bgcolor", "magenta",
        ]));
        let output = uncolored.render_sized(&[t.clone()], &states, 80, 10);
        assert!(!output.contains('\u{1b}'));
        assert!(output.contains("ALL CODEX SESSIONS IDLE"));
    }

    #[test]
    fn hide_done_children_hides_only_finished_subagents() {
        let main = row_thread("main", "main-task");
        let done_child = Thread {
            parent_id: Some("main".to_string()),
            kind: "spawn".to_string(),
            depth: 1,
            created_at_ms: 2,
            ..row_thread("done-child", "done-child-task")
        };
        let run_child = Thread {
            parent_id: Some("main".to_string()),
            kind: "spawn".to_string(),
            depth: 1,
            created_at_ms: 3,
            ..row_thread("run-child", "run-child-task")
        };
        let threads = vec![main.clone(), done_child.clone(), run_child.clone()];
        let states = states_of(&[(&main, "done"), (&done_child, "done"), (&run_child, "run")]);

        for flag in ["--hide-done", "--hide-done-children"] {
            let dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude", "off", flag]));
            assert!(dashboard.args.hide_done);
            let output = dashboard.render_sized(&threads, &states, 120, 30);
            assert!(output.contains("main-task"));
            assert!(!output.contains("done-child-task"));
            assert!(output.contains("run-child-task"));
        }
    }

    #[test]
    fn render_keeps_the_header_and_trims_the_tail_to_the_terminal_height() {
        let t = Thread {
            model: Some("gpt-5.6-terra".to_string()),
            reasoning_effort: Some("medium".to_string()),
            ..row_thread("t", "\u{672B}\u{5C3E}\u{5074}\u{306B}\u{3042}\u{308B}\u{4F5C}\u{696D}\u{540D}")
        };
        let states = states_of(&[(&t, "run")]);
        let dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude", "off"]));

        let output = dashboard.render_sized(&[t.clone()], &states, 120, 5);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(5, lines.len());
        assert!(lines[0].starts_with("CODEX AGENTS LIVE"));
        assert!(lines[2].contains("CODEX"));
        assert!(lines[3].contains("more rows omitted"));
        assert_eq!("q / Esc / Ctrl-C to stop", lines[4]);
        assert!(!output.contains("PID 1"));

        // with only two rows the header and the omission notice win
        let two = dashboard.render_sized(&[t.clone()], &states, 120, 2);
        let lines: Vec<&str> = two.lines().collect();
        assert_eq!(2, lines.len());
        assert!(lines[0].starts_with("CODEX AGENTS LIVE"));
        assert!(lines[1].contains("more rows omitted"));
    }

    #[test]
    fn claude_children_end_with_the_process_and_bash_ends_when_idle() {
        use crate::claude::transcript::AgentInfo;
        let bash = AgentInfo { description: None, subagent_type: Some("bash".to_string()), started_at: 100.0, done_at: None };
        let agent = AgentInfo { subagent_type: Some("general-purpose".to_string()), ..bash.clone() };
        // still running while the session says busy/shell
        assert_eq!(claude_child_done_at(&bash, Some("shell"), 150.0, Some(50.0)), None);
        assert_eq!(claude_child_done_at(&bash, Some("busy"), 150.0, Some(50.0)), None);
        // idle means no background Bash is alive; agents are not covered by that
        assert_eq!(claude_child_done_at(&bash, Some("idle"), 150.0, Some(50.0)), Some(150.0));
        assert_eq!(claude_child_done_at(&agent, Some("idle"), 150.0, Some(50.0)), None);
        // ...but it says nothing about a bash started after that idle status
        assert_eq!(claude_child_done_at(&bash, Some("idle"), 90.0, Some(50.0)), None);
        // started before the current process: gone, whatever its kind
        assert_eq!(claude_child_done_at(&agent, Some("busy"), 150.0, Some(120.0)), Some(120.0));
        // its own notification wins
        let done = AgentInfo { done_at: Some(130.0), ..bash.clone() };
        assert_eq!(claude_child_done_at(&done, Some("busy"), 150.0, Some(50.0)), Some(130.0));
    }

    #[test]
    fn a_bash_child_closed_by_an_idle_status_stays_done_when_the_session_is_busy_again() {
        // The real case: a bash child whose notification never came showed
        // DONE while its session was idle, then RUN again once it went busy.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::create_dir_all(home.join("projects/p")).unwrap();
        let launch = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "sh-1", "name": "Bash",
                "input": {"command": "cargo test", "description": "run tests", "run_in_background": true}}]}
        });
        std::fs::write(home.join("projects/p/sess-1.jsonl"), format!("{}\n", launch)).unwrap();
        let pid = std::process::id();
        let write_session = |status: &str, status_at_ms: i64| {
            std::fs::write(
                home.join(format!("sessions/{}.json", pid)),
                serde_json::json!({"pid": pid, "sessionId": "sess-1", "cwd": "/tmp/p", "status": status,
                       "statusUpdatedAt": status_at_ms, "startedAt": 1_767_222_000_000_i64})
                    .to_string(),
            )
            .unwrap();
        };
        let home_arg = home.to_str().unwrap().to_string();
        let mut dashboard = Dashboard::new(parse_args(&["--codex", "off", "--claude-home", &home_arg]));
        let child_phase = |dashboard: &mut Dashboard| dashboard.snapshot_claude().1["sess-1:sh-1"].phase.clone();

        write_session("shell", 1_767_225_660_000);
        assert_eq!(child_phase(&mut dashboard), "run");
        write_session("idle", 1_767_226_200_000);
        assert_eq!(child_phase(&mut dashboard), "done");
        write_session("busy", 1_767_226_800_000);
        assert_eq!(child_phase(&mut dashboard), "done");
    }

    #[test]
    fn only_claude_busy_status_counts_as_running() {
        assert!(claude_status_is_running(Some("busy")));
        assert!(!claude_status_is_running(Some("shell")));
        assert!(!claude_status_is_running(Some("idle")));
        assert!(!claude_status_is_running(Some("waiting")));
        assert!(!claude_status_is_running(None));
    }

    #[test]
    fn header_and_banner_titles_follow_the_codex_claude_toggles() {
        let both = Dashboard::new(parse_args(&["--codex", "on", "--claude", "on"]));
        let codex_only = Dashboard::new(parse_args(&["--claude", "off"]));
        let claude_only = Dashboard::new(parse_args(&["--codex", "off"]));

        assert_eq!("AGENTS LIVE", both.header_title());
        assert_eq!("CODEX AGENTS LIVE", codex_only.header_title());
        assert_eq!("CLAUDE AGENTS LIVE", claude_only.header_title());

        assert_eq!("ALL AGENTS IDLE", both.all_idle_banner_title());
        assert_eq!("ALL CODEX SESSIONS IDLE", codex_only.all_idle_banner_title());
        assert_eq!("ALL CLAUDE SESSIONS IDLE", claude_only.all_idle_banner_title());
    }

    #[test]
    fn running_subagents_and_processes_are_counted_per_source() {
        let codex_main_1 = Thread { pid: 10, created_at_ms: 10, ..thread("codex-main-1", "codex") };
        let codex_main_2 = Thread { pid: 11, created_at_ms: 11, ..thread("codex-main-2", "codex") };
        let codex_child = Thread {
            pid: 10,
            created_at_ms: 12,
            kind: "spawn".to_string(),
            parent_id: Some("codex-main-1".to_string()),
            depth: 1,
            ..thread("codex-child", "codex")
        };
        let claude_main = Thread { pid: 20, created_at_ms: 20, ..thread("claude-main", "claude") };
        let threads = vec![
            codex_main_1.clone(),
            codex_main_2.clone(),
            codex_child.clone(),
            claude_main.clone(),
        ];
        let mut states = HashMap::new();
        for (t, phase) in [
            (&codex_main_1, "run"),
            (&codex_main_2, "done"),
            (&codex_child, "done"),
            (&claude_main, "run"),
        ] {
            states.insert(
                t.id.clone(),
                AgentRowState { phase: phase.to_string(), ..Default::default() },
            );
        }
        let visible: Vec<&Thread> = threads.iter().collect();

        assert_eq!((1, 1, 2), Dashboard::source_counts(&visible, &states, "codex"));
        assert_eq!((1, 0, 1), Dashboard::source_counts(&visible, &states, "claude"));

        let dashboard = Dashboard::new(parse_args(&["--color", "never"]));
        let output = dashboard.render_sized(&threads, &states, 120, 40);
        assert!(output.contains("CODEX  Weekly --  --B $--   1 running \u{00B7} 1 subagents \u{00B7} 2 processes"));
        assert!(output.contains("CLAUDE  Weekly --  5h --  --B $--   1 running \u{00B7} 0 subagents \u{00B7} 1 processes"));
        assert!(!output.lines().next().unwrap().contains("running"));
    }

    #[test]
    fn app_server_daemon_pids_are_recognized_from_ps_output() {
        let ps = "1001 /Users/me/.codex/packages/app-server-daemon/releases/0.157.1/bin/codex app-server --listen unix:// --managed-daemon
1002 codex resume
  701 /Applications/My Tools/bin/codex app-server --listen unix://
1003 codex";
        let daemons = parse_app_server_pids(ps);
        assert_eq!(daemons, [1001, 701].into_iter().collect());
    }

    #[test]
    fn sessions_held_by_the_app_server_daemon_get_their_own_blocks() {
        // Codex 0.157+: one daemon pid holds every session's threads.
        let mut a = row_thread("a-main", "Session A");
        let mut a_child = row_thread("a-child", "child_task_a");
        let mut b = row_thread("b-main", "Session B");
        let mut b_child = row_thread("b-child", "child_task_b");
        let mut b_grandchild = row_thread("b-grandchild", "grandchild_task_b");
        for t in [&mut a, &mut a_child, &mut b, &mut b_child, &mut b_grandchild] {
            t.pid = 1001;
        }
        b.created_at_ms = a.created_at_ms + 1000;
        a_child.kind = "spawn".to_string();
        a_child.parent_id = Some("a-main".to_string());
        a_child.depth = 1;
        b_child.kind = "spawn".to_string();
        b_child.parent_id = Some("b-main".to_string());
        b_child.depth = 1;
        b_grandchild.kind = "spawn".to_string();
        b_grandchild.parent_id = Some("b-child".to_string());
        b_grandchild.depth = 2;
        let threads = vec![a, a_child, b, b_child, b_grandchild];
        let mut states = HashMap::new();
        for t in &threads {
            states.insert(t.id.clone(), row_state("run"));
        }
        // b-child finished and is hidden: its running grandchild must still
        // land in b's block, not become a block of its own
        states.insert("b-child".to_string(), row_state("done"));

        let mut dashboard = Dashboard::new(parse_args(&["--color", "never", "--claude", "off", "--hide-done"]));
        dashboard.codex_daemon_pids.insert(1001, true);
        let output = dashboard.render_sized(&threads, &states, 140, 40);
        let headers: Vec<&str> = output.lines().filter(|l| l.starts_with("\u{256D}")).collect();
        assert_eq!(2, headers.len(), "{}", output);
        assert!(headers[0].contains("app-server 1001") && headers[0].contains("a-main"), "{}", headers[0]);
        assert!(headers[1].contains("app-server 1001") && headers[1].contains("b-main"), "{}", headers[1]);
        assert!(headers[0].contains("2 running") && headers[1].contains("2 running"), "{}", output);
        let b_block = &output[output.find(headers[1]).unwrap()..];
        assert!(b_block.contains("grandchild_task_b") && !b_block.contains("child_task_a"), "{}", output);

        // an ordinary CLI process keeps the plain "PID" label
        dashboard.codex_daemon_pids.insert(1001, false);
        let output = dashboard.render_sized(&threads, &states, 140, 40);
        assert!(output.contains("PID 1001") && !output.contains("app-server"), "{}", output);
    }

    #[test]
    fn title_line_is_neutral_white_and_lists_resets_in_group_order() {
        let codex = row_thread("c", "codex-task");
        let mut claude = row_thread("k", "claude-task");
        claude.origin = "claude".to_string();
        let mut states = HashMap::new();
        states.insert(codex.id.clone(), row_state("run"));
        states.insert(claude.id.clone(), row_state("run"));
        let threads = [codex, claude];

        for name in ["cool", "codex", "cyberpunk"] {
            let dashboard = Dashboard::new(parse_args(&["--color", "always", "--theme", name]));
            let output = dashboard.render_sized(&threads, &states, 120, 30);
            let title = output.lines().next().unwrap();
            assert!(title.starts_with("\x1b[1;97mAGENTS LIVE"), "{}: {:?}", name, title);
            // the Codex header hue is still used for its summary line
            let header = theme::theme(name).get("header").copied().unwrap();
            assert!(!title.contains(header), "{}", name);
        }

        let codex_first = Dashboard::new(parse_args(&["--color", "never"]));
        let title = codex_first.render_sized(&threads, &states, 120, 30).lines().next().unwrap().to_string();
        assert!(title.find("Codex\u{1F504}").unwrap() < title.find("Claude\u{1F504}").unwrap(), "{}", title);

        let claude_first = Dashboard::new(parse_args(&["--color", "never", "--group-order", "claude-first"]));
        let title = claude_first.render_sized(&threads, &states, 120, 30).lines().next().unwrap().to_string();
        assert!(title.find("Claude\u{1F504}").unwrap() < title.find("Codex\u{1F504}").unwrap(), "{}", title);
    }

    #[test]
    fn group_order_lists_one_origins_groups_before_the_others() {
        let codex = Thread {
            pid: 1,
            created_at_ms: 100,
            cwd: "/tmp/zzz-codex-project".to_string(),
            ..thread("codex-main", "codex")
        };
        let claude = Thread {
            pid: 2,
            created_at_ms: 1,
            cwd: "/tmp/aaa-claude-project".to_string(),
            ..thread("claude-main", "claude")
        };
        let threads = vec![codex.clone(), claude.clone()];
        let states = states_of(&[(&codex, "done"), (&claude, "done")]);

        let default_dashboard = Dashboard::new(parse_args(&["--color", "never"]));
        assert_eq!("codex-first", default_dashboard.args.group_order);
        let output = default_dashboard.render_sized(&threads, &states, 120, 40);
        assert!(output.find("CODEX  Weekly").unwrap() < output.find("CLAUDE  Weekly").unwrap());

        let claude_first = Dashboard::new(parse_args(&["--color", "never", "--group-order", "claude-first"]));
        let output = claude_first.render_sized(&threads, &states, 120, 40);
        assert!(output.find("CLAUDE  Weekly").unwrap() < output.find("CODEX  Weekly").unwrap());

        // "mixed" was removed and must be rejected outright
        use clap::Parser;
        assert!(crate::cli::Args::try_parse_from([
            "agents-live", "--once", "--group-order", "mixed"
        ])
        .is_err());
    }

    #[test]
    fn group_sort_session_name_orders_groups_by_their_rename_name() {
        let later_a = Thread {
            pid: 1,
            created_at_ms: 200,
            session_name: Some("alpha".to_string()),
            title: "zzz".to_string(),
            ..thread("a", "codex")
        };
        let earlier_z = Thread {
            pid: 2,
            created_at_ms: 100,
            session_name: Some("zulu".to_string()),
            title: "aaa".to_string(),
            ..thread("z", "codex")
        };
        let threads = vec![later_a.clone(), earlier_z.clone()];
        let states = states_of(&[(&later_a, "done"), (&earlier_z, "done")]);

        let by_created = Dashboard::new(parse_args(&["--color", "never", "--claude", "off"]));
        assert_eq!("created", by_created.args.group_sort);
        let output = by_created.render_sized(&threads, &states, 120, 40);
        assert!(output.find("PID 2").unwrap() < output.find("PID 1").unwrap());

        let by_name = Dashboard::new(parse_args(&[
            "--color", "never", "--claude", "off", "--group-sort", "session-name",
        ]));
        let output = by_name.render_sized(&threads, &states, 120, 40);
        assert!(output.find("PID 1").unwrap() < output.find("PID 2").unwrap());
    }

    #[test]
    fn weekly_reverse_wraps_only_the_weekly_figure_and_never_the_green_state() {
        let t = thread("t", "codex");
        let mut states = HashMap::new();
        states.insert("t".to_string(), weekly_state(None, 14.0, 100.0));
        let visible: Vec<&Thread> = vec![&t];
        let header = theme::theme("cool").get("header").copied().unwrap();

        let plain = Dashboard::new(parse_args(&["--color", "always", "--claude", "off"]));
        let line = plain.source_summary_line("codex", 120, &visible, &states);
        assert!(!line.contains(REVERSE));

        let reversed = Dashboard::new(parse_args(&["--color", "always", "--claude", "off", "--weekly-reverse"]));
        let line = reversed.source_summary_line("codex", 120, &visible, &states);
        assert!(line.contains(&format!("{}86%{}", REVERSE, NO_REVERSE)));
        // the label itself stays outside the reversed span
        assert!(line.contains(&format!("Weekly {}86%", REVERSE)));

        // at >=95% remaining the green band wins and no reverse is emitted
        states.insert("t".to_string(), weekly_state(None, 3.0, 100.0));
        let line = reversed.source_summary_line("codex", 120, &visible, &states);
        let green = theme::theme("cool").get("weekly_high").copied().unwrap();
        assert!(line.starts_with(green));
        assert!(!line.contains(REVERSE));
        assert!(!line.contains(NO_REVERSE));

        // --color never emits nothing either way
        let never = Dashboard::new(parse_args(&["--color", "never", "--claude", "off", "--weekly-reverse"]));
        let line = never.source_summary_line("codex", 120, &visible, &states);
        assert!(!line.contains('\u{1b}'));
        let _ = header;
    }


}
