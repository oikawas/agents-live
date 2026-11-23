//! Live Antigravity CLI conversations. A running `antigravity` process holds
//! `presence/<conversation id>.lock` open for each conversation it has open
//! (the same scheme as Codex's thread-writer locks), and the summaries
//! database says whether each one is working.

use crate::antigravity::db::{load_conversations, Conversation, ConversationLog};
use crate::antigravity::quota::latest_quota;
use crate::codex::lock::held_lock_ids;
use crate::model::{AgentRowState, Thread};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const ORIGIN: &str = "antigravity";

/// RUNNING and BUSY are a turn in progress; CANCELING is still winding one
/// down. IDLE (and UNSPECIFIED, for a conversation never run) is not.
pub fn status_is_running(status: &str) -> bool {
    matches!(
        status,
        "CASCADE_RUN_STATUS_RUNNING" | "CASCADE_RUN_STATUS_BUSY" | "CASCADE_RUN_STATUS_CANCELING"
    )
}

/// The held conversations plus every conversation descending from one (an
/// agent it started), each with the pid of the process holding its root and
/// its depth below the nearest shown ancestor.
fn shown_conversations(
    held: &HashMap<String, i64>,
    conversations: &HashMap<String, Conversation>,
) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = held.iter().map(|(id, pid)| (id.clone(), *pid)).collect();
    for id in conversations.keys() {
        if held.contains_key(id) {
            continue;
        }
        let mut current = id.as_str();
        for _ in 0..32 {
            let parent = match conversations.get(current).and_then(|c| c.parent_id.as_deref()) {
                Some(p) => p,
                None => break,
            };
            if let Some(pid) = held.get(parent) {
                out.push((id.clone(), *pid));
                break;
            }
            current = parent;
        }
    }
    out.sort();
    out
}

