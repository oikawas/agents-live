use crate::model::{source_info, Thread};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

/// Load thread metadata + spawn-edge parent ids for the given held thread
/// ids (id -> pid). Mirrors `load_threads` in the Python script.
pub fn load_threads(
    db_path: &Path,
    held: &HashMap<String, i64>,
) -> rusqlite::Result<Vec<Thread>> {
    if held.is_empty() {
        return Ok(Vec::new());
    }
    let uri = format!("file:{}?mode=ro", db_path.display());
    let connection = Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(1))?;

    let ids: Vec<&String> = held.keys().collect();
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");

    let mut edges: HashMap<String, String> = HashMap::new();
    {
        let sql = format!(
            "SELECT parent_thread_id, child_thread_id FROM thread_spawn_edges WHERE child_thread_id IN ({})",
            placeholders
        );
        let mut stmt = connection.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| *id as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt.query(params.as_slice())?;
        while let Some(row) = rows.next()? {
            let parent: String = row.get(0)?;
            let child: String = row.get(1)?;
            edges.insert(child, parent);
        }
    }

    let mut threads = Vec::new();
    {
        let sql = format!(
            "SELECT id, rollout_path, source, cwd, title, name, model, reasoning_effort, tokens_used, \
             agent_nickname, agent_role, agent_path, COALESCE(created_at_ms, created_at * 1000) AS created_ms \
             FROM threads WHERE id IN ({})",
            placeholders
        );
        let mut stmt = connection.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| *id as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt.query(params.as_slice())?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let rollout_path: String = row.get(1)?;
            let source: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let cwd: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            let title: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
            let session_name: Option<String> = row.get(5)?;
            let model: Option<String> = row.get(6)?;
            let reasoning_effort: Option<String> = row.get(7)?;
            let tokens_used: Option<i64> = row.get(8)?;
            let nickname: Option<String> = row.get(9)?;
            let role: Option<String> = row.get(10)?;
            let agent_path: Option<String> = row.get(11)?;
            let created_ms: Option<i64> = row.get(12)?;

            let (kind, spawn) = source_info(&source);
            let mut parent_id = edges.get(&id).cloned();
            if kind == "spawn" && parent_id.is_none() {
                parent_id = spawn
                    .get("parent_thread_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            let depth = if kind == "spawn" {
                spawn.get("depth").and_then(|v| v.as_i64()).unwrap_or(0)
            } else {
                0
            };
            let pid = *held.get(&id).unwrap_or(&0);
            threads.push(Thread {
                id,
                pid,
                rollout_path,
                source,
                cwd,
                title,
                session_name,
                model,
                tokens_used,
                nickname,
                role,
                agent_path,
                created_at_ms: created_ms.unwrap_or(0),
                reasoning_effort,
                parent_id,
                kind: kind.to_string(),
                depth,
                origin: "codex".to_string(),
            });
        }
    }
    Ok(threads)
}

/// List (id, rollout_path, model) rows updated at/after `cutoff` (epoch
/// seconds), for the 30-day usage tracker. Mirrors `CodexUsageTracker._list_sources`.
pub fn list_usage_sources(
    db_path: &Path,
    cutoff: f64,
) -> rusqlite::Result<Vec<(String, String, Option<String>)>> {
    let uri = format!("file:{}?mode=ro", db_path.display());
    let connection = Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(1))?;
    let cutoff_ms = (cutoff * 1000.0) as i64;
    let mut stmt = connection.prepare(
        "SELECT id, rollout_path, model FROM threads \
         WHERE rollout_path IS NOT NULL AND COALESCE(updated_at_ms, updated_at * 1000) >= ?",
    )?;
    let mut rows = stmt.query([cutoff_ms])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let path: String = row.get(1)?;
        let model: Option<String> = row.get(2)?;
        result.push((id, path, model));
    }
    Ok(result)
}
