//! bash's own history file: read for `!` completion and appended to when a `!`
//! command succeeds.
//!
//! Nothing here owns that file. It is never trimmed, rewritten, or renamed —
//! `HISTFILESIZE` decides how long it gets and `histappend` decides whether
//! anybody else's lines survive a shell exiting. This module only reads it and
//! adds a line.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

/// How many of the newest commands the matcher considers. The file is bash's,
/// not ours — it is only ever appended to — so this bounds the candidate list
/// rather than the history.
const CONSIDERED_COMMANDS: usize = 1000;

/// The commands bash has recorded, newest last.
///
/// Entries are one line each: bash joins a multi-line command onto a single
/// line unless `lithist` is set, so a line reader stays honest, and a
/// `#<epoch>` marker — written when `HISTTIMEFORMAT` is set — is skipped.
pub struct BashHistory {
    /// `None` when no history file can be located; the prompt still works, it
    /// just remembers nothing.
    path: Option<PathBuf>,
    /// Oldest first, deduplicated, at most [`CONSIDERED_COMMANDS`].
    commands: Vec<String>,
}

impl BashHistory {
    /// Reads the user's history from their own file. Every failure is an empty
    /// history: remembering commands is a convenience, and a prompt that cannot
    /// remember what you typed is still a prompt.
    pub fn open() -> Self {
        Self::load(history_path())
    }

    /// A history with no file behind it, for tests that only exercise
    /// matching.
    #[cfg(test)]
    pub fn for_test(commands: &[&str]) -> Self {
        Self {
            path: None,
            commands: commands.iter().map(ToString::to_string).collect(),
        }
    }

    /// Records a command that ran successfully: one copy at the newest position
    /// in memory, and one line appended for the next reader — bash, the Emacs
    /// client, or the next OneLoop.
    pub fn record(&mut self, command: &str) {
        self.remember(command);
        if let Some(path) = &self.path {
            let _ = append(path, command);
        }
    }

    /// Commands containing `fragment`, newest first, with prefix matches ahead
    /// of matches found mid-command.
    pub fn matches(&self, fragment: &str, limit: usize) -> Vec<&str> {
        let needle = fragment.to_lowercase();
        let mut prefixed = Vec::new();
        let mut containing = Vec::new();
        for command in self.commands.iter().rev() {
            let lowered = command.to_lowercase();
            if lowered.starts_with(&needle) {
                prefixed.push(command.as_str());
            } else if lowered.contains(&needle) {
                containing.push(command.as_str());
            }
        }
        prefixed.into_iter().chain(containing).take(limit).collect()
    }

    /// The command a hint may show. A hint is appended at the cursor instead of
    /// written over what is already typed, so only a prefix match can be
    /// offered — a command matched mid-string would have to rewrite the line.
    pub fn hint_match(&self, fragment: &str) -> Option<&str> {
        let needle = fragment.to_lowercase();
        self.commands
            .iter()
            .rev()
            .find(|command| command.to_lowercase().starts_with(&needle))
            .map(String::as_str)
    }

    fn load(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self {
                path: None,
                commands: Vec::new(),
            };
        };
        // Read as bytes: a history file can hold anything a terminal sent, and
        // a replacement character is a better outcome than no history at all.
        let contents = fs::read(&path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let mut commands = deduplicated(&contents);
        if commands.len() > CONSIDERED_COMMANDS {
            commands.drain(..commands.len() - CONSIDERED_COMMANDS);
        }
        Self {
            path: Some(path),
            commands,
        }
    }

    fn remember(&mut self, command: &str) {
        self.commands.retain(|seen| seen.as_str() != command);
        self.commands.push(command.to_string());
        if self.commands.len() > CONSIDERED_COMMANDS {
            self.commands.remove(0);
        }
    }
}

/// `ONELOOP_BASH_HISTORY`, then bash's own `HISTFILE`, then `~/.bash_history`.
fn history_path() -> Option<PathBuf> {
    let override_path = env::var("ONELOOP_BASH_HISTORY").ok();
    let histfile = env::var("HISTFILE").ok();
    let home = env::var("HOME").ok();
    resolve(
        override_path.as_deref(),
        histfile.as_deref(),
        home.as_deref(),
    )
}

/// The resolution itself, with the environment passed in so precedence can be
/// tested without mutating the process environment out from under every other
/// test. An empty value counts as unset, which is what an exported-but-empty
/// `HISTFILE` means.
fn resolve(
    override_path: Option<&str>,
    histfile: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    let named = |value: Option<&str>| {
        value
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
    };
    named(override_path)
        .or_else(|| named(histfile))
        .or_else(|| named(home).map(|home| home.join(".bash_history")))
}

/// Keeps the newest occurrence of each command, oldest first, skipping the
/// `#<epoch>` markers bash writes when `HISTTIMEFORMAT` is set.
fn deduplicated(contents: &str) -> Vec<String> {
    let mut commands: Vec<String> = Vec::new();
    for line in contents.lines() {
        let command = line.trim();
        if command.is_empty() || is_timestamp(command) {
            continue;
        }
        commands.retain(|seen| seen.as_str() != command);
        commands.push(command.to_string());
    }
    commands
}

