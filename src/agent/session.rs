use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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
    /// The project this conversation belongs to, from the working directory's
    /// name. Prefixed to the id sent to providers so two projects worked on
    /// the same day do not look like one conversation.
    project: String,
    /// Whether messages are written to `path`. A one-shot session keeps them
    /// in memory and touches no file.
    persist: bool,
    /// The project's lock, held for as long as this process owns the session
    /// files. Dropped — and the lock released by the kernel — when the session
    /// goes away with the process.
    lock: Option<File>,
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
        let lock = acquire_lock(cwd)?;

        let today = Local::now().format("%Y-%m-%d").to_string();
        let path = find_latest_session(&sessions_dir, &today);

        let messages = if path.exists() {
            load_messages(&path)?
        } else {
            Vec::new()
        };

        Ok(Self {
            messages,
            path,
            project: project_name(cwd),
            persist: true,
            lock: Some(lock),
        })
    }

    /// A session that lives only in memory: nothing is read on the way in,
    /// nothing is written on the way out. A one-shot run uses one so it
    /// neither inherits nor disturbs the conversation on disk.
    pub fn in_memory(cwd: &Path) -> Self {
        let today = Local::now().format("%Y-%m-%d").to_string();
        Self {
            messages: Vec::new(),
            // Named but never created: it only gives `id()` a stem to report.
            path: cwd
                .join(".oneloop")
                .join("sessions")
                .join(format!("{today}.jsonl")),
            project: project_name(cwd),
            persist: false,
            lock: None,
        }
    }

    pub fn push_user(&mut self, content: String) -> Result<()> {
        let message = Message::User(UserMessage { content });
        self.append(message)
    }

    pub fn push_assistant(&mut self, content: String) -> Result<()> {
        self.push_assistant_with_reasoning(content, None)
    }

    pub fn push_assistant_with_reasoning(
        &mut self,
        content: String,
        reasoning_content: Option<String>,
    ) -> Result<()> {
        let message = Message::Assistant(AssistantMessage {
            content,
            reasoning_content,
        });
        self.append(message)
    }

    pub fn push_tool_call(&mut self, id: String, name: String, arguments: Value) -> Result<()> {
        let message = Message::ToolCall(ToolCall {
            id,
            name,
            arguments,
            parse_error: None,
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

    /// Stable identifier for the conversation, shared across requests in this
    /// session: the project name and the session file's stem. Never empty, so
    /// it is always a valid header value.
    pub fn id(&self) -> String {
        let stem = self
            .path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("session");
        format!("{}-{stem}", self.project)
    }

    /// The old file stays on disk. Suffixes derive from the base date:
    /// 2026-04-20.jsonl, 2026-04-20-001.jsonl, 2026-04-20-002.jsonl.
    pub fn rotate(&self) -> Result<Self> {
        let sessions_dir = self
            .path
            .parent()
            .context("session file has no parent directory")?;

        // Extract the base date (e.g. "2026-04-20" from "2026-04-20-002.jsonl"
        // or "2026-04-20.jsonl"). Assumes date-shaped filenames: the first
        // three dash-separated parts are the date, anything after is the
        // rotation suffix.
        let filename = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        let base_name = filename.split('-').take(3).collect::<Vec<_>>().join("-");

        let next = max_suffix(sessions_dir, &base_name).map_or(1, |n| n + 1);
        let new_name = format!("{base_name}-{next:03}.jsonl");
        let new_path = sessions_dir.join(new_name);

        // The clone shares the lock: `/clear` moves to a new file in the same
        // project, not to a project nobody claimed.
        let lock = self
            .lock
            .as_ref()
            .map(File::try_clone)
            .transpose()
            .context("failed to carry the session lock across /clear")?;
        Ok(Self {
            messages: Vec::new(),
            path: new_path,
            project: self.project.clone(),
            persist: self.persist,
            lock,
        })
    }

    /// Append error results for tool calls that never received one — e.g. a
    /// run cancelled between recording the calls and their results. Providers
    /// reject conversations containing dangling tool calls, so an unrepaired
    /// session would break every later request. Returns how many were closed.
    pub fn repair_dangling_tool_calls(&mut self) -> Result<usize> {
        let mut pending: Vec<(String, String)> = Vec::new();
        for message in &self.messages {
            match message {
                Message::ToolCall(tool_call) => {
                    pending.push((tool_call.id.clone(), tool_call.name.clone()));
                }
                Message::ToolResult(tool_result) => {
                    pending.retain(|(id, _)| id != &tool_result.tool_call_id);
                }
                _ => {}
            }
        }

        let count = pending.len();
        for (id, name) in pending {
            self.push_tool_result(
                id,
                name,
                "[interrupted — the run was cancelled before this tool finished]".to_string(),
                true,
            )?;
        }
        Ok(count)
    }

    fn append(&mut self, message: Message) -> Result<()> {
        if self.persist {
            append_message(&self.path, &message)?;
        }
        self.messages.push(message);
        Ok(())
    }

    /// A session for tests: in memory, so no test writes to a real path.
    #[cfg(test)]
    fn for_test(messages: Vec<Message>, path: PathBuf, project: &str) -> Self {
        Self {
            messages,
            path,
            project: project.to_string(),
            persist: false,
            lock: None,
        }
    }
}

/// The working directory's name, lowercased and reduced to characters a header
/// may carry: `oneloop` for this repo, `foo` for a checkout of foo. The id
/// keeps it as a prefix, so two projects on the same day are not one id.
fn project_name(cwd: &Path) -> String {
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "oneloop".to_string()
    } else {
        cleaned
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

fn max_suffix(sessions_dir: &Path, prefix: &str) -> Option<u32> {
    let entries = fs::read_dir(sessions_dir).ok()?;
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let rest = name_str.strip_prefix(&format!("{prefix}-"))?;
            rest.strip_suffix(".jsonl")?.parse::<u32>().ok()
        })
        .max()
}

/// The highest-suffixed file for the date, or the base one.
/// Claims the project for this process.
///
/// Two OneLoop runs in one project would both append to the same session file,
/// and neither can tell a tool call that is still running from one that died —
/// the startup repair would answer a live call and leave two results for it.
/// So the second run stops here instead.
fn acquire_lock(cwd: &Path) -> Result<File> {
    let path = cwd.join(".oneloop").join("session.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => bail!(
            "another OneLoop is running in this project ({} is locked); its session file is \
             append-only, so two would answer each other's tool calls",
            path.display()
        ),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("failed to lock {}", path.display()))
        }
    }
}

