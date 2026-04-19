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
        fs::create_dir_all(&sessions_dir)
            .with_context(|| format!("failed to create sessions directory: {}", sessions_dir.display()))?;

        let today = Local::now().format("%Y-%m-%d").to_string();
        let path = sessions_dir.join(format!("{today}.jsonl"));

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
        let message = Message::ToolCall(ToolCall { id, name, arguments });
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
