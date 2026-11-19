use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::output::first_line;
use crate::path::{display_file, normalize, resolve_path};
use crate::tools::AgentContext;

use super::{Tool, ToolResult};
use refusal::Refusal;

mod matcher;
mod refusal;
mod show;

pub struct EditTool;

#[derive(Debug, Deserialize)]
struct EditInput {
    /// The file to edit. In a batch, the default for an edit that names no path.
    path: Option<String>,
    /// The single-edit form: what to replace.
    old_text: Option<String>,
    /// The single-edit form: what to replace it with.
    new_text: Option<String>,
    /// The batch form: one or more edits, each naming its own path.
    #[serde(default)]
    edits: Vec<Replacement>,
}

#[derive(Debug, Deserialize)]
struct Replacement {
    /// The file this replacement is in, when one call spans several.
    path: Option<String>,
    old_text: String,
    new_text: String,
}

/// A call after its two forms are one: the edits, and the path they default to.

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> String {
        "Replace text in files: one edit as path, old_text and new_text, or \
         several as edits"
            .to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit, relative to the working \
                                    directory unless absolute. In a batch it is the \
                                    default for an edit that names none."
                },
                "old_text": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Non-empty text to replace, copied from the file: \
                                    exact bytes first, then trailing whitespace ignored."
                },
                "new_text": {
                    "type": "string",
                    "description": "Replacement text; empty deletes the match."
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Several edits in one call, each naming its own path: \
                                    one call may span files, and no two edits in the same \
                                    file overlap.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Path to the file this edit is in. Defaults to \
                                                the call's `path`."
                            },
                            "old_text": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Text to replace, copied the same way as above."
                            },
                            "new_text": {
                                "type": "string",
                                "description": "Replacement text; empty deletes the match."
                            }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "anyOf": [
                { "required": ["path", "old_text", "new_text"] },
                { "required": ["edits"] }
            ]
        })
    }

    /// The resolved path, spelled the way every other tool in the trace spells
    /// it — or, for a call that spans files, the first two and a count. A call
    /// whose arguments do not name a file at all says so.
    fn trace_label(&self, arguments: &Value, cwd: &Path) -> Option<String> {
        let labels = call_paths(arguments, cwd)
            .iter()
            .map(|path| first_line(&display_file(cwd, path)))
            .collect::<Vec<_>>();
        Some(match labels.as_slice() {
            [] => "?".to_string(),
            [only] => only.clone(),
            [first, second] => format!("{first}, {second}"),
            [first, second, ..] => format!("{first}, {second} and {} more", labels.len() - 2),
        })
    }

    /// Every file this call writes, resolved as the call itself resolves them. A
    /// batch that spans several holds all of their locks, so two calls
    /// overlapping on any one file still take turns instead of each writing over
    /// the other's read. A call too malformed to name a file takes none, because
    /// it writes none.
    fn lock_paths(&self, arguments: &Value, cwd: &Path) -> Vec<PathBuf> {
        call_paths(arguments, cwd)
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        match edit(input, &ctx.cwd).await {
            Ok(report) => Ok(ToolResult {
                content: report,
                is_error: false,
            }),
            // A refusal is an answer the model can act on, so it travels back the
            // way a success does and only `is_error` says otherwise.
            Err(Fault::Refused(refusal)) => Ok(ToolResult {
                content: refusal.to_string(),
                is_error: true,
            }),
            Err(Fault::Broken(error)) => Err(error),
        }
    }
}

/// Why a step stopped: a [`Refusal`] is an answer the model can act on and returns
/// as a tool result, while a failure is the tool's own problem and returns as the
/// caller's error.
enum Fault {
    Refused(Refusal),
    Broken(anyhow::Error),
}

