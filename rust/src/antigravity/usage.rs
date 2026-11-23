//! 30-day usage for Antigravity: every `conversations/<id>.db` touched within
//! the window, each model call priced on its own.

use crate::antigravity::db::ConversationLog;
use crate::model::{antigravity_api_equivalent_cost, UsageTotals};
use crate::usage_tracker::{FileState, UsageSource};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

pub struct AntigravityUsageSource {
    pub home: PathBuf,
}

fn modified_secs(path: &std::path::Path) -> Option<f64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs_f64())
}

impl UsageSource for AntigravityUsageSource {
    fn list_sources(&self, cutoff: f64) -> Result<Vec<(String, String, Option<String>)>, String> {
        let dir = self.home.join("conversations");
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // Not installed (or never run): nothing to count, not an error.
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e != "db").unwrap_or(true) {
                continue;
            }
            let wal = PathBuf::from(format!("{}-wal", path.display()));
            let touched = [modified_secs(&path), modified_secs(&wal)].into_iter().flatten().reduce(f64::max);
            if touched.map(|t| t < cutoff).unwrap_or(true) {
                continue;
            }
            let id = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            out.push((id, path.to_string_lossy().to_string(), None));
        }
        Ok(out)
    }

    fn make_state(&self, path: String, _model: Option<String>) -> Box<dyn FileState> {
        Box::new(AntigravityUsageFileState { log: ConversationLog::new(path), cutoff: 0.0 })
    }
}

pub struct AntigravityUsageFileState {
    log: ConversationLog,
    cutoff: f64,
}

impl FileState for AntigravityUsageFileState {
    fn path(&self) -> &str {
        self.log.path()
    }

    fn set_default_model(&mut self, _model: Option<String>) {}

    fn refresh(&mut self, cutoff: f64) {
        self.cutoff = cutoff;
        self.log.refresh();
    }

    fn totals(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for event in self.log.events.iter().filter(|e| e.at >= self.cutoff) {
            let tokens = event.usage.total_tokens();
            totals.total_tokens += tokens;
            match antigravity_api_equivalent_cost(event.model.as_deref(), &event.usage) {
                Some(cost) => {
                    totals.priced_tokens += tokens;
                    totals.cost_usd += cost;
                }
                None => totals.unpriced_tokens += tokens,
            }
        }
        totals
    }
}
