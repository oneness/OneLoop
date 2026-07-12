use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;
use serde_json::Value;
use crate::output::{DIM, RESET};

/// Append-only metrics log, one JSON line per event.
/// File lives at `.oneloop/metrics/<session-filename>.jsonl`,
/// mirroring the session file naming for easy correlation.
pub struct Metrics {
    path: PathBuf,
}

impl Metrics {
    /// Create a metrics file matching the given session path.
    /// `.oneloop/sessions/2026-04-20.jsonl` → `.oneloop/metrics/2026-04-20.jsonl`
    pub fn from_session_path(session_path: &Path) -> Result<Self> {
        let project_dir = session_path
            .parent() // sessions/
            .and_then(|p| p.parent()) // .oneloop/
            .context("session path is too shallow to derive metrics directory")?;

        let metrics_dir = project_dir.join("metrics");
        fs::create_dir_all(&metrics_dir).with_context(|| {
            format!(
                "failed to create metrics directory: {}",
                metrics_dir.display()
            )
        })?;

        let filename = session_path
            .file_name()
            .context("session path has no filename")?;

        Ok(Self {
            path: metrics_dir.join(filename),
        })
    }

    /// Append a metrics event. Errors are printed to stderr, never propagated.
    pub fn log(&self, event: &str, data: Value) {
        if let Err(e) = self.try_log(event, data) {
            eprintln!("{DIM}  [metrics] {e:#}{RESET}");
        }
    }

    fn try_log(&self, event: &str, data: Value) -> Result<()> {
        let ts = Local::now().to_rfc3339();
        let mut entry = serde_json::json!({ "ts": ts, "event": event });

        if let Value::Object(data_map) = data
            && let Value::Object(ref mut entry_map) = entry
        {
            for (k, v) in data_map {
                entry_map.insert(k, v);
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open metrics file: {}", self.path.display()))?;

        serde_json::to_writer(&mut file, &entry)
            .with_context(|| format!("failed to write metrics: {}", self.path.display()))?;
        writeln!(file)
            .with_context(|| format!("failed to write metrics: {}", self.path.display()))?;

        Ok(())
    }

    #[expect(dead_code, reason = "useful for debugging, will be needed later")]
    pub fn path(&self) -> &Path {
        &self.path
    }
}