impl From<Refusal> for Fault {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<anyhow::Error> for Fault {
    fn from(error: anyhow::Error) -> Self {
        Self::Broken(error)
    }
}

/// One call, as the steps it takes: check the arguments, group the edits by the
/// file each one names, read every file, locate each group's anchors in that
/// file's own snapshot, splice each non-overlapping batch, then write the files
/// and report what landed.
async fn edit(input: Value, cwd: &Path) -> Result<String, Fault> {
    let targets = targets(input, cwd)?;

    // Every file is read, matched and spliced before any is written, which is
    // what lets a refusal say nothing was written: a miss in the last file
    // leaves the first ones alone.
    let mut files = Vec::with_capacity(targets.len());
    for target in &targets {
        let original = read_text(&target.path).await?;
        let found = locate_all(&original, &target.edits, &target.path)?;
        let (updated, applied) = splice_all(&original, &target.edits, &found, &target.path)?;
        files.push(Landed {
            path: target.path.clone(),
            original,
            updated,
            applied,
        });
    }

    for (written, file) in files.iter().enumerate() {
        tokio::fs::write(&file.path, &file.updated)
            .await
            .with_context(|| {
                format!(
                    "failed to write edited file: {} ({} of {} files were written \
                     before it)",
                    file.path.display(),
                    written,
                    files.len()
                )
            })?;
    }

    Ok(report(&files))
}

/// Every file the call writes, in the order it names them, without repeats —
/// resolved and normalized, so two spellings of one file are one key. The one
/// reading of a call's paths: the trace spells them and the batch locks them.
/// A call that does not decode names no file, and writes none.
fn call_paths(arguments: &Value, cwd: &Path) -> Vec<PathBuf> {
    targets(arguments.clone(), cwd)
        .unwrap_or_default()
        .into_iter()
        .map(|target| target.path)
        .collect()
}

/// One file's share of the call: the path its edits name, and each edit at the
/// position the model sent it in, so a refusal names the edit to look at.
struct Target {
    path: PathBuf,
    edits: Vec<(usize, Replacement)>,
}

/// The call as the work it names: one target per file, in the order the files
/// first appear. This is the one reading of a call — both forms desugar here,
/// the paths resolve here — so what execution writes, what the trace spells and
/// what the batch locks cannot disagree.
///
/// A group is one snapshot: its replacements are matched together, and two of
/// them may not overlap. An empty anchor is refused first, because it would
/// otherwise reach the matcher, where it matches once per byte and is reported
/// as ambiguous rather than wrong.
fn targets(input: Value, cwd: &Path) -> Result<Vec<Target>, Refusal> {
    let input: EditInput =
        serde_json::from_value(input).map_err(|error| Refusal::bad_input(&error.to_string()))?;

    // The two forms as one. One edit is `path`, `old_text` and `new_text`; a
    // batch is `edits`, each entry carrying the path it belongs to, with the
    // call's own path as their default. Sending both is not a bigger call, it is
    // two answers to one question, so it is refused rather than guessed at.
    let flat = input.old_text.is_some() || input.new_text.is_some();
    if flat && !input.edits.is_empty() {
        return Err(Refusal::both_forms());
    }
    let edits = match (input.old_text, input.new_text) {
        (Some(old_text), Some(new_text)) => {
            if named(&input.path).is_none() {
                return Err(Refusal::flat_needs_path());
            }
            vec![Replacement {
                path: None,
                old_text,
                new_text,
            }]
        }
        (None, None) if !input.edits.is_empty() => input.edits,
        (None, None) => return Err(Refusal::no_edit()),
        _ => return Err(Refusal::half_edit()),
    };
    if let Some(index) = edits.iter().position(|edit| edit.old_text.is_empty()) {
        // The single form has no batch to give a position in, so its refusal
        // names the field instead of a member of an array it never sent.
        return Err(if flat {
            Refusal::empty_old_text()
        } else {
            Refusal::empty_anchor(index)
        });
    }

    let mut targets: Vec<Target> = Vec::new();
    for (index, edit) in edits.into_iter().enumerate() {
        let raw = named(&edit.path)
            .or_else(|| named(&input.path))
            .ok_or_else(|| Refusal::no_path(index))?;
        let path = normalize(&resolve_path(cwd, raw));
        match targets.iter_mut().find(|target| target.path == path) {
            Some(target) => target.edits.push((index, edit)),
            None => targets.push(Target {
                path,
                edits: vec![(index, edit)],
            }),
        }
    }
    Ok(targets)
}

/// A path the call named, or nothing: an empty or blank string names no file, and
/// a directory is not a file to read.
fn named(path: &Option<String>) -> Option<&str> {
    path.as_deref().filter(|raw| !raw.trim().is_empty())
}

/// The file's contents, with a missing or undecodable file refused and anything
/// else a failure. One call, not an existence check beside it: two answers to one
/// question leave a window in which the file can go away.
async fn read_text(path: &Path) -> Result<String, Fault> {
    tokio::fs::read_to_string(path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Fault::Refused(Refusal::not_found(path)),
            std::io::ErrorKind::InvalidData => Fault::Refused(Refusal::not_text(path)),
            _ => Fault::Broken(
                anyhow::Error::new(error)
                    .context(format!("failed to read file: {}", path.display())),
            ),
        })
}

/// An anchor that landed: the matcher that found it and the bytes it covers.
#[derive(Debug, Clone)]
struct Found {
    matcher: &'static str,
    span: matcher::Span,
}

impl Found {
    /// Whether the model already knows what landed here: an exact anchor *is* the
    /// bytes it sent, so a receipt would only repeat them, while a fallback
    /// matched something it did not send.
    fn is_fallback(&self) -> bool {
        self.matcher != matcher::EXACT
    }
}

/// Each anchor of this file located in the snapshot, or the first refusal. Every
/// edit matches the original bytes — a batch cannot read its own writes — and
/// one refusal is final: nothing is written unless every anchor names one place.
fn locate_all(
    content: &str,
    edits: &[(usize, Replacement)],
    path: &Path,
) -> Result<Vec<Found>, Refusal> {
    edits
        .iter()
        .map(
            |(index, edit)| match matcher::locate(content, &edit.old_text) {
                matcher::Located::Found { matcher, span } => Ok(Found { matcher, span }),
                matcher::Located::Ambiguous {
                    matcher: matched,
                    places,
                    lines,
                } => {
                    let shown = show::candidates(content, &lines);
                    Err(Refusal::ambiguous(
                        *index,
                        path,
                        matched,
                        places,
                        &shown.lines,
                        shown.hidden,
                    ))
                }
                matcher::Located::Miss => {
                    let hint = show::hint(content, &matcher::probes(content, &edit.old_text));
                    Err(Refusal::miss(*index, path, &hint))
                }
            },
        )
        .collect()
}

