use std::path::Path;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentContext;

use super::{Tool, ToolResult, resolve_path};

pub struct EditTool;

#[derive(Debug, Deserialize)]
struct EditInput {
    path: String,
    old_text: String,
    new_text: String,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> String {
        "Edit file contents".to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit. Relative to the \
                                    working directory unless absolute. `~` is \
                                    expanded; pass it through as-is."
                },
                "old_text": {
                    "type": "string",
                    "description": "Exact text to replace"
                },
                "new_text": {
                    "type": "string",
                    "description": "Replacement text"
                }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: EditInput = serde_json::from_value(input).context(
            "invalid edit input; expected { path: string, old_text: string, new_text: string }",
        )?;

        let path = resolve_path(&ctx.cwd, &input.path);

        if !tokio::fs::try_exists(&path)
            .await
            .with_context(|| format!("failed to check file existence: {}", path.display()))?
        {
            return Err(anyhow!("file does not exist: {}", path.display()));
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read file: {}", path.display()))?;

        let occurrences = content.matches(&input.old_text).count();
        if occurrences == 0 {
            return Err(anyhow!(not_found(&path, &content, &input.old_text)));
        }
        if occurrences > 1 {
            return Err(anyhow!(not_unique(
                &path,
                &content,
                &input.old_text,
                occurrences
            )));
        }

        let updated = content.replacen(&input.old_text, &input.new_text, 1);
        tokio::fs::write(&path, updated)
            .await
            .with_context(|| format!("failed to write edited file: {}", path.display()))?;

        Ok(ToolResult {
            content: format!("edited {}", path.display()),
            is_error: false,
        })
    }
}

/// A refusal that says how many matches there were and nothing else leaves the
/// model extending its anchor blind: in one session, ten consecutive attempts
/// on a file with two identical helper blocks failed on the same ambiguity.
/// Naming where every match is turns that guess into a choice.
fn not_unique(path: &Path, content: &str, old_text: &str, occurrences: usize) -> String {
    /// Enough to show the shape of the problem without pasting a file back.
    const LISTED: usize = 5;

    let lines = match_lines(content, old_text, LISTED);
    let mut message = format!(
        "old_text is not unique in file: {} ({occurrences} matches, nothing was written)",
        path.display()
    );
    for number in &lines {
        if let Some(line) = content.lines().nth(number - 1) {
            message.push_str(&format!("\n  {number}: {}", excerpt(line)));
        }
    }
    if occurrences > LISTED {
        message.push_str(&format!("\n  … and {} more", occurrences - LISTED));
    }
    message.push_str(
        "\nExtend old_text with the lines that differ between the matches, so it \
         names a single place.",
    );
    message
}

/// A miss is usually not a misunderstanding of the text but of its bytes: a
/// blank line padded with spaces, a line number copied out of a listing, or an
/// anchor from another file. Each has a distinct tell, and naming it is worth
/// more than the bare refusal, which leaves the model retrying the same shape.
fn not_found(path: &Path, content: &str, old_text: &str) -> String {
    let mut message = format!(
        "old_text not found in file: {} (nothing was written)",
        path.display()
    );
    if let Some(hint) = numbering_hint(content, old_text) {
        message.push_str(&hint);
    } else if differs_only_in_spacing(content, old_text) {
        message.push_str(
            "\nThe file holds this text with different spacing — a blank line \
             padded with spaces does not match an empty one. Copy the exact \
             spacing, or anchor on one distinctive line.",
        );
    } else if let Some((number, line)) = nearest_line(content, old_text) {
        message.push_str(&format!(
            "\nThe nearest line is {number}: {}\nAnchor on a line the file really \
             holds, or read the file again to copy it exactly.",
            excerpt(line)
        ));
    } else {
        message.push_str("\nRead the file again and copy its text exactly.");
    }
    message
}

