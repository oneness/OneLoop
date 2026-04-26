use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::messages::{AssistantMessage, Message, ToolCall, ToolResultMessage, UserMessage};

#[derive(Debug, Serialize, Deserialize)]
struct SessionEntry {
    message: Message,
}

#[derive(Debug)]
pub struct Session {
    messages: Vec<Message>,
    path: PathBuf,
}

impl Session {
    pub fn open_or_create(cwd: &Path) -> Result<Self> {
        let sessions_dir = cwd.join(".oneloop").join("sessions");
        fs::create_dir_all(&sessions_dir).with_context(|| {
            format!(
                "failed to create sessions directory: {}",
                sessions_dir.display()
            )
        })?;

        let today = Local::now().format("%Y-%m-%d").to_string();
        let path = find_latest_session(&sessions_dir, &today);

        let messages = if path.exists() {
            load_messages(&path)?
        } else {
            Vec::new()
        };

        Ok(Self { messages, path })
    }

    pub fn push_user(&mut self, content: String) -> Result<()> {
        let message = Message::User(UserMessage { content });
        self.append(message)
    }

    pub fn push_assistant(&mut self, content: String) -> Result<()> {
        let message = Message::Assistant(AssistantMessage { content });
        self.append(message)
    }

    pub fn push_tool_call(&mut self, id: String, name: String, arguments: Value) -> Result<()> {
        let message = Message::ToolCall(ToolCall {
            id,
            name,
            arguments,
        });
        self.append(message)
    }

    pub fn push_tool_result(
        &mut self,
        tool_call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
    ) -> Result<()> {
        let message = Message::ToolResult(ToolResultMessage {
            tool_call_id,
            tool_name,
            content,
            is_error,
        });
        self.append(message)
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start a new session file, preserving the old one on disk.
    /// Returns a fresh session with empty messages and a new file path.
    /// Suffix is always derived from the base date, so files go
    /// 2026-04-20.jsonl, 2026-04-20-001.jsonl, 2026-04-20-002.jsonl, etc.
    pub fn rotate(&self) -> Result<Self> {
        let sessions_dir = self
            .path
            .parent()
            .context("session file has no parent directory")?;

        // Extract the base date (e.g. "2026-04-20" from "2026-04-20-002.jsonl" or "2026-04-20.jsonl")
        let filename = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        let base_name = filename.split('-').take(3).collect::<Vec<_>>().join("-");

        let next = find_next_suffix(sessions_dir, &base_name);
        let new_name = format!("{base_name}-{next:03}.jsonl");
        let new_path = sessions_dir.join(new_name);

        Ok(Self {
            messages: Vec::new(),
            path: new_path,
        })
    }

    fn append(&mut self, message: Message) -> Result<()> {
        append_message(&self.path, &message)?;
        self.messages.push(message);
        Ok(())
    }
}

fn load_messages(path: &Path) -> Result<Vec<Message>> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open session file: {}", path.display()))?;

    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line =
            line.with_context(|| format!("failed to read session line from: {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: SessionEntry = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse session entry in: {}", path.display()))?;
        messages.push(entry.message);
    }

    Ok(messages)
}

fn append_message(path: &Path, message: &Message) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open session file for append: {}", path.display()))?;

    let entry = SessionEntry {
        message: message.clone(),
    };
    let line = serde_json::to_string(&entry).context("failed to serialize session entry")?;
    writeln!(file, "{line}")
        .with_context(|| format!("failed to append session entry to: {}", path.display()))?;
    Ok(())
}

/// Find the next available numeric suffix for session file rotation.
/// E.g., if "2026-04-20-001.jsonl" and "2026-04-20-002.jsonl" exist, returns 3.
/// Starts at 1 if no suffixed files exist.
fn find_next_suffix(sessions_dir: &Path, base_name: &str) -> u32 {
    let max = find_max_suffix(sessions_dir, base_name);
    max + 1
}

/// Find the highest numeric suffix among existing session files for the given base name.
/// Returns 0 if no suffixed files exist.
fn find_max_suffix(sessions_dir: &Path, base_name: &str) -> u32 {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return 0;
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Match pattern: <base_name>-NNN.jsonl
            let rest = name_str.strip_prefix(&format!("{base_name}-"))?;
            let stripped = rest.strip_suffix(".jsonl")?;
            stripped.parse::<u32>().ok()
        })
        .max()
        .unwrap_or(0)
}

/// Find the latest session file for a given date.
/// Checks for suffixed files (e.g. "2026-04-20-002.jsonl") and returns the
/// one with the highest suffix. Falls back to the base file ("2026-04-20.jsonl").
fn find_latest_session(sessions_dir: &Path, date: &str) -> PathBuf {
    let base_path = sessions_dir.join(format!("{date}.jsonl"));

    let max_suffix = if let Ok(entries) = fs::read_dir(sessions_dir) {
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let rest = name_str.strip_prefix(&format!("{date}-"))?;
                let stripped = rest.strip_suffix(".jsonl")?;
                stripped.parse::<u32>().ok()
            })
            .max()
    } else {
        None
    };

    match max_suffix {
        Some(n) => sessions_dir.join(format!("{date}-{n:03}.jsonl")),
        None => base_path,
    }
}