fn find_latest_session(sessions_dir: &Path, date: &str) -> PathBuf {
    match max_suffix(sessions_dir, date) {
        Some(n) => sessions_dir.join(format!("{date}-{n:03}.jsonl")),
        None => sessions_dir.join(format!("{date}.jsonl")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_session(name: &str) -> Session {
        let path = std::env::temp_dir().join(format!(
            "oneloop-session-test-{}-{name}.jsonl",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        Session::for_test(Vec::new(), path, "oneloop")
    }

    #[test]
    fn a_second_session_in_the_same_project_is_refused() {
        let cwd = project_dir("second-session");
        let _first = Session::open_or_create(&cwd).unwrap();

        assert!(Session::open_or_create(&cwd).is_err());
    }

    #[test]
    fn the_refusal_names_the_other_run() {
        let cwd = project_dir("refusal-message");
        let _first = Session::open_or_create(&cwd).unwrap();

        let error = Session::open_or_create(&cwd).unwrap_err();

        assert!(format!("{error:#}").contains("another OneLoop is running"));
    }

    #[test]
    fn a_finished_session_releases_the_project() {
        let cwd = project_dir("released-project");
        drop(Session::open_or_create(&cwd).unwrap());

        assert!(Session::open_or_create(&cwd).is_ok());
    }

    #[test]
    fn repair_closes_tool_calls_without_results() {
        let mut session = temp_session("dangling");
        session
            .push_tool_call("call-1".into(), "bash".into(), json!({"command": "ls"}))
            .unwrap();
        session
            .push_tool_call("call-2".into(), "read".into(), json!({"path": "x"}))
            .unwrap();
        session
            .push_tool_result("call-1".into(), "bash".into(), "ok".into(), false)
            .unwrap();

        let repaired = session.repair_dangling_tool_calls().unwrap();

        assert_eq!(repaired, 1);
        match session.messages().last().unwrap() {
            Message::ToolResult(result) => {
                assert_eq!(result.tool_call_id, "call-2");
                assert!(result.is_error);
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
        let _ = fs::remove_file(session.path());
    }

    /// The id a provider sees is the project and the session's own name, so
    /// two projects worked on the same day are not mistaken for one session.
    #[test]
    fn the_id_prefixes_the_project() {
        let session = temp_session("id");
        let id = session.id();
        assert!(id.starts_with("oneloop-"), "got: {id}");
        assert!(id.len() > "oneloop-".len());

        let same_day_other_project =
            Session::for_test(Vec::new(), session.path().to_path_buf(), "foo");
        assert_ne!(same_day_other_project.id(), id);
    }

    /// A directory can be named anything, and the id travels in a header: a
    /// header value must be plain ASCII with no spaces or control characters.
    #[test]
    fn a_project_name_is_safe_for_a_header() {
        assert_eq!(project_name(Path::new("/home/x/oneloop")), "oneloop");
        assert_eq!(project_name(Path::new("/home/x/Foo Bar")), "foo-bar");
        assert_eq!(project_name(Path::new("/home/x/café")), "caf-");
        // A path with no usable last component falls back to the client name.
        assert_eq!(project_name(Path::new("/")), "oneloop");
    }

    #[test]
    fn repair_leaves_complete_sessions_untouched() {
        let mut session = temp_session("complete");
        session
            .push_tool_call("call-1".into(), "bash".into(), json!({"command": "ls"}))
            .unwrap();
        session
            .push_tool_result("call-1".into(), "bash".into(), "ok".into(), false)
            .unwrap();

        let repaired = session.repair_dangling_tool_calls().unwrap();

        assert_eq!(repaired, 0);
        assert_eq!(session.messages().len(), 2);
        let _ = fs::remove_file(session.path());
    }

    /// A one-shot uses one of these: it must not inherit a session on disk.
    #[test]
    fn an_in_memory_session_ignores_a_prior_session_file() {
        let dir = project_dir("mem-read");
        fs::write(today_file(&dir), "prior\n").unwrap();

        let session = Session::in_memory(&dir);

        assert!(session.messages().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Nor may it write one, so it leaves no trace for the next run.
    #[test]
    fn an_in_memory_session_writes_no_file() {
        let dir = project_dir("mem-write");
        let file = today_file(&dir);

        let mut session = Session::in_memory(&dir);
        session.push_user("hello".to_string()).unwrap();

        assert!(!file.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    fn project_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oneloop-session-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".oneloop").join("sessions")).unwrap();
        dir
    }

    fn today_file(cwd: &Path) -> PathBuf {
        let today = Local::now().format("%Y-%m-%d").to_string();
        cwd.join(".oneloop")
            .join("sessions")
            .join(format!("{today}.jsonl"))
    }
}
