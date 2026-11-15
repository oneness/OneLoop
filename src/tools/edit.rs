use std::path::Path;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    path::resolve_path,
    tools::{AgentContext, PATH_ARGUMENT, path_trace_label},
};

use super::{Tool, ToolResult};

pub struct EditTool;

#[derive(Debug, Deserialize)]
struct EditInput {
    path: String,
    edits: Vec<Replacement>,
}

#[derive(Debug, Deserialize)]
struct Replacement {
    old_text: String,
    new_text: String,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> String {
        "Edit a file with one or more exact text replacements".to_string()
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
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One or more exact replacements in this file. Every old_text is matched against the original file, must occur exactly once, and must not overlap another edit. Combine nearby changes into one edit.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Non-empty exact text to replace; it must occur exactly once in the original file."
                            },
                            "new_text": {
                                "type": "string",
                                "description": "Replacement text; an empty string deletes the matched text."
                            }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    /// The resolved path, spelled the way every other tool in the trace spells
    /// it. A call that arrived without one says so.
    fn trace_label(&self, arguments: &Value, cwd: &Path) -> Option<String> {
        path_trace_label(arguments, cwd)
    }

    fn lock_path(&self) -> Option<&'static str> {
        Some(PATH_ARGUMENT)
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: EditInput = serde_json::from_value(input).context(
            "invalid edit input; expected { path: string, edits: [{ old_text: non-empty string, new_text: string }] }",
        )?;
        if input.edits.is_empty() {
            return Err(anyhow!("edits must contain at least one replacement"));
        }
        if let Some((index, _)) = input
            .edits
            .iter()
            .enumerate()
            .find(|(_, edit)| edit.old_text.is_empty())
        {
            return Err(anyhow!("edits[{index}].old_text must not be empty"));
        }

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

        let mut matches = Vec::with_capacity(input.edits.len());
        for (index, edit) in input.edits.iter().enumerate() {
            let mut occurrences = content.match_indices(&edit.old_text);
            let Some((start, _)) = occurrences.next() else {
                return Err(anyhow!(
                    "edits[{index}]: {}",
                    not_found(&path, &content, &edit.old_text)
                ));
            };
            if let Some((next_start, _)) = occurrences.next() {
                if next_start < start + edit.old_text.len() {
                    return Err(anyhow!(
                        "edits[{index}].old_text has overlapping matches in file (nothing was written); provide a longer anchor"
                    ));
                }
                let count = 2 + occurrences.count();
                return Err(anyhow!(
                    "edits[{index}]: {}",
                    not_unique(&path, &content, &edit.old_text, count)
                ));
            }
            let next_character = start + edit.old_text.chars().next().map_or(0, char::len_utf8);
            if next_character < content.len() && content[next_character..].contains(&edit.old_text)
            {
                return Err(anyhow!(
                    "edits[{index}].old_text has overlapping matches in file (nothing was written); provide a longer anchor"
                ));
            }
            matches.push((start, start + edit.old_text.len(), index));
        }

        matches.sort_unstable_by_key(|(start, _, _)| *start);
        for pair in matches.windows(2) {
            let (start, end, index) = pair[0];
            let (next_start, _, next_index) = pair[1];
            if next_start < end {
                return Err(anyhow!(
                    "edits[{index}] and edits[{next_index}] overlap in file: {} (nothing was written); combine overlapping changes into one edit",
                    path.display()
                ));
            }
            debug_assert!(start < end);
        }

        let mut updated = content;
        for (start, end, index) in matches.into_iter().rev() {
            let edit = &input.edits[index];
            updated.replace_range(start..end, &edit.new_text);
        }

        tokio::fs::write(&path, updated)
            .await
            .with_context(|| format!("failed to write edited file: {}", path.display()))?;