pub fn snapshot(
    home: &Path,
    logs: &mut HashMap<String, ConversationLog>,
) -> (Vec<Thread>, HashMap<String, AgentRowState>, Option<String>) {
    let (held, lock_error) = held_lock_ids(&home.join("presence"), "antigravity", "Antigravity");
    if held.is_empty() {
        logs.clear();
        return (Vec::new(), HashMap::new(), lock_error);
    }
    let (conversations, db_error) = match load_conversations(home) {
        Ok(c) => (c, None),
        Err(err) => (HashMap::new(), Some(format!("antigravity summaries unavailable: {}", err))),
    };
    let shown = shown_conversations(&held, &conversations);
    // Account-wide limits, cached by the optional statusLine hook.
    let quota = latest_quota(home);
    let shown_ids: HashSet<&str> = shown.iter().map(|(id, _)| id.as_str()).collect();

    let mut threads = Vec::new();
    let mut states = HashMap::new();
    for (id, pid) in &shown {
        let conversation = conversations.get(id);
        let log = logs
            .entry(id.clone())
            .or_insert_with(|| ConversationLog::new(home.join("conversations").join(format!("{}.db", id)).to_string_lossy().to_string()));
        log.refresh();

        let parent_id = conversation
            .and_then(|c| c.parent_id.clone())
            .filter(|p| shown_ids.contains(p.as_str()));
        let mut depth = 0i64;
        let mut cursor = parent_id.clone();
        while let Some(p) = cursor {
            depth += 1;
            if depth > 32 {
                break;
            }
            cursor = conversations.get(&p).and_then(|c| c.parent_id.clone()).filter(|q| shown_ids.contains(q.as_str()));
        }
        let status = conversation.map(|c| c.status.as_str()).unwrap_or("");
        let running = status_is_running(status);
        let first_call_at = log.events.first().map(|e| e.at);
        let last_call_at = log.events.last().map(|e| e.at);
        let title = conversation
            .map(|c| if c.title.trim().is_empty() { c.preview.clone() } else { c.title.clone() })
            .unwrap_or_default();
        let thread = Thread {
            id: id.clone(),
            pid: *pid,
            rollout_path: log.path().to_string(),
            source: String::new(),
            cwd: conversation.map(|c| c.workspace.clone()).unwrap_or_default(),
            title,
            session_name: None,
            model: log.last_model(),
            tokens_used: Some(log.total_tokens()),
            nickname: conversation.map(|c| c.agent_name.clone()).filter(|n| !n.is_empty()),
            role: None,
            agent_path: None,
            created_at_ms: (conversation.and_then(|c| c.created_at).or(first_call_at).unwrap_or(0.0) * 1000.0) as i64,
            reasoning_effort: log.last_effort(),
            parent_id: parent_id.clone(),
            kind: if parent_id.is_some() { "spawn".to_string() } else { "main".to_string() },
            depth,
            origin: ORIGIN.to_string(),
        };
        let phase_at = conversation
            .and_then(|c| c.last_modified_at)
            .or(last_call_at)
            .unwrap_or(thread.created_at_ms as f64 / 1000.0);
        let activity = match status {
            "CASCADE_RUN_STATUS_CANCELING" => "canceling",
            _ if running => "running",
            _ => "complete",
        };
        states.insert(
            id.clone(),
            AgentRowState {
                phase: if running { "run".to_string() } else { "done".to_string() },
                phase_at,
                activity: activity.to_string(),
                total_tokens: Some(log.total_tokens()),
                weekly_used_percent: quota.and_then(|q| q.weekly).map(|l| l.used_percent),
                weekly_sample_at: quota.and_then(|q| q.weekly.map(|_| q.captured_at)),
                weekly_resets_at: quota.and_then(|q| q.weekly).and_then(|l| l.resets_at),
                five_hour_used_percent: quota.and_then(|q| q.five_hour).map(|l| l.used_percent),
                five_hour_sample_at: quota.and_then(|q| q.five_hour.map(|_| q.captured_at)),
                five_hour_resets_at: quota.and_then(|q| q.five_hour).and_then(|l| l.resets_at),
                ..AgentRowState::default()
            },
        );
        threads.push(thread);
    }
    logs.retain(|id, _| shown_ids.contains(id.as_str()));
    let error = match (lock_error, db_error) {
        (Some(a), Some(b)) => Some(format!("{}; {}", a, b)),
        (a, b) => a.or(b),
    };
    (threads, states, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(id: &str, parent: Option<&str>) -> Conversation {
        Conversation {
            id: id.to_string(),
            title: String::new(),
            preview: String::new(),
            workspace: String::new(),
            status: "CASCADE_RUN_STATUS_IDLE".to_string(),
            parent_id: parent.map(|p| p.to_string()),
            agent_name: String::new(),
            last_modified_at: None,
            created_at: None,
        }
    }

    #[test]
    fn descendants_of_a_held_conversation_are_shown_under_its_pid() {
        let conversations: HashMap<String, Conversation> = [
            conversation("root", None),
            conversation("child", Some("root")),
            conversation("grandchild", Some("child")),
            conversation("other", None),
            conversation("orphan", Some("gone")),
        ]
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
        let held: HashMap<String, i64> = [("root".to_string(), 42)].into_iter().collect();
        let shown = shown_conversations(&held, &conversations);
        assert_eq!(
            vec![("child".to_string(), 42), ("grandchild".to_string(), 42), ("root".to_string(), 42)],
            shown
        );
    }

    #[test]
    fn only_a_turn_in_progress_counts_as_running() {
        assert!(status_is_running("CASCADE_RUN_STATUS_RUNNING"));
        assert!(status_is_running("CASCADE_RUN_STATUS_BUSY"));
        assert!(status_is_running("CASCADE_RUN_STATUS_CANCELING"));
        assert!(!status_is_running("CASCADE_RUN_STATUS_IDLE"));
        assert!(!status_is_running("CASCADE_RUN_STATUS_UNSPECIFIED"));
    }
}