/// A `#` followed by nine or more digits is bash's timestamp marker, not a
/// command: epochs have been ten digits since 2001.
fn is_timestamp(line: &str) -> bool {
    line.strip_prefix('#')
        .is_some_and(|rest| rest.len() >= 9 && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Appends one line. Best effort by design: a history that cannot be written is
/// not a reason to fail the command that ran.
fn append(path: &Path, command: &str) -> Result<()> {
    let mut file = open_for_append(path)?;
    file.write_all(format!("{command}\n").as_bytes())
        .with_context(|| format!("failed to append to {}", path.display()))
}

fn open_for_append(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    // Only consulted when the file has to be created: bash keeps its history
    // private, and a file created here should match.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{BashHistory, CONSIDERED_COMMANDS, deduplicated, resolve};
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn temp_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "oneloop-history-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    fn with_file(commands: &[&str], path: &Path) -> BashHistory {
        let mut history = BashHistory::for_test(commands);
        history.path = Some(path.to_path_buf());
        history
    }

    #[test]
    fn the_newest_occurrence_of_a_command_is_kept() {
        assert_eq!(
            deduplicated("git status\ncargo test\ngit status\n"),
            ["cargo test", "git status"]
        );
    }

    #[test]
    fn blank_lines_are_not_commands() {
        assert_eq!(deduplicated("\n\n   \n"), Vec::<String>::new());
    }

    #[test]
    fn timestamp_markers_are_not_commands() {
        assert_eq!(
            deduplicated("#1729876543\nls\n#1729876544\ngit status\n"),
            ["ls", "git status"]
        );
    }

    #[test]
    fn a_hash_line_too_short_to_be_a_timestamp_is_a_command() {
        assert_eq!(deduplicated("#1 is a comment\n"), ["#1 is a comment"]);
    }

    #[test]
    fn prefix_matches_outrank_matches_found_mid_command() {
        let history = BashHistory::for_test(&["git status", "cargo test", "git log --oneline"]);
        assert_eq!(
            history.matches("g", 5),
            ["git log --oneline", "git status", "cargo test"]
        );
    }

    #[test]
    fn a_hint_is_only_offered_for_a_prefix_match() {
        let history = BashHistory::for_test(&["git log --oneline"]);
        assert_eq!(history.hint_match("git lo"), Some("git log --oneline"));
        assert_eq!(history.hint_match("log"), None);
    }

    #[test]
    fn matching_ignores_case() {
        let history = BashHistory::for_test(&["Cargo Test"]);
        assert_eq!(history.matches("cargo", 5), ["Cargo Test"]);
    }

    #[test]
    fn recording_a_repeated_command_moves_it_to_the_newest_position() {
        let mut history = BashHistory::for_test(&["git status", "cargo test"]);
        history.record("git status");
        assert_eq!(history.commands, ["cargo test", "git status"]);
    }

    #[test]
    fn a_recorded_command_is_appended_to_the_file() {
        let path = temp_path("append");
        let mut history = with_file(&[], &path);
        history.record("cargo test");
        assert_eq!(fs::read_to_string(&path).unwrap(), "cargo test\n");
    }

    #[test]
    fn only_the_newest_commands_are_considered() {
        let path = temp_path("considered");
        let lines: String = (0..CONSIDERED_COMMANDS + 10)
            .map(|index| format!("echo {index}\n"))
            .collect();
        fs::write(&path, lines).unwrap();
        let history = BashHistory::load(Some(path));
        assert_eq!(history.commands.len(), CONSIDERED_COMMANDS);
    }

    /// The one that guards the data: a history three times the size we consider
    /// must come back untouched, because the file belongs to bash.
    #[test]
    fn reading_the_history_never_rewrites_it() {
        let path = temp_path("untouched");
        let lines: String = (0..CONSIDERED_COMMANDS * 3)
            .map(|index| format!("echo {index}\n"))
            .collect();
        fs::write(&path, &lines).unwrap();

        BashHistory::load(Some(path.clone()));

        assert_eq!(fs::read_to_string(&path).unwrap(), lines);
    }

    #[test]
    fn recording_never_rewrites_what_is_already_there() {
        let path = temp_path("no-prune");
        let lines: String = (0..CONSIDERED_COMMANDS + 10)
            .map(|index| format!("echo {index}\n"))
            .collect();
        fs::write(&path, &lines).unwrap();
        let mut history = BashHistory::load(Some(path.clone()));

        history.record("cargo test");

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{lines}cargo test\n")
        );
    }

    #[test]
    fn a_history_with_no_file_lives_only_in_memory() {
        let mut history = BashHistory::for_test(&[]);
        history.record("cargo test");
        assert_eq!(history.matches("cargo", 5), ["cargo test"]);
    }

    #[test]
    fn the_path_prefers_our_override_then_bashs_histfile_then_home() {
        assert_eq!(
            resolve(Some("/own/history"), Some("/bash/history"), Some("/home")),
            Some(PathBuf::from("/own/history"))
        );
        assert_eq!(
            resolve(None, Some("/bash/history"), Some("/home")),
            Some(PathBuf::from("/bash/history"))
        );
        assert_eq!(
            resolve(Some("  "), None, Some("/home")),
            Some(PathBuf::from("/home/.bash_history"))
        );
        assert_eq!(resolve(None, None, None), None);
    }
}
