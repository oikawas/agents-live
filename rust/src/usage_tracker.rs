//! Shared 30-day background usage tracker. Reads rollout/transcript usage in
//! a background thread and never writes a persistent cache. Mirrors
//! `ThirtyDayUsageTracker` in the Python script; Codex and Claude Code each
//! provide the source-specific pieces via `UsageSource`.

use crate::model::{ThirtyDayUsage, UsageTotals, USAGE_WINDOW_SECONDS};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub const USAGE_REFRESH_INTERVAL: f64 = 60.0;

/// Per-file incremental tail-reading state, implemented separately for
/// Codex rollout JSONL and Claude Code transcript JSONL.
pub trait FileState: Send {
    fn path(&self) -> &str;
    fn set_default_model(&mut self, model: Option<String>);
    fn refresh(&mut self, cutoff: f64);
    /// Sum of this file's retained (post-cutoff-eviction) usage buckets.
    fn totals(&self) -> UsageTotals;
}

/// Enumerates (source_id, path, default_model) rows to track and builds new
/// per-file state for a source id first seen this refresh.
pub trait UsageSource: Send + Sync {
    fn list_sources(&self, cutoff: f64) -> Result<Vec<(String, String, Option<String>)>, String>;
    fn make_state(&self, path: String, model: Option<String>) -> Box<dyn FileState>;
}

struct Inner {
    file_states: HashMap<String, Box<dyn FileState>>,
    summary: Option<ThirtyDayUsage>,
    #[allow(dead_code)]
    error: Option<String>,
    last_started_at: f64,
    running: bool,
}

pub struct ThirtyDayUsageTracker {
    source: Arc<dyn UsageSource>,
    refresh_interval: f64,
    inner: Arc<Mutex<Inner>>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl ThirtyDayUsageTracker {
    pub fn new(source: Arc<dyn UsageSource>) -> Self {
        ThirtyDayUsageTracker {
            source,
            refresh_interval: USAGE_REFRESH_INTERVAL,
            inner: Arc::new(Mutex::new(Inner {
                file_states: HashMap::new(),
                summary: None,
                error: None,
                last_started_at: 0.0,
                running: false,
            })),
        }
    }

    pub fn refresh(&self, background: bool) {
        let now = now_secs();
        {
            let mut guard = self.inner.lock().unwrap();
            if guard.running {
                return;
            }
            if guard.summary.is_some() && now - guard.last_started_at < self.refresh_interval {
                return;
            }
            guard.last_started_at = now;
            guard.running = true;
        }
        if background {
            let inner = Arc::clone(&self.inner);
            let source = Arc::clone(&self.source);
            std::thread::spawn(move || {
                Self::do_refresh(&inner, &*source, now);
            });
        } else {
            Self::do_refresh(&self.inner, &*self.source, now);
        }
    }

    fn do_refresh(inner: &Arc<Mutex<Inner>>, source: &dyn UsageSource, now: f64) {
        let cutoff = now - USAGE_WINDOW_SECONDS;
        let result = source.list_sources(cutoff);
        let rows = match result {
            Ok(rows) => rows,
            Err(err) => {
                let mut guard = inner.lock().unwrap();
                guard.error = Some(err);
                guard.running = false;
                return;
            }
        };

        // Refreshing file contents can be slow (disk I/O); do it without
        // holding the shared lock so `cost_usd()`/`priced_tokens()` readers
        // on the render thread never block on this background work.
        let mut local_states: HashMap<String, Box<dyn FileState>> = {
            let mut guard = inner.lock().unwrap();
            std::mem::take(&mut guard.file_states)
        };

        let mut active_ids: HashSet<String> = HashSet::new();
        for (source_id, path, model) in rows {
            active_ids.insert(source_id.clone());
            let needs_new = match local_states.get(&source_id) {
                Some(state) => state.path() != path,
                None => true,
            };
            if needs_new {
                local_states.insert(source_id.clone(), source.make_state(path, model));
            } else if let Some(state) = local_states.get_mut(&source_id) {
                state.set_default_model(model);
            }
            if let Some(state) = local_states.get_mut(&source_id) {
                state.refresh(cutoff);
            }
        }
        local_states.retain(|id, _| active_ids.contains(id));

        let mut total_tokens = 0i64;
        let mut priced_tokens = 0i64;
        let mut unpriced_tokens = 0i64;
        let mut cost_usd = 0.0f64;
        for state in local_states.values() {
            let totals = state.totals();
            total_tokens += totals.total_tokens;
            priced_tokens += totals.priced_tokens;
            unpriced_tokens += totals.unpriced_tokens;
            cost_usd += totals.cost_usd;
        }
        let summary = ThirtyDayUsage {
            total_tokens,
            priced_tokens,
            unpriced_tokens,
            cost_usd,
            sampled_at: now,
        };

        let mut guard = inner.lock().unwrap();
        guard.file_states = local_states;
        guard.summary = Some(summary);
        guard.error = None;
        guard.running = false;
    }

    pub fn cost_usd(&self) -> Option<f64> {
        self.inner.lock().unwrap().summary.as_ref().map(|s| s.cost_usd)
    }

    pub fn priced_tokens(&self) -> Option<i64> {
        self.inner.lock().unwrap().summary.as_ref().map(|s| s.priced_tokens)
    }

    pub fn unpriced_tokens(&self) -> Option<i64> {
        self.inner.lock().unwrap().summary.as_ref().map(|s| s.unpriced_tokens)
    }
}

/// Approximate Python's `subprocess`/`select` timeout style sleep used
/// elsewhere; kept here since usage refresh scheduling is time-based too.
#[allow(dead_code)]
pub fn sleep(duration: Duration) {
    std::thread::sleep(duration);
}