        Ok(ToolResult {
            content: format!(
                "applied {} edit(s) to {}",
                input.edits.len(),
                path.display()
            ),
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

    async fn edit_error(cwd: &Path, edits: Value) -> String {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        let input = json!({ "path": "file.txt", "edits": edits });
        EditTool
            .execute(input, &ctx)
            .await
            .expect_err("the edit must be refused")
            .to_string()
    }

    /// Independent replacements are all matched against the same original and
    /// written together, so one tool call can change several regions safely.
    #[tokio::test]
    async fn multiple_disjoint_edits_apply_together() {
        let dir = dir_holding("multiple", "alpha\nbeta\ngamma\n");
        let ctx = AgentContext { cwd: dir.clone() };
        let result = EditTool
            .execute(
                json!({
                    "path": "file.txt",
                    "edits": [
                        {"old_text": "alpha", "new_text": "first"},
                        {"old_text": "gamma", "new_text": "last"}
                    ]
                }),
                &ctx,
            )
            .await
            .expect("both disjoint replacements should apply");

        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "first\nbeta\nlast\n"
        );
        assert!(result.content.contains("2 edit(s)"), "{}", result.content);
    }

    /// An invalid member rejects the whole batch before any replacement is
    /// written, even when an earlier member matched uniquely.
    #[tokio::test]
    async fn a_missing_match_rejects_the_whole_edit_batch() {
        let dir = dir_holding("batch-missing", "alpha\nbeta\n");
        let error = edit_error(
            &dir,
            json!([
                {"old_text": "alpha", "new_text": "first"},
                {"old_text": "missing", "new_text": "last"}
            ]),
        )
        .await;

        assert!(error.contains("edits[1]"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "alpha\nbeta\n"
        );
    }

    /// Match ranges that overlap in the original are rejected rather than
    /// applying replacements whose meaning depends on order.
    #[tokio::test]
    async fn overlapping_edit_ranges_are_rejected_without_writing() {
        let dir = dir_holding("batch-overlap", "abcdef\n");
        let error = edit_error(
            &dir,
            json!([
                {"old_text": "abc", "new_text": "left"},
                {"old_text": "cde", "new_text": "right"}
            ]),
        )
        .await;

        assert!(error.contains("overlap"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "abcdef\n"
        );
    }

    /// Applying from the end preserves original offsets when an earlier edit
    /// changes length, and an empty replacement deletes text in the same batch.
    #[tokio::test]
    async fn a_batch_handles_length_changes_and_deletions_together() {
        let dir = dir_holding("batch-offsets-delete", "a middle z\n");
        let ctx = AgentContext { cwd: dir.clone() };
        EditTool
            .execute(
                json!({
                    "path": "file.txt",
                    "edits": [
                        {"old_text": "a", "new_text": "long beginning"},
                        {"old_text": " middle", "new_text": ""}
                    ]
                }),
                &ctx,
            )
            .await
            .expect("the disjoint replacement and deletion should apply");

        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "long beginning z\n"
        );
    }

    /// A non-empty anchor still performs a unique replacement and reports the
    /// number of edits applied.
    #[tokio::test]
    async fn a_unique_edit_rewrites_the_file() {
        let dir = dir_holding("unique", "alpha\nbeta\n");
        let ctx = AgentContext { cwd: dir.clone() };
        let result = EditTool
            .execute(
                json!({ "path": "file.txt", "edits": [{ "old_text": "beta", "new_text": "gamma" }] }),
                &ctx,
            )
            .await
            .expect("the edit must succeed");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "alpha\ngamma\n"
        );
        assert_eq!(
            result.content,
            format!("applied 1 edit(s) to {}", dir.join("file.txt").display())
        );
    }

    /// All matches are located before mutation, so repeated text in one
    /// replacement is rejected rather than applying to an arbitrary occurrence.
    #[tokio::test]
    async fn an_ambiguous_member_rejects_the_whole_edit_batch() {
        let dir = dir_holding("batch-ambiguous-single", "same\nother\nsame\n");
        let error = edit_error(&dir, json!([{"old_text": "same", "new_text": "new"}])).await;

        assert!(error.contains("edits[0]"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "same\nother\nsame\n"
        );
    }

    /// A substring with overlapping starting positions is ambiguous even when
    /// non-overlapping string iteration would report only one occurrence.
    #[tokio::test]
    async fn a_self_overlapping_anchor_is_refused_without_writing() {
        let dir = dir_holding("self-overlap", "aaa\n");
        let error = edit_error(&dir, json!([{"old_text": "aa", "new_text": "x"}])).await;

        assert!(error.contains("overlapping matches"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "aaa\n"
        );
    }

    /// Two identical edit entries target the same source range and therefore
    /// overlap; reject both instead of letting array order decide the result.
    #[tokio::test]
    async fn duplicate_edit_ranges_are_rejected_without_writing() {
        let dir = dir_holding("batch-duplicate", "same\n");
        let error = edit_error(
            &dir,
            json!([
                {"old_text": "same", "new_text": "first"},
                {"old_text": "same", "new_text": "second"}
            ]),
        )
        .await;

        assert!(error.contains("overlap"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "same\n"
        );
    }

    /// A later ambiguous member also rejects the whole call; earlier matches
    /// are not applied before ambiguity validation completes.
    #[tokio::test]
    async fn a_later_ambiguous_edit_rejects_the_whole_batch() {
        let dir = dir_holding("batch-ambiguous-later", "first\nsame\nother\nsame\n");
        let error = edit_error(
            &dir,
            json!([
                {"old_text": "first", "new_text": "changed"},
                {"old_text": "same", "new_text": "new"}
            ]),
        )
        .await;

        assert!(error.contains("edits[1]"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "first\nsame\nother\nsame\n"
        );
    }

    /// Empty batches are rejected before reading or writing a path.
    #[tokio::test]
    async fn an_empty_edit_batch_is_refused() {
        let dir = dir_holding("batch-empty", "content\n");
        let error = edit_error(&dir, json!([])).await;
        assert!(error.contains("at least one replacement"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "content\n"
        );
    }

    /// Non-overlapping ranges are measured in the original file, even when one
    /// replacement inserts text that another edit also names.
    #[tokio::test]
    async fn replacements_do_not_match_text_inserted_by_another_edit() {
        let dir = dir_holding("batch-original", "first\nsecond\n");
        let ctx = AgentContext { cwd: dir.clone() };
        EditTool
            .execute(
                json!({
                    "path": "file.txt",
                    "edits": [
                        {"old_text": "first", "new_text": "second"},
                        {"old_text": "second", "new_text": "last"}
                    ]
                }),
                &ctx,
            )
            .await
            .expect("both original ranges are disjoint");

        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "second\nlast\n"
        );
    }

    #[test]
    fn the_schema_requires_a_nonempty_old_text() {
        let schema = EditTool.schema();
        assert_eq!(schema["properties"]["edits"]["minItems"], 1);
        assert_eq!(
            schema["properties"]["edits"]["items"]["properties"]["old_text"]["minLength"],
            1
        );
        assert_eq!(schema["required"], json!(["path", "edits"]));
        assert_eq!(
            schema["properties"]["edits"]["items"]["required"],
            json!(["old_text", "new_text"])
        );
    }

    #[tokio::test]
    async fn an_empty_old_text_is_refused_without_changing_the_file() {
        let dir = dir_holding("empty-old-text", "alpha\nbeta\n");
        let ctx = AgentContext { cwd: dir.clone() };

        let error = EditTool
            .execute(
                json!({"path": "file.txt", "edits": [{"old_text": "", "new_text": "replacement"}]}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("old_text must not be empty"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "alpha\nbeta\n"
        );
    }

    /// The shape that cost ten attempts in one session: two identical blocks,
    /// and a refusal that said only "2 matches".
    #[tokio::test]
    async fn an_ambiguous_edit_names_every_candidate_line() {
        let dir = dir_holding("ambiguous", "same\nother\nsame\n");
        let message = edit_error(&dir, json!([{"old_text": "same", "new_text": "x"}])).await;
        assert!(message.contains("1: same"), "{message}");
        assert!(message.contains("3: same"), "{message}");
    }

    #[tokio::test]
    async fn a_padded_blank_line_is_diagnosed_as_spacing() {
        let dir = dir_holding("spacing", "a\n\nb\n");
        let message = edit_error(&dir, json!([{"old_text": "a\n   \nb", "new_text": "x"}])).await;
        assert!(message.contains("different spacing"), "{message}");
    }

    #[tokio::test]
    async fn a_failed_edit_leaves_the_file_untouched() {
        let dir = dir_holding("untouched", "a\n\nb\n");
        let _ = edit_error(&dir, json!([{"old_text": "a\n   \nb", "new_text": "x"}])).await;
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

    /// Match-line reporting only examines the first `limit` matches.
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
