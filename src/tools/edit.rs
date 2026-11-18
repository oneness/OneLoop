use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::path::resolve_path;
use crate::tools::{AgentContext, PATH_ARGUMENT, path_trace_label};

use super::{Tool, ToolResult};
use refusal::Refusal;

mod matcher;
mod refusal;
mod show;

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
        "Edit a file by replacing one or more blocks of text".to_string()
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
                    "description": "One or more replacements, each matching one place in \
                                    the original file and none of them overlapping another.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Non-empty text to replace, copied from the \
                                                file: exact bytes first, then trailing \
                                                whitespace ignored."
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
            "required": ["path", "edits"]
        })
    }

    /// The resolved path, spelled the way every other tool in the trace spells
    /// it. A call that arrived without one says so.
    fn trace_label(&self, arguments: &Value, cwd: &Path) -> Option<String> {
        path_trace_label(arguments, cwd)
    }

    /// The batch holds this file's lock for the whole call, which is what makes
    /// one snapshot safe to rewrite. A caller reaching `execute` without it — the
    /// registry's own is one — does not get that guarantee.
    fn lock_path(&self) -> Option<&'static str> {
        Some(PATH_ARGUMENT)
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

/// One call, as the steps it takes: check the arguments, read the file, locate
/// every anchor in that one snapshot, splice a non-overlapping batch, write it,
/// and report what landed.
async fn edit(input: Value, cwd: &Path) -> Result<String, Fault> {
    let input = parse_input(input)?;
    let path = resolve_path(cwd, &input.path);

    let content = read_text(&path).await?;
    let found = locate_all(&content, &input.edits, &path)?;
    let (updated, applied) = splice_all(&content, &input.edits, &found, &path)?;

    tokio::fs::write(&path, &updated)
        .await
        .with_context(|| format!("failed to write edited file: {}", path.display()))?;

    Ok(report(&path, &content, &applied))
}

/// The arguments, checked for what a provider may ignore in the schema: an empty
/// anchor would otherwise reach the matcher, where it matches once per byte and
/// is reported as ambiguous rather than wrong.
fn parse_input(input: Value) -> Result<EditInput, Refusal> {
    let input: EditInput =
        serde_json::from_value(input).map_err(|error| Refusal::bad_input(&error.to_string()))?;
    if input.edits.is_empty() {
        return Err(Refusal::empty_batch());
    }
    if let Some(index) = input.edits.iter().position(|edit| edit.old_text.is_empty()) {
        return Err(Refusal::empty_anchor(index));
    }
    Ok(input)
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

/// Each anchor located in the snapshot, or the first refusal. Every edit matches
/// the original bytes — a batch cannot read its own writes — and one refusal is
/// final: nothing is written unless every anchor names one place.
fn locate_all(content: &str, edits: &[Replacement], path: &Path) -> Result<Vec<Found>, Refusal> {
    edits
        .iter()
        .enumerate()
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
                        index,
                        path,
                        matched,
                        places,
                        &shown.lines,
                        shown.hidden,
                    ))
                }
                matcher::Located::Miss => {
                    let hint = show::hint(content, &matcher::probes(content, &edit.old_text));
                    Err(Refusal::miss(index, path, &hint))
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

/// The replacements spliced into one snapshot, or the first colliding pair.
fn splice_all<'a>(
    content: &str,
    edits: &'a [Replacement],
    found: &[Found],
    path: &Path,
) -> Result<(String, Vec<Applied<'a>>), Refusal> {
    let replacements = edits
        .iter()
        .zip(found)
        .map(|(edit, found)| (found.span.clone(), edit.new_text.as_str()))
        .collect::<Vec<_>>();
    let (updated, landed) = matcher::splice(content, &replacements)
        .map_err(|overlap| Refusal::overlap(overlap.earlier, overlap.later, path))?;

    let applied = edits
        .iter()
        .zip(found)
        .zip(landed)
        .map(|((edit, found), line)| Applied {
            found: found.clone(),
            text: edit.new_text.as_str(),
            line,
        })
        .collect();
    Ok((updated, applied))
}

/// What the tool did: a line for each exact edit, the region for a fallback. Bounded
/// in lines rather than edits because the result is re-sent every later turn; what
/// does not fit is counted, so the model still learns the rest was applied.
fn report(path: &Path, original: &str, applied: &[Applied]) -> String {
    /// The most lines one result spends on receipts.
    const LINES: usize = 24;

    let count = applied.len();
    let noun = if count == 1 { "edit" } else { "edits" };
    let mut report = format!("applied {count} {noun} to {}", path.display());
    let mut spent = 0;
    let mut shown = 0;
    for patch in applied {
        let receipt = if patch.found.is_fallback() {
            format!(
                "{} at line {}:\n{}",
                patch.found.matcher,
                patch.line,
                show::change(original, &patch.found.span, patch.text, patch.line)
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
        report.push_str(&format!("\n… and {} more regions", count - shown));
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
    /// is what tells it which two edits to combine.
    #[test]
    fn an_overlap_names_the_two_edits_to_combine() {
        let edits = ["abc", "cde"].map(|old_text| Replacement {
            old_text: old_text.to_string(),
            new_text: "x".to_string(),
        });
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

        let refusal = splice_all("abcdef\n", &edits, &found, Path::new("/repo/file.rs"))
            .expect_err("the spans overlap");

        assert!(
            refusal
                .to_string()
                .contains("edits[0] and edits[1] overlap"),
            "{refusal}"
        );
    }

    /// Empty batches are rejected before reading or writing a path.
    #[tokio::test]
    async fn an_empty_edit_batch_is_refused() {
        let dir = dir_holding("batch-empty", "content\n");
        let error = refusal(&dir, json!([])).await;

        assert!(error.contains("at least one replacement"), "{error}");
        assert_eq!(body(&dir), "content\n");
    }

    #[tokio::test]
    async fn an_empty_old_text_is_refused_without_changing_the_file() {
        let dir = dir_holding("empty-old-text", "alpha\nbeta\n");
        let error = refusal(&dir, json!([{ "old_text": "", "new_text": "x" }])).await;

        assert!(error.contains("old_text must not be empty"), "{error}");
        assert_eq!(body(&dir), "alpha\nbeta\n");
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
        assert_eq!(schema["required"], json!(["path", "edits"]));
        assert_eq!(
            schema["properties"]["edits"]["items"]["required"],
            json!(["old_text", "new_text"])
        );
    }

    /// A batch larger than the receipt budget is reported up to the budget and
    /// counted beyond it, because the result is re-sent on every later turn.
    #[test]
    fn a_report_stops_at_the_line_budget_and_counts_the_rest() {
        let applied = (1..=30)
            .map(|line| Applied {
                found: Found {
                    matcher: matcher::EXACT,
                    span: 0..1,
                },
                text: "x",
                line,
            })
            .collect::<Vec<_>>();
        let report = report(Path::new("/repo/file.rs"), "a\n", &applied);

        assert_eq!(report.matches("exact at line").count(), 24);
        assert!(report.contains("… and 6 more regions"), "{report}");
    }

    #[test]
    fn one_edit_is_not_reported_as_edits() {
        let applied = [Applied {
            found: Found {
                matcher: matcher::EXACT,
                span: 0..1,
            },
            text: "x",
            line: 1,
        }];
        let report = report(Path::new("/repo/file.rs"), "a\n", &applied);

        assert!(
            report.starts_with("applied 1 edit to /repo/file.rs"),
            "{report}"
        );
    }
}