/// One replacement that landed, as the receipt needs it: how the anchor was
/// found, the bytes it covered, the text written there, and the line it starts
/// on now.
#[derive(Debug)]
struct Applied<'a> {
    found: Found,
    text: &'a str,
    line: usize,
}

/// One file after its share of the batch landed: the bytes it held, the bytes it
/// holds now, and the receipts for what landed where.
struct Landed<'a> {
    path: PathBuf,
    original: String,
    updated: String,
    applied: Vec<Applied<'a>>,
}

/// The replacements spliced into this file's snapshot, or the first colliding
/// pair — named by the positions the model sent them in, not by position in the
/// group, which is not a number it ever wrote.
fn splice_all<'a>(
    content: &str,
    edits: &'a [(usize, Replacement)],
    found: &[Found],
    path: &Path,
) -> Result<(String, Vec<Applied<'a>>), Refusal> {
    let replacements = edits
        .iter()
        .zip(found)
        .map(|((_, edit), found)| (found.span.clone(), edit.new_text.as_str()))
        .collect::<Vec<_>>();
    let (updated, landed) = matcher::splice(content, &replacements).map_err(|overlap| {
        Refusal::overlap(edits[overlap.earlier].0, edits[overlap.later].0, path)
    })?;

    let applied = edits
        .iter()
        .zip(found)
        .zip(landed)
        .map(|(((_, edit), found), line)| Applied {
            found: found.clone(),
            text: edit.new_text.as_str(),
            line,
        })
        .collect();
    Ok((updated, applied))
}