/// The 1-based lines the first `limit` matches start on.
///
/// Bounded on purpose: an `old_text` of `""` matches every character boundary,
/// and counting lines from each offset in turn is quadratic in the file size.
/// The caller already knows the true total, so only the few it shows are found.
fn match_lines(content: &str, needle: &str, limit: usize) -> Vec<usize> {
    content
        .match_indices(needle)
        .take(limit)
        .map(|(offset, _)| content[..offset].matches('\n').count() + 1)
        .collect()
}

/// A line trimmed to something a diagnostic can quote without becoming one.
fn excerpt(line: &str) -> String {
    const MAX_CHARS: usize = 96;

    let line = line.trim_end();
    match line.char_indices().nth(MAX_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

/// `grep -n` writes `135-text` and some readers write `135:text`; no file
/// contains either, so a numbered line can never match.
///
/// The hint needs proof, not just a shape: a changelog line may legitimately
/// begin `2026-…`, so a prefix only counts when removing it yields a line the
/// file actually holds.
fn numbering_hint(content: &str, old_text: &str) -> Option<String> {
    old_text.lines().find_map(|line| {
        let stripped = strip_number_prefix(line)?;
        let held = content
            .lines()
            .any(|candidate| candidate.trim() == stripped.trim());
        held.then(|| {
            format!(
                "\nold_text looks copied from a numbered listing: `{}`. The `NNN-` or \
                 `NNN:` prefix is not part of the file — remove it.",
                excerpt(line)
            )
        })
    })
}

/// The text after a leading `NNN-` or `NNN:`.
fn strip_number_prefix(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    rest.strip_prefix('-')
        .or_else(|| rest.strip_prefix(':'))
        .map(str::trim_start)
}

/// The same text with each line trimmed. A blank line holding spaces and one
/// holding nothing are the same line to a reader and different bytes to a
/// match, which is exactly the miss this catches.
fn differs_only_in_spacing(content: &str, old_text: &str) -> bool {
    fn trimmed(text: &str) -> String {
        text.lines().map(str::trim).collect::<Vec<_>>().join("\n")
    }

    let wanted = trimmed(old_text);
    !wanted.trim().is_empty() && trimmed(content).contains(&wanted)
}

/// The file line sharing the longest prefix with the first line of `old_text`.
///
/// A short overlap (`/// `, `    `) matches half of any file, so only a long
/// one is offered — this is a pointer to the right neighbourhood, not a guess
/// at what was meant.
fn nearest_line<'a>(content: &'a str, old_text: &str) -> Option<(usize, &'a str)> {
    const MIN_SHARED_CHARS: usize = 12;

    let needle = old_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    content
        .lines()
        .enumerate()
        .map(|(index, line)| (shared_prefix(needle, line.trim()), index + 1, line))
        .filter(|(shared, _, _)| *shared >= MIN_SHARED_CHARS)
        .max_by_key(|(shared, _, _)| *shared)
        .map(|(_, number, line)| (number, line))
}

fn shared_prefix(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(left, right)| left == right)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> &'static Path {
        Path::new("/repo/file.rs")
    }

    /// A temp directory holding `file.txt`, so the tool is exercised the way
    /// the agent calls it — through `execute`, not through the helpers.
    fn dir_holding(name: &str, body: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-edit-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), body).unwrap();
        dir
    }

    async fn edit_error(cwd: &Path, old_text: &str) -> String {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        let input = json!({ "path": "file.txt", "old_text": old_text, "new_text": "x" });
        EditTool
            .execute(input, &ctx)
            .await
            .expect_err("the edit must be refused")
            .to_string()
    }

    /// The path this change did not touch: a unique anchor still writes, and
    /// says so, so a diagnostic that fires too eagerly cannot pass unnoticed.
    #[tokio::test]
    async fn a_unique_edit_rewrites_the_file() {
        let dir = dir_holding("unique", "alpha\nbeta\n");
        let ctx = AgentContext { cwd: dir.clone() };
        let result = EditTool
            .execute(
                json!({ "path": "file.txt", "old_text": "beta", "new_text": "gamma" }),
                &ctx,
            )
            .await
            .expect("the edit must succeed");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "alpha\ngamma\n"
        );
        assert!(result.content.starts_with("edited"), "{}", result.content);
    }

    /// The shape that cost ten attempts in one session: two identical blocks,
    /// and a refusal that said only "2 matches".
    #[tokio::test]
    async fn an_ambiguous_edit_names_every_candidate_line() {
        let dir = dir_holding("ambiguous", "same\nother\nsame\n");
        let message = edit_error(&dir, "same").await;
        assert!(message.contains("1: same"), "{message}");
        assert!(message.contains("3: same"), "{message}");
    }

    #[tokio::test]
    async fn a_padded_blank_line_is_diagnosed_as_spacing() {
        let dir = dir_holding("spacing", "a\n\nb\n");
        let message = edit_error(&dir, "a\n   \nb").await;
        assert!(message.contains("different spacing"), "{message}");
    }

    #[tokio::test]
    async fn a_failed_edit_leaves_the_file_untouched() {
        let dir = dir_holding("untouched", "a\n\nb\n");
        let _ = edit_error(&dir, "a\n   \nb").await;
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "a\n\nb\n"
        );
    }

    /// `grep -n` context lines reach the model as `135-text`; the prefix is not
    /// file content and can never match, so the refusal names it.
    #[test]
    fn line_numbers_copied_from_a_listing_are_named() {
        let message = not_found(
            path(),
            "atomic because it moves\n",
            "135-atomic because it moves",
        );
        assert!(message.contains("numbered listing"), "{message}");
    }

    /// A line that merely starts with a date is not a copied line number: the
    /// hint fires only when the file holds the text behind the prefix.
    #[test]
    fn a_date_prefixed_line_is_not_called_a_copied_line_number() {
        let message = not_found(path(), "- something else\n", "2026-release notes");
        assert!(!message.contains("numbered listing"), "{message}");
    }

    /// An anchor from a different file still points at the right
    /// neighbourhood, which is the whole value of the nearest line.
    #[test]
    fn a_missing_anchor_names_the_nearest_line() {
        let content = "first\n    /// A window that stops early must say where\nlast\n";
        let message = not_found(
            path(),
            content,
            "    /// A window that stops early must say where to resume",
        );
        assert!(message.contains("nearest line is 2"), "{message}");
    }

    /// Nothing similar is a plain refusal: a pointer invented from a shared
    /// `/// ` would send the model to the wrong place with confidence.
    #[test]
    fn an_unrelated_anchor_stays_a_plain_refusal() {
        let message = not_found(path(), "fn main() {}\n", "let x = 1;");
        assert!(!message.contains("nearest line"), "{message}");
    }

    /// Only the listed matches are shown, but the count says how many there
    /// really were, so a one-character anchor cannot paste a file back.
    #[test]
    fn a_refusal_summarises_matches_beyond_the_listed_ones() {
        let content = "x\nx\nx\nx\nx\nx\n";
        let message = not_unique(path(), content, "x", 6);
        assert!(message.contains("… and 1 more"), "{message}");
    }

    /// Every refusal says nothing was written, so a failed edit is never
    /// mistaken for a silent partial write.
    #[test]
    fn every_refusal_confirms_nothing_was_written() {
        for message in [
            not_found(path(), "fn main() {}\n", "let x = 1;"),
            not_unique(path(), "same\nsame\n", "same", 2),
        ] {
            assert!(message.contains("nothing was written"), "{message}");
        }
    }

    #[test]
    fn a_line_is_measured_from_its_first_character() {
        assert_eq!(match_lines("a\nb\na\n", "a", 5), vec![1, 3]);
    }

    /// An empty `old_text` matches every character boundary; only the few that
    /// get shown are located, so the work stays proportional to the file.
    #[test]
    fn only_the_listed_matches_are_located() {
        let content = "x\nx\nx\nx\nx\n";
        assert_eq!(match_lines(content, "x", 2), vec![1, 2]);
    }

    #[test]
    fn a_long_match_excerpt_is_shortened() {
        let line = "x".repeat(200);
        assert!(excerpt(&line).ends_with('…'));
    }
}