/// What the tool did, a block per file: a line for each exact edit, the region for
/// a fallback. Bounded in lines rather than edits because the result is re-sent
/// every later turn; what does not fit is counted, so the model still learns the
/// rest was applied. A call that changed one file says exactly what it said
/// before it could change several.
fn report(files: &[Landed]) -> String {
    /// The most lines one result spends on receipts, across every file in it.
    const LINES: usize = 24;

    let mut report = String::new();
    let mut spent = 0;
    for file in files {
        let count = file.applied.len();
        let noun = if count == 1 { "edit" } else { "edits" };
        if !report.is_empty() {
            report.push('\n');
        }
        report.push_str(&format!(
            "applied {count} {noun} to {}",
            file.path.display()
        ));
        let mut shown = 0;
        for patch in &file.applied {
            let receipt = if patch.found.is_fallback() {
                format!(
                    "{} at line {}:\n{}",
                    patch.found.matcher,
                    patch.line,
                    show::change(&file.original, &patch.found.span, patch.text, patch.line)
                )
            } else {
                format!("{} at line {}", patch.found.matcher, patch.line)
            };
            let cost = receipt.lines().count();
            if spent + cost > LINES {
                break;
            }
            spent += cost;
            shown += 1;
            report.push('\n');
            report.push_str(&receipt);
        }
        if shown < count {
            let rest = count - shown;
            report.push_str(&format!(
                "\n… and {rest} more {}",
                if rest == 1 { "region" } else { "regions" }
            ));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temp directory holding `file.txt`, so the tool is exercised the way
    /// the agent calls it — through `execute`, not through the matcher.
    fn dir_holding(name: &str, body: &str) -> PathBuf {
        let dir = crate::config::test_dir(name, None);
        std::fs::write(dir.join("file.txt"), body).unwrap();
        dir
    }

    fn body(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("file.txt")).unwrap()
    }

    /// A temp directory holding each of `files`, for the calls that span more
    /// than one.
    fn dir_with(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = crate::config::test_dir(name, None);
        for (file, text) in files {
            std::fs::write(dir.join(file), text).unwrap();
        }
        dir
    }

    fn text_of(dir: &Path, file: &str) -> String {
        std::fs::read_to_string(dir.join(file)).unwrap()
    }

    /// One file's landed batch, for the report tests, which care about the shape
    /// of the answer rather than how the bytes got there.
    fn landed(path: &str, applied: Vec<Applied<'static>>) -> Landed<'static> {
        Landed {
            path: PathBuf::from(path),
            original: "a\n".to_string(),
            updated: String::new(),
            applied,
        }
    }

    /// One exact replacement that landed on `line`.
    fn exact(line: usize) -> Applied<'static> {
        Applied {
            found: Found {
                matcher: matcher::EXACT,
                span: 0..1,
            },
            text: "x",
            line,
        }
    }

    /// Any arguments at all, answered: for the calls whose arguments are what is
    /// under test, rather than the text they change.
    async fn call(cwd: &Path, arguments: Value) -> ToolResult {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        EditTool
            .execute(arguments, &ctx)
            .await
            .expect("the call must be answered")
    }

    async fn edit(cwd: &Path, edits: Value) -> String {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        EditTool
            .execute(json!({ "path": "file.txt", "edits": edits }), &ctx)
            .await
            .expect("the edit must succeed")
            .content
    }

    /// A refusal is an answered call, not a failed one: the result carries the
    /// instruction and `is_error` is the only thing that differs, so the batch
    /// does not prefix it with a report about the machine.
    async fn refusal(cwd: &Path, edits: Value) -> String {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        let result = EditTool
            .execute(json!({ "path": "file.txt", "edits": edits }), &ctx)
            .await
            .expect("a refusal must be answered, not returned as an error");
        assert!(
            result.is_error,
            "a refusal must be marked: {}",
            result.content
        );
        result.content
    }

    /// Independent replacements are all matched against the same original and
    /// written together, so one tool call can change several regions safely.
    #[tokio::test]
    async fn multiple_disjoint_edits_apply_together() {
        let dir = dir_holding("multiple", "alpha\nbeta\ngamma\n");
        edit(
            &dir,
            json!([
                {"old_text": "alpha", "new_text": "first"},
                {"old_text": "gamma", "new_text": "last"}
            ]),
        )
        .await;

        assert_eq!(body(&dir), "first\nbeta\nlast\n");
    }

    /// One call carries a change through every file it names: each file's batch is
    /// matched against its own original, and all of them are written together.
    #[tokio::test]
    async fn one_call_edits_several_files() {
        let dir = dir_with(
            "multi-file",
            &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")],
        );
        let report = call(
            &dir,
            json!({
                "edits": [
                    {"path": "a.rs", "old_text": "fn a() {}", "new_text": "fn a() { 1; }"},
                    {"path": "b.rs", "old_text": "fn b() {}", "new_text": "fn b() { 2; }"}
                ]
            }),
        )
        .await;

        assert!(!report.is_error, "{}", report.content);
        assert_eq!(text_of(&dir, "a.rs"), "fn a() { 1; }\n");
        assert_eq!(text_of(&dir, "b.rs"), "fn b() { 2; }\n");
        assert!(
            report.content.contains("applied 1 edit to"),
            "{}",
            report.content
        );
    }

    /// A call's own `path` is the default for every edit that names none, which
    /// keeps the one-file call exactly as short as it was.
    #[tokio::test]
    async fn the_call_path_is_the_default_for_an_edit() {
        let dir = dir_with("default-path", &[("a.rs", "one\n"), ("b.rs", "two\n")]);
        call(
            &dir,
            json!({
                "path": "a.rs",
                "edits": [
                    {"old_text": "one", "new_text": "first"},
                    {"path": "b.rs", "old_text": "two", "new_text": "second"}
                ]
            }),
        )
        .await;

        assert_eq!(text_of(&dir, "a.rs"), "first\n");
        assert_eq!(text_of(&dir, "b.rs"), "second\n");
    }

    /// A refusal in one file writes none of the others: "nothing was written" is
    /// true of the whole call, not just of the file that missed.
    #[tokio::test]
    async fn a_refusal_in_one_file_leaves_every_file_alone() {
        let dir = dir_with(
            "multi-file-refusal",
            &[("a.rs", "one\n"), ("b.rs", "two\n")],
        );
        let refusal = call(
            &dir,
            json!({
                "edits": [
                    {"path": "a.rs", "old_text": "one", "new_text": "first"},
                    {"path": "b.rs", "old_text": "missing", "new_text": "second"}
                ]
            }),
        )
        .await;

        assert!(refusal.is_error, "{}", refusal.content);
        assert!(refusal.content.contains("edits[1]"), "{}", refusal.content);
        assert_eq!(text_of(&dir, "a.rs"), "one\n");
        assert_eq!(text_of(&dir, "b.rs"), "two\n");
    }

    /// Two spellings of one file are one snapshot: the second edit's line number
    /// is measured in the file both of them saw, not in a second copy of it.
    #[tokio::test]
    async fn one_file_named_twice_is_one_snapshot() {
        let dir = dir_with("one-file-twice", &[("a.txt", "one\ntwo\n")]);
        let report = call(
            &dir,
            json!({"edits": [
                {"path": "a.txt", "old_text": "one", "new_text": "1"},
                {"path": "./a.txt", "old_text": "two", "new_text": "2"}
            ]}),
        )
        .await;

        assert!(!report.is_error, "{}", report.content);
        assert_eq!(text_of(&dir, "a.txt"), "1\n2\n");
        assert!(
            report.content.contains("exact at line 1"),
            "{}",
            report.content
        );
        assert!(
            report.content.contains("exact at line 2"),
            "{}",
            report.content
        );
    }

    /// The same anchor twice names one place, so the batch is refused as an
    /// overlap rather than applying whichever came first.
    #[tokio::test]
    async fn the_same_anchor_twice_is_refused() {
        let dir = dir_with("same-anchor-twice", &[("a.txt", "one\n")]);
        let refusal = call(
            &dir,
            json!({"edits": [
                {"path": "a.txt", "old_text": "one", "new_text": "x"},
                {"path": "a.txt", "old_text": "one", "new_text": "y"}
            ]}),
        )
        .await;

        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal.content.contains("edits[0] and edits[1]"),
            "{}",
            refusal.content
        );
        assert_eq!(text_of(&dir, "a.txt"), "one\n");
    }

    /// A fallback receipt quotes the file it landed in: in a call spanning files,
    /// that is that file's own bytes, not the first file's.
    #[tokio::test]
    async fn a_fallback_in_a_later_file_quotes_that_file() {
        let dir = dir_with(
            "fallback-later-file",
            &[("a.txt", "one\n"), ("b.txt", "alpha   \nbeta\n")],
        );
        let report = call(
            &dir,
            json!({"edits": [
                {"path": "a.txt", "old_text": "one", "new_text": "1"},
                {"path": "b.txt", "old_text": "alpha\nbeta", "new_text": "ALPHA\nBETA"}
            ]}),
        )
        .await;

        assert!(!report.is_error, "{}", report.content);
        assert!(
            report.content.contains("   1 - alpha   "),
            "{}",
            report.content
        );
        assert!(
            report.content.contains("   1 + ALPHA"),
            "{}",
            report.content
        );
    }

    /// An edit that names no file, in a call that names no default, is refused by
    /// position before anything is read.
    #[tokio::test]
    async fn an_edit_with_no_path_anywhere_is_refused() {
        let dir = dir_holding("no-path", "alpha\n");
        let refusal = call(
            &dir,
            json!({"edits": [{"old_text": "alpha", "new_text": "beta"}]}),
        )
        .await;

        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal.content.contains("edits[0] names no path"),
            "{}",
            refusal.content
        );
        assert_eq!(body(&dir), "alpha\n");
    }

    /// Applying from the end preserves original offsets when an earlier edit
    /// changes length, and an empty replacement deletes text in the same batch.
    #[tokio::test]
    async fn a_batch_handles_length_changes_and_deletions_together() {
        let dir = dir_holding("batch-offsets-delete", "a middle z\n");
        edit(
            &dir,
            json!([
                {"old_text": "a", "new_text": "long beginning"},
                {"old_text": " middle", "new_text": ""}
            ]),
        )
        .await;

        assert_eq!(body(&dir), "long beginning z\n");
    }

    /// Non-overlapping ranges are measured in the original file, even when one
    /// replacement inserts text that another edit also names.
    #[tokio::test]
    async fn replacements_do_not_match_text_inserted_by_another_edit() {
        let dir = dir_holding("batch-original", "first\nsecond\n");
        edit(
            &dir,
            json!([
                {"old_text": "first", "new_text": "second"},
                {"old_text": "second", "new_text": "last"}
            ]),
        )
        .await;

        assert_eq!(body(&dir), "second\nlast\n");
    }

    #[tokio::test]
    async fn a_unique_edit_rewrites_the_file() {
        let dir = dir_holding("unique", "alpha\nbeta\n");
        edit(&dir, json!([{ "old_text": "beta", "new_text": "gamma" }])).await;

        assert_eq!(body(&dir), "alpha\ngamma\n");
    }

    /// An exact anchor is the text the model sent, so the receipt is the line it
    /// landed on and nothing else: the file is not quoted back at the model that
    /// just quoted it.
    #[tokio::test]
    async fn an_exact_edit_is_reported_as_one_line() {
        let dir = dir_holding("report-exact", "alpha\nbeta\ngamma\n");
        let report = edit(
            &dir,
            json!([{ "old_text": "beta", "new_text": "replaced" }]),
        )
        .await;

        assert_eq!(
            report,
            format!(
                "applied 1 edit to {}\nexact at line 2",
                dir.join("file.txt").display()
            )
        );
    }

    /// A fallback is the one case worth quoting: it matched text the model did
    /// not send, so the receipt shows the lines the file really held.
    #[tokio::test]
    async fn a_fallback_reports_the_region_it_changed() {
        let dir = dir_holding("report-fallback", "alpha   \nbeta\n");
        let report = edit(
            &dir,
            json!([{ "old_text": "alpha\nbeta", "new_text": "ALPHA\nBETA" }]),
        )
        .await;

        assert!(report.contains("trailing whitespace ignored"), "{report}");
        assert!(report.contains("   1 - alpha   "), "{report}");
        assert!(report.contains("   1 + ALPHA"), "{report}");
    }

    /// A trailing space in the file is not a wrong anchor: the fallback matcher
    /// lands the edit and the result says which one did.
    #[tokio::test]
    async fn trailing_whitespace_in_the_file_still_matches() {
        let dir = dir_holding("trailing", "alpha   \nbeta\n");
        let report = edit(
            &dir,
            json!([{ "old_text": "alpha\nbeta", "new_text": "alpha\nbeta\ngamma" }]),
        )
        .await;

        assert_eq!(body(&dir), "alpha\nbeta\ngamma\n");
        assert!(report.contains("trailing whitespace ignored"), "{report}");
    }

    /// A file that spells its lines CRLF is edited through an anchor copied from a
    /// read, which carries only `\n`: the anchor is respelled, the replacement
    /// written back the same way, and the report says `exact`, not whitespace.
    #[tokio::test]
    async fn a_crlf_file_is_edited_and_stays_crlf() {
        let dir = dir_holding("crlf", "alpha\r\nbeta\r\ngamma\r\n");
        let report = edit(&dir, json!([{ "old_text": "beta", "new_text": "B1\nB2" }])).await;

        assert_eq!(body(&dir), "alpha\r\nB1\r\nB2\r\ngamma\r\n");
        assert!(report.contains("exact at line 2"), "{report}");
    }

    /// An anchor whose indentation is off is refused, but the refusal names the
    /// normalization that would have carried it, so the retry is a rewrite
    /// rather than a re-read.
    #[tokio::test]
    async fn a_miss_names_the_normalization_that_would_have_matched() {
        let dir = dir_holding("miss-indent", "fn main() {\n    let x = 1;\n}\n");
        let error = refusal(
            &dir,
            json!([{ "old_text": "  fn main() {\n    let x = 1;\n}", "new_text": "x" }]),
        )
        .await;

        assert!(error.contains("indentation ignored"), "{error}");
    }

    /// A rewrap is the miss a model makes when it edits its own prose: every word
    /// is there and only the line ends moved. The refusal names the row that would
    /// have carried it, on the lines to copy, so the retry is a rewrite and not a
    /// re-read — and the file is left alone, as with any other miss.
    #[tokio::test]
    async fn a_miss_names_the_line_breaks_a_rewrap_moved() {
        let content = "such flag. The answer is printed, then recorded as\nreference material.\n";
        let dir = dir_holding("miss-breaks", content);
        let error = refusal(
            &dir,
            json!([{
                "old_text": "The answer is printed, then recorded as reference\nmaterial.",
                "new_text": "x"
            }]),
        )
        .await;

        assert!(error.contains("line breaks ignored"), "{error}");
        assert!(error.contains("   2 | reference material."), "{error}");
        assert_eq!(body(&dir), content);
    }

    #[tokio::test]
    async fn a_missing_anchor_leaves_the_file_untouched() {
        let dir = dir_holding("untouched", "alpha\nbeta\n");
        let error = refusal(&dir, json!([{ "old_text": "omega", "new_text": "x" }])).await;

        assert!(error.contains("nothing was written"), "{error}");
        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    /// An invalid member rejects the whole batch before any replacement is
    /// written, even when an earlier member matched uniquely.
    #[tokio::test]
    async fn a_missing_match_rejects_the_whole_edit_batch() {
        let dir = dir_holding("batch-missing", "alpha\nbeta\n");
        let error = refusal(
            &dir,
            json!([
                {"old_text": "alpha", "new_text": "first"},
                {"old_text": "missing", "new_text": "last"}
            ]),
        )
        .await;

        assert!(error.contains("edits[1]"), "{error}");
        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    /// Ambiguity is refused rather than resolved by array order, and the
    /// refusal names both candidate lines.
    #[tokio::test]
    async fn an_ambiguous_edit_names_every_candidate_line() {
        let dir = dir_holding("ambiguous", "same\nother\nsame\n");
        let error = refusal(&dir, json!([{ "old_text": "same", "new_text": "x" }])).await;

        assert!(error.contains("   1 | same"), "{error}");
        assert!(error.contains("   3 | same"), "{error}");
        assert_eq!(body(&dir), "same\nother\nsame\n");
    }

    /// A substring with overlapping starting positions is ambiguous even when
    /// non-overlapping iteration would report only one occurrence.
    #[tokio::test]
    async fn a_self_overlapping_anchor_is_refused_without_writing() {
        let dir = dir_holding("self-overlap", "aaa\n");
        let error = refusal(&dir, json!([{ "old_text": "aa", "new_text": "x" }])).await;

        assert!(error.contains("2 places"), "{error}");
        assert_eq!(body(&dir), "aaa\n");
    }

    /// Match ranges that overlap in the original are rejected rather than
    /// applying replacements whose meaning depends on order.
    #[tokio::test]
    async fn overlapping_edit_ranges_are_rejected_without_writing() {
        let dir = dir_holding("batch-overlap", "abcdef\n");
        let error = refusal(
            &dir,
            json!([
                {"old_text": "abc", "new_text": "left"},
                {"old_text": "cde", "new_text": "right"}
            ]),
        )
        .await;

        assert!(error.contains("overlap"), "{error}");
        assert_eq!(body(&dir), "abcdef\n");
    }

    /// The colliding pair is named by the position the model sent it in, which
    /// is what tells it which two edits to combine — not by its position in the
    /// group, which is a number the model never wrote.
    #[test]
    fn an_overlap_names_the_two_edits_to_combine() {
        let mut edits = ["abc", "cde"]
            .map(|old_text| Replacement {
                path: None,
                old_text: old_text.to_string(),
                new_text: "x".to_string(),
            })
            .into_iter();
        let group = [(1, edits.next().unwrap()), (2, edits.next().unwrap())];
        let found = [
            Found {
                matcher: matcher::EXACT,
                span: 0..3,
            },
            Found {
                matcher: matcher::EXACT,
                span: 2..5,
            },
        ];

        let refusal = splice_all("abcdef\n", &group, &found, Path::new("/repo/file.rs"))
            .expect_err("the spans overlap");

        assert!(
            refusal
                .to_string()
                .contains("edits[1] and edits[2] overlap"),
            "{refusal}"
        );
    }

    /// A call with neither form is refused before reading or writing a path:
    /// an empty `edits` is not a batch of zero, it is a call with no edit.
    #[tokio::test]
    async fn a_call_with_no_edit_is_refused() {
        let dir = dir_holding("batch-empty", "content\n");
        let error = refusal(&dir, json!([])).await;

        assert!(error.contains("carries no edit"), "{error}");
        assert_eq!(body(&dir), "content\n");
    }

    /// The core shape: one edit, three fields. No array, no nesting, nothing the
    /// single most common call has to spell out twice.
    #[tokio::test]
    async fn one_edit_is_path_old_and_new() {
        let dir = dir_holding("flat", "alpha\nbeta\n");
        let report = call(
            &dir,
            json!({"path": "file.txt", "old_text": "beta", "new_text": "gamma"}),
        )
        .await;

        assert!(!report.is_error, "{}", report.content);
        assert_eq!(body(&dir), "alpha\ngamma\n");
        assert!(
            report.content.contains("applied 1 edit to"),
            "{}",
            report.content
        );
    }

    /// The batch is the same edit repeated, so one edit sent either way must land
    /// the same bytes and read the same in the receipt.
    #[tokio::test]
    async fn the_two_forms_do_the_same_thing() {
        let flat = dir_holding("flat-vs-batch", "alpha\nbeta\n");
        let batch = dir_holding("flat-vs-batch-two", "alpha\nbeta\n");

        let one = call(
            &flat,
            json!({"path": "file.txt", "old_text": "beta", "new_text": "gamma"}),
        )
        .await;
        let other = call(
            &batch,
            json!({"path": "file.txt", "edits": [
                {"old_text": "beta", "new_text": "gamma"}
            ]}),
        )
        .await;

        assert_eq!(body(&flat), body(&batch));
        assert_eq!(
            one.content.replace(&flat.display().to_string(), "DIR"),
            other.content.replace(&batch.display().to_string(), "DIR")
        );
    }

    /// Both forms in one call is two answers to one question: refused by name,
    /// rather than resolved by a precedence rule the model cannot see.
    #[tokio::test]
    async fn a_call_with_both_forms_is_refused() {
        let dir = dir_holding("both-forms", "alpha\n");
        let error = call(
            &dir,
            json!({
                "path": "file.txt",
                "old_text": "alpha",
                "new_text": "beta",
                "edits": [{"old_text": "alpha", "new_text": "gamma"}]
            }),
        )
        .await;

        assert!(error.is_error, "{}", error.content);
        assert!(error.content.contains("not both"), "{}", error.content);
        assert_eq!(body(&dir), "alpha\n");
    }

    /// Half an edit names nothing to write, and the refusal says which halves a
    /// single edit has.
    #[tokio::test]
    async fn a_call_with_half_an_edit_is_refused() {
        let dir = dir_holding("half-edit", "alpha\n");
        let error = call(&dir, json!({"path": "file.txt", "old_text": "alpha"})).await;

        assert!(error.is_error, "{}", error.content);
        assert!(
            error.content.contains("needs both old_text and new_text"),
            "{}",
            error.content
        );
        assert_eq!(body(&dir), "alpha\n");
    }

    /// The single form has no array to put a path in, so a missing `path` is the
    /// edit naming no file at all.
    #[tokio::test]
    async fn a_flat_edit_with_no_path_is_refused() {
        let dir = dir_holding("flat-no-path", "alpha\n");
        let error = call(&dir, json!({"old_text": "alpha", "new_text": "beta"})).await;

        assert!(error.is_error, "{}", error.content);
        assert!(error.content.contains("needs `path`"), "{}", error.content);
        assert_eq!(body(&dir), "alpha\n");
    }

    #[tokio::test]
    async fn an_empty_old_text_is_refused_without_changing_the_file() {
        let dir = dir_holding("empty-old-text", "alpha\nbeta\n");
        let error = refusal(&dir, json!([{ "old_text": "", "new_text": "x" }])).await;

        assert!(error.contains("old_text must not be empty"), "{error}");
        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    /// The single form has no array to give a position in, so its refusal names
    /// the field rather than `edits[0]`, which the model never sent.
    #[tokio::test]
    async fn an_empty_anchor_in_the_single_form_names_the_field() {
        let dir = dir_holding("empty-old-text-flat", "alpha\n");
        let error = call(
            &dir,
            json!({"path": "file.txt", "old_text": "", "new_text": "x"}),
        )
        .await;

        assert!(error.is_error, "{}", error.content);
        assert!(
            error.content.contains("old_text must not be empty"),
            "{}",
            error.content
        );
        assert!(!error.content.contains("edits[0]"), "{}", error.content);
        assert_eq!(body(&dir), "alpha\n");
    }

    /// A blank path names no file, so an edit carrying one lands in the call's
    /// own path instead of resolving the blank against the working directory and
    /// failing to read a directory.
    #[tokio::test]
    async fn a_blank_path_falls_back_to_the_call_path() {
        let dir = dir_holding("blank-path", "alpha\n");
        let report = call(
            &dir,
            json!({
                "path": "file.txt",
                "edits": [{"path": "", "old_text": "alpha", "new_text": "beta"}]
            }),
        )
        .await;

        assert!(!report.is_error, "{}", report.content);
        assert_eq!(body(&dir), "beta\n");
    }

    /// The same is true of the call's own path in the single form: blank is not a
    /// file, so the edit names none.
    #[tokio::test]
    async fn a_blank_call_path_is_refused() {
        let dir = dir_holding("blank-call-path", "alpha\n");
        let error = call(
            &dir,
            json!({"path": "  ", "old_text": "alpha", "new_text": "beta"}),
        )
        .await;

        assert!(error.is_error, "{}", error.content);
        assert!(error.content.contains("needs `path`"), "{}", error.content);
        assert_eq!(body(&dir), "alpha\n");
    }

    /// A path that is not there is named as such, rather than as a read failure
    /// whose io message the model has to decode.
    #[tokio::test]
    async fn a_missing_file_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-missing-file", None);
        let error = refusal(&dir, json!([{ "old_text": "a", "new_text": "b" }])).await;

        assert!(error.contains("file does not exist"), "{error}");
    }

    /// An anchor cannot be found in bytes the tool cannot decode, so the refusal
    /// says what the file is instead of reporting a failed read.
    #[tokio::test]
    async fn a_file_that_is_not_text_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-binary", None);
        std::fs::write(dir.join("file.txt"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let error = refusal(&dir, json!([{ "old_text": "a", "new_text": "b" }])).await;

        assert!(error.contains("not a UTF-8 text file"), "{error}");
    }

    /// A path the tool cannot read for a reason it cannot explain — a directory
    /// where a file should be — is a failure, not a refusal: it returns as an
    /// error, so the trace says the tool broke.
    #[tokio::test]
    async fn an_unreadable_path_fails_instead_of_refusing() {
        let dir = crate::config::test_dir("edit-unreadable", None);
        std::fs::create_dir(dir.join("file.txt")).unwrap();
        let ctx = AgentContext { cwd: dir.clone() };

        let error = EditTool
            .execute(
                json!({ "path": "file.txt", "edits": [{ "old_text": "a", "new_text": "b" }] }),
                &ctx,
            )
            .await
            .expect_err("a path that cannot be read is a failure, not a refusal");

        assert!(
            format!("{error:#}").contains("failed to read file"),
            "{error:#}"
        );
    }

    /// Arguments that do not decode are a refusal like any other: the model is
    /// told the shape it owed, not shown a deserializer's complaint alone.
    #[tokio::test]
    async fn malformed_arguments_are_refused_with_the_shape_expected() {
        let dir = dir_holding("bad-input", "content\n");
        let error = refusal(&dir, json!([{ "old_text": "content" }])).await;

        assert!(error.contains("invalid edit input"), "{error}");
        assert!(error.contains("new_text"), "{error}");
        assert_eq!(body(&dir), "content\n");
    }

    #[test]
    fn the_schema_requires_a_nonempty_old_text() {
        let schema = EditTool.schema();
        assert_eq!(schema["properties"]["edits"]["minItems"], 1);
        assert_eq!(
            schema["properties"]["edits"]["items"]["properties"]["old_text"]["minLength"],
            1
        );
        assert_eq!(schema["properties"]["old_text"]["minLength"], 1);
        assert_eq!(
            schema["properties"]["edits"]["items"]["required"],
            json!(["old_text", "new_text"])
        );
    }

    /// The schema states the two forms as the two required sets it accepts: one
    /// edit's three fields, or `edits`. An edit in a batch carries its own path,
    /// which is what lets one call span files.
    #[test]
    fn the_schema_states_both_forms() {
        let schema = EditTool.schema();
        let edit = &schema["properties"]["edits"]["items"];

        assert_eq!(
            schema["anyOf"],
            json!([
                {"required": ["path", "old_text", "new_text"]},
                {"required": ["edits"]}
            ])
        );
        assert_eq!(edit["properties"]["path"]["type"], "string");
        assert!(
            schema["required"].is_null(),
            "the forms are stated by anyOf, not by one required set: {schema}"
        );
    }

    /// The trace names the files a call spans once each, by first appearance, and
    /// counts the rest — a label is one line, however many files it covers.
    #[test]
    fn the_trace_names_every_file_once() {
        let label = EditTool.trace_label(
            &json!({
                "edits": [
                    {"path": "b.rs", "old_text": "b", "new_text": "B"},
                    {"path": "a.rs", "old_text": "a", "new_text": "A"},
                    {"path": "./b.rs", "old_text": "c", "new_text": "C"},
                    {"path": "c.rs", "old_text": "c", "new_text": "C"}
                ]
            }),
            Path::new("/repo"),
        );

        assert_eq!(label, Some("b.rs, a.rs and 1 more".to_string()));
    }

    /// A batch larger than the receipt budget is reported up to the budget and
    /// counted beyond it, because the result is re-sent on every later turn.
    #[test]
    fn a_report_stops_at_the_line_budget_and_counts_the_rest() {
        let applied = (1..=30).map(exact).collect::<Vec<_>>();
        let report = report(&[landed("/repo/file.rs", applied)]);

        assert_eq!(report.matches("exact at line").count(), 24);
        assert!(report.contains("… and 6 more regions"), "{report}");
    }

    #[test]
    fn one_edit_is_not_reported_as_edits() {
        let report = report(&[landed("/repo/file.rs", vec![exact(1)])]);

        assert!(
            report.starts_with("applied 1 edit to /repo/file.rs"),
            "{report}"
        );
    }

    /// One region left out is a region, not "1 more regions": the count is the
    /// sentence the model reads.
    #[test]
    fn a_report_pluralises_the_one_region_it_left_out() {
        let applied = (1..=25).map(exact).collect::<Vec<_>>();
        let report = report(&[landed("/repo/file.rs", applied)]);

        assert!(report.ends_with("… and 1 more region"), "{report}");
    }

    /// A call that changed two files reports both, one block each: the count is
    /// per file, so "applied 1 edit" twice is two files, not a batch of two.
    #[test]
    fn a_report_names_every_file_it_changed() {
        let report = report(&[
            landed("/repo/a.rs", vec![exact(1)]),
            landed("/repo/b.rs", vec![exact(1), exact(2)]),
        ]);

        assert!(report.contains("applied 1 edit to /repo/a.rs"), "{report}");
        assert!(report.contains("applied 2 edits to /repo/b.rs"), "{report}");
    }
}
