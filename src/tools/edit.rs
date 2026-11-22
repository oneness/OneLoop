//! The `edit` tool: one file, one or more replacements, one write.
//!
//! A call names `path` and an ordered `edits` list. Each `old_str` is matched
//! against the bytes the previous edit wrote, so a batch can change one end of
//! a file and then anchor on what it wrote at the other. `replace_all` turns a
//! repeated anchor into one edit over every place it names. The file is written
//! once, after the last edit landed, so a refusal anywhere leaves it as it was.
//!
//! An anchor is matched against the file's own bytes — exact first, then a pass
//! that ignores trailing whitespace — and refused when it names no place or more
//! than one, unless `replace_all` was asked for. A refusal is an answer, not a
//! failure: it carries the words the model needs to retry.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::path::resolve_path;
use crate::tools::{
    AgentContext, PATH_ARGUMENT, PATH_DESCRIPTION, Tool, ToolResult, path_trace_label, replace_file,
};

use refusal::Refusal;

mod matcher;
mod refusal;
mod show;
mod steps;

/// One call to the one file tool: a path and the edits to apply to it in order.
pub struct EditTool;

/// The rule the schema states for an anchor and its replacement, written once:
/// a model reading one tool must not be taught two versions of one requirement.
const OLD_STR_DESCRIPTION: &str = "The exact existing text to replace. It must \
                                   uniquely match one place in the file unless \
                                   `replace_all` is set: include 2 to 3 lines of \
                                   unchanged code immediately before and after the \
                                   change to anchor the match, and never leave it \
                                   empty.";
/// The other half of that rule: what the replacement owes the anchor.
const NEW_STR_DESCRIPTION: &str = "The complete replacement text. It must keep the \
                                   surrounding anchor lines `old_str` included, and \
                                   never carry truncation comments like `// ... \
                                   existing code ...` or `# ... unchanged`.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    edits: Vec<Replacement>,
}

/// The replacement text, and whether one anchor means one place or all of them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    old_str: String,
    new_str: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> String {
        "Change part of an existing file with one or more exact search-and-replace \
         edits, applied in the order given. Each edit is matched against what the \
         one before it wrote, so a later edit can anchor on what an earlier one \
         wrote. An edit with `replace_all` rewrites every occurrence of its \
         anchor; without it the anchor must match one place. If any edit is \
         refused, nothing is written. Prefer this tool over `write` for an \
         existing file."
            .to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": PATH_DESCRIPTION
                },
                "edits": {
                    "type": "array",
                    "description": "Ordered list of search-and-replace operations, applied \
                                    one after another in this order.",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_str": {
                                "type": "string",
                                "minLength": 1,
                                "description": OLD_STR_DESCRIPTION
                            },
                            "new_str": {
                                "type": "string",
                                "description": NEW_STR_DESCRIPTION
                            },
                            "replace_all": {
                                "type": "boolean",
                                "default": false,
                                "description": "If true, replace every occurrence of old_str. \
                                                Defaults to false, which requires old_str to \
                                                match exactly one place."
                            }
                        },
                        "required": ["old_str", "new_str"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        })
    }

    /// The resolved path, spelled the way every other tool in the trace spells
    /// it. A call that arrived without one says so.
    fn trace_label(&self, arguments: &Value, cwd: &Path) -> Option<String> {
        path_trace_label(arguments, cwd)
    }

    /// The batch holds this file's lock for the whole call, which is what makes
    /// one running copy safe to rewrite. A caller reaching `execute` without it
    /// — the registry's own is one — does not get that guarantee.
    fn lock_path(&self) -> Option<&'static str> {
        Some(PATH_ARGUMENT)
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        answered(edit(input, &ctx.cwd).await)
    }
}

/// The answer a call gets: a report, a refusal rendered as the model's next
/// instruction, or the tool's own failure. A refusal travels back the way a
/// success does and only `is_error` says otherwise.
fn answered(outcome: Result<String, Fault>) -> Result<ToolResult> {
    match outcome {
        Ok(report) => Ok(ToolResult {
            content: report,
            is_error: false,
        }),
        Err(Fault::Refused(refusal)) => Ok(ToolResult {
            content: refusal.to_string(),
            is_error: true,
        }),
        Err(Fault::Broken(error)) => Err(error),
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

/// One call, as the steps it takes: check the arguments, read the file once,
/// apply each edit to the running copy, then write that copy once — so a refusal
/// in the last edit leaves the file as it was.
async fn edit(input: Value, cwd: &Path) -> Result<String, Fault> {
    let input = parse_input(input)?;
    let path = resolve_path(cwd, &input.path);

    let mut updated = steps::read_text(&path).await?;
    let mut receipts = Vec::with_capacity(input.edits.len());
    for (at, edit) in input.edits.iter().enumerate() {
        // Matched against what the edit before it wrote: that is what applying
        // them "in order" means.
        let found = if edit.replace_all {
            steps::locate_all(&updated, &edit.old_str, &path, at)?
        } else {
            steps::locate(&updated, &edit.old_str, &path, at)?
        };
        // Rendered now, while the running copy still holds the anchors this edit
        // matched; after the splice those bytes are gone.
        receipts.push(steps::receipt(
            at,
            &updated,
            &found,
            &edit.new_str,
            edit.replace_all,
        ));
        updated = matcher::splice(&updated, &found.spans, &edit.new_str);
    }

    replace_file(&path, &updated).await.map_err(Fault::Broken)?;

    Ok(steps::report(&path, &receipts))
}

/// The arguments, checked for what a provider may ignore in the schema: an empty
/// anchor would reach the matcher, where it matches once per byte and is reported
/// as ambiguous rather than wrong, and a blank path would resolve to the working
/// directory.
fn parse_input(input: Value) -> Result<EditInput, Refusal> {
    let input: EditInput =
        serde_json::from_value(input).map_err(|error| Refusal::bad_input(&error.to_string()))?;
    if input.edits.is_empty() {
        return Err(Refusal::no_edits());
    }
    if input.path.trim().is_empty() {
        return Err(Refusal::no_path());
    }
    if let Some(at) = input.edits.iter().position(|edit| edit.old_str.is_empty()) {
        return Err(Refusal::empty_anchor(at));
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temp directory holding `file.txt`, so a call is exercised the way the
    /// agent makes one — through `execute`, not through the matcher.
    fn dir_holding(name: &str, body: &str) -> PathBuf {
        let dir = crate::config::test_dir(name, None);
        std::fs::write(dir.join("file.txt"), body).unwrap();
        dir
    }

    fn body(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("file.txt")).unwrap()
    }

    async fn call(cwd: &Path, arguments: Value) -> ToolResult {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        EditTool
            .execute(arguments, &ctx)
            .await
            .expect("the call must be answered")
    }

    async fn apply(cwd: &Path, edits: Value) -> ToolResult {
        call(cwd, json!({ "path": "file.txt", "edits": edits })).await
    }

    /// The helpers below assert the precondition a test assumes — that a call
    /// succeeded or was refused — so each test has one assertion of its own.
    async fn edit(cwd: &Path, edits: Value) -> String {
        let result = apply(cwd, edits).await;
        assert!(!result.is_error, "{}", result.content);
        result.content
    }

    async fn refusal(cwd: &Path, edits: Value) -> String {
        let result = apply(cwd, edits).await;
        assert!(
            result.is_error,
            "a refusal must be marked: {}",
            result.content
        );
        result.content
    }

    async fn refusal_call(cwd: &Path, arguments: Value) -> String {
        let result = call(cwd, arguments).await;
        assert!(
            result.is_error,
            "a refusal must be marked: {}",
            result.content
        );
        result.content
    }

    #[tokio::test]
    async fn a_unique_edit_rewrites_the_file() {
        let dir = dir_holding("unique", "alpha\nbeta\n");
        edit(&dir, json!([{"old_str": "beta", "new_str": "gamma"}])).await;

        assert_eq!(body(&dir), "alpha\ngamma\n");
    }

    #[tokio::test]
    async fn a_replacement_can_change_length_or_delete() {
        let dir = dir_holding("length", "a middle z\n");
        edit(&dir, json!([{"old_str": " middle", "new_str": ""}])).await;

        assert_eq!(body(&dir), "a z\n");
    }

    /// Every receipt names its edit by position, including the only one in a
    /// call: one tool, one shape, whatever the batch size.
    #[tokio::test]
    async fn an_exact_edit_is_reported_by_position() {
        let dir = dir_holding("report-exact", "alpha\nbeta\ngamma\n");
        let report = edit(&dir, json!([{"old_str": "beta", "new_str": "replaced"}])).await;

        assert_eq!(
            report,
            format!(
                "applied 1 edit to {}\nedits[0]: exact",
                dir.join("file.txt").display()
            )
        );
    }

    /// A fallback is worth quoting: it matched text the model did not send, so
    /// the receipt shows the lines the file really held.
    #[tokio::test]
    async fn a_fallback_reports_the_region_it_changed() {
        let dir = dir_holding("report-fallback", "alpha   \nbeta\n");
        let report = edit(
            &dir,
            json!([{"old_str": "alpha\nbeta", "new_str": "ALPHA\nBETA"}]),
        )
        .await;

        assert_eq!(
            report,
            format!(
                "applied 1 edit to {}\nedits[0]: trailing whitespace ignored:\n- alpha   \n- beta\n+ ALPHA\n+ BETA",
                dir.join("file.txt").display()
            )
        );
    }

    #[tokio::test]
    async fn trailing_whitespace_in_the_file_still_matches() {
        let dir = dir_holding("trailing", "alpha   \nbeta\n");
        edit(
            &dir,
            json!([{"old_str": "alpha\nbeta", "new_str": "alpha\nbeta\ngamma"}]),
        )
        .await;

        assert_eq!(body(&dir), "alpha\nbeta\ngamma\n");
    }

    /// A CRLF file is edited through an anchor copied from a read, which carries
    /// only `\n`: the anchor is respelled and the replacement written back the
    /// same way.
    #[tokio::test]
    async fn a_crlf_file_is_edited_and_stays_crlf() {
        let dir = dir_holding("crlf", "alpha\r\nbeta\r\ngamma\r\n");
        edit(&dir, json!([{"old_str": "beta", "new_str": "B1\nB2"}])).await;

        assert_eq!(body(&dir), "alpha\r\nB1\r\nB2\r\ngamma\r\n");
    }

    #[tokio::test]
    async fn a_miss_names_the_normalization_that_would_have_matched() {
        let dir = dir_holding("miss-indent", "fn main() {\n    let x = 1;\n}\n");
        let error = refusal(
            &dir,
            json!([{"old_str": "  fn main() {\n    let x = 1;\n}", "new_str": "x"}]),
        )
        .await;

        assert!(error.contains("indentation ignored"), "{error}");
    }

    #[tokio::test]
    async fn a_miss_names_the_line_breaks_a_rewrap_moved() {
        let content = "such flag. The answer is printed, then recorded as\nreference material.\n";
        let dir = dir_holding("miss-breaks", content);
        let error = refusal(
            &dir,
            json!([{
                "old_str": "The answer is printed, then recorded as reference\nmaterial.",
                "new_str": "x"
            }]),
        )
        .await;

        assert!(error.contains("line breaks ignored"), "{error}");
    }

    #[tokio::test]
    async fn a_rewrap_miss_shows_the_lines_to_copy() {
        let content = "such flag. The answer is printed, then recorded as\nreference material.\n";
        let dir = dir_holding("miss-breaks-copy", content);
        let error = refusal(
            &dir,
            json!([{
                "old_str": "The answer is printed, then recorded as reference\nmaterial.",
                "new_str": "x"
            }]),
        )
        .await;

        assert!(error.contains("   2 | reference material."), "{error}");
    }

    #[tokio::test]
    async fn a_missing_anchor_says_nothing_was_written() {
        let dir = dir_holding("untouched-refusal", "alpha\nbeta\n");
        let error = refusal(&dir, json!([{"old_str": "omega", "new_str": "x"}])).await;

        assert!(error.contains("nothing was written"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_anchor_leaves_the_file_untouched() {
        let dir = dir_holding("untouched", "alpha\nbeta\n");
        refusal(&dir, json!([{"old_str": "omega", "new_str": "x"}])).await;

        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn an_ambiguous_edit_names_the_first_candidate() {
        let dir = dir_holding("ambiguous-first", "same\nother\nsame\n");
        let error = refusal(&dir, json!([{"old_str": "same", "new_str": "x"}])).await;

        assert!(error.contains("   1 | same"), "{error}");
    }

    #[tokio::test]
    async fn an_ambiguous_edit_names_the_last_candidate() {
        let dir = dir_holding("ambiguous-last", "same\nother\nsame\n");
        let error = refusal(&dir, json!([{"old_str": "same", "new_str": "x"}])).await;

        assert!(error.contains("   3 | same"), "{error}");
    }

    #[tokio::test]
    async fn a_self_overlapping_anchor_is_refused_without_writing() {
        let dir = dir_holding("self-overlap", "aaa\n");
        let error = refusal(&dir, json!([{"old_str": "aa", "new_str": "x"}])).await;

        assert!(error.contains("2 places"), "{error}");
    }

    #[tokio::test]
    async fn an_empty_anchor_is_refused_by_position() {
        let dir = dir_holding("empty-anchor", "a\n");
        let error = refusal(
            &dir,
            json!([
                {"old_str": "a", "new_str": "A"},
                {"old_str": "", "new_str": "x"}
            ]),
        )
        .await;

        assert!(
            error.contains("edits[1].old_str must not be empty (nothing was written)"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_empty_anchor_leaves_the_file_untouched() {
        let dir = dir_holding("empty-anchor-untouched", "a\n");
        refusal(
            &dir,
            json!([
                {"old_str": "a", "new_str": "A"},
                {"old_str": "", "new_str": "x"}
            ]),
        )
        .await;

        assert_eq!(body(&dir), "a\n");
    }

    #[tokio::test]
    async fn a_blank_path_is_refused() {
        let dir = dir_holding("blank-path", "alpha\n");
        let error = refusal_call(
            &dir,
            json!({
                "path": "  ",
                "edits": [{"old_str": "alpha", "new_str": "beta"}]
            }),
        )
        .await;

        assert!(error.contains("path must name a file"), "{}", error);
    }

    #[tokio::test]
    async fn a_blank_path_changes_nothing() {
        let dir = dir_holding("blank-path-untouched", "alpha\n");
        call(
            &dir,
            json!({
                "path": "  ",
                "edits": [{"old_str": "alpha", "new_str": "beta"}]
            }),
        )
        .await;

        assert_eq!(body(&dir), "alpha\n");
    }

    #[tokio::test]
    async fn a_missing_file_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-missing-file", None);
        let error = refusal(&dir, json!([{"old_str": "a", "new_str": "b"}])).await;

        assert!(error.contains("file does not exist"), "{error}");
    }

    #[tokio::test]
    async fn a_file_that_is_not_text_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-binary", None);
        std::fs::write(dir.join("file.txt"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let error = refusal(&dir, json!([{"old_str": "a", "new_str": "b"}])).await;

        assert!(error.contains("not a UTF-8 text file"), "{error}");
    }

    #[tokio::test]
    async fn a_directory_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-directory", None);
        std::fs::create_dir(dir.join("file.txt")).unwrap();
        let error = refusal_call(
            &dir,
            json!({"path": "file.txt", "edits": [{"old_str": "a", "new_str": "b"}]}),
        )
        .await;

        assert!(error.contains("is a directory, not a file"), "{}", error);
    }

    /// A path the tool cannot read for a reason it cannot explain is a failure,
    /// not a refusal: it returns as an error, so the trace says the tool broke.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_path_fails_instead_of_refusing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::config::test_dir("edit-unreadable", None);
        let path = dir.join("file.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let ctx = AgentContext { cwd: dir.clone() };

        let error = EditTool
            .execute(
                json!({"path": "file.txt", "edits": [{"old_str": "a", "new_str": "b"}]}),
                &ctx,
            )
            .await
            .expect_err("a path that cannot be read is a failure, not a refusal");

        assert!(
            format!("{error:#}").contains("failed to read file"),
            "{error:#}"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[tokio::test]
    async fn malformed_arguments_are_refused_with_the_shape_expected() {
        let dir = dir_holding("bad-input", "content\n");
        let error = refusal_call(
            &dir,
            json!({"path": "file.txt", "edits": [{"old_str": "content"}]}),
        )
        .await;

        assert!(error.contains("invalid edit input"), "{}", error);
    }

    /// A malformed edit is refused by the field it left out, because that is the
    /// one thing the model has to add before the call can work.
    #[tokio::test]
    async fn a_malformed_edit_is_refused_by_the_field_it_left_out() {
        let dir = dir_holding("bad-input-shape", "content\n");
        let error = refusal_call(
            &dir,
            json!({"path": "file.txt", "edits": [{"old_str": "content"}]}),
        )
        .await;

        assert!(error.contains("missing field `new_str`"), "{}", error);
    }

    /// A call still carrying the shape of the old single-edit tool is refused by
    /// the fields it should replace with `edits`, not quietly read as no work.
    #[tokio::test]
    async fn a_single_edit_shape_is_refused_with_the_new_shape() {
        let dir = dir_holding("old-shape", "alpha\n");
        let error = refusal_call(
            &dir,
            json!({"path": "file.txt", "old_str": "alpha", "new_str": "beta"}),
        )
        .await;

        assert!(error.contains("expected `path` or `edits`"), "{}", error);
    }

    #[tokio::test]
    async fn a_field_the_schema_does_not_name_is_refused() {
        let dir = dir_holding("unknown-field", "alpha\n");
        let error = refusal_call(
            &dir,
            json!({
                "path": "file.txt",
                "old_str": "alpha",
                "edits": [{"old_str": "alpha", "new_str": "beta"}]
            }),
        )
        .await;

        assert!(error.contains("unknown field `old_str`"), "{}", error);
    }

    #[tokio::test]
    async fn an_unknown_field_inside_an_edit_is_refused() {
        let dir = dir_holding("unknown-edit-field", "alpha\n");
        let error = refusal(
            &dir,
            json!([{"old_str": "alpha", "new_str": "beta", "new_text": "beta"}]),
        )
        .await;

        assert!(error.contains("unknown field `new_text`"), "{}", error);
    }

    #[tokio::test]
    async fn every_edit_lands() {
        let dir = dir_holding("all", "a\nb\nc\n");
        edit(
            &dir,
            json!([
                {"old_str": "a\n", "new_str": "A\n"},
                {"old_str": "c\n", "new_str": "C\n"}
            ]),
        )
        .await;

        assert_eq!(body(&dir), "A\nb\nC\n");
    }

    #[tokio::test]
    async fn a_batch_report_counts_every_edit() {
        let dir = dir_holding("batch-count", "a\nb\nc\n");
        let report = edit(
            &dir,
            json!([
                {"old_str": "a\n", "new_str": "A\n"},
                {"old_str": "c\n", "new_str": "C\n"}
            ]),
        )
        .await;

        assert!(report.starts_with("applied 2 edits to "), "{report}");
    }

    /// The order in the array is the order they apply in, so one edit can anchor
    /// on what the one before it wrote — which separate calls do not guarantee.
    #[tokio::test]
    async fn a_later_edit_sees_what_an_earlier_one_wrote() {
        let dir = dir_holding("order", "A\n");
        edit(
            &dir,
            json!([
                {"old_str": "A", "new_str": "B"},
                {"old_str": "B", "new_str": "C"}
            ]),
        )
        .await;

        assert_eq!(body(&dir), "C\n");
    }

    /// A refusal in the middle writes nothing: the copy the edits build is only
    /// written once, after the last one landed.
    #[tokio::test]
    async fn a_refusal_in_the_middle_writes_nothing() {
        let dir = dir_holding("refusal-middle", "a\nb\n");
        refusal(
            &dir,
            json!([
                {"old_str": "a\n", "new_str": "A\n"},
                {"old_str": "missing", "new_str": "x"},
                {"old_str": "b\n", "new_str": "B\n"}
            ]),
        )
        .await;

        assert_eq!(body(&dir), "a\nb\n");
    }

    #[tokio::test]
    async fn a_refusal_names_the_edit_that_missed() {
        let dir = dir_holding("refusal-position", "a\nb\n");
        let error = refusal(
            &dir,
            json!([
                {"old_str": "a\n", "new_str": "A\n"},
                {"old_str": "missing", "new_str": "x"}
            ]),
        )
        .await;

        assert!(error.starts_with("edits[1]: "), "{error}");
    }

    #[tokio::test]
    async fn an_empty_batch_is_refused() {
        let dir = dir_holding("empty-batch", "a\n");
        let error = refusal(&dir, json!([])).await;

        assert!(error.contains("at least one replacement"), "{error}");
    }

    /// A fallback in the middle still quotes what it matched: the receipt is
    /// built before the splice removed those bytes.
    #[tokio::test]
    async fn a_fallback_quotes_the_text_that_edit_saw() {
        let dir = dir_holding("fallback-middle", "alpha   \nbeta\n");
        let report = edit(
            &dir,
            json!([
                {"old_str": "alph", "new_str": "ALPH"},
                {"old_str": "ALPHa\nbeta", "new_str": "ALPHA\nBETA"}
            ]),
        )
        .await;

        assert!(report.contains("- ALPHa   "), "{report}");
    }

    #[tokio::test]
    async fn a_receipt_names_the_first_position_it_was_sent_at() {
        let dir = dir_holding("receipt-first", "a\nb\n");
        let report = edit(
            &dir,
            json!([
                {"old_str": "a", "new_str": "A"},
                {"old_str": "b", "new_str": "B"}
            ]),
        )
        .await;

        assert!(report.contains("edits[0]: exact"), "{report}");
    }

    #[tokio::test]
    async fn a_receipt_names_the_last_position_it_was_sent_at() {
        let dir = dir_holding("receipt-last", "a\nb\n");
        let report = edit(
            &dir,
            json!([
                {"old_str": "a", "new_str": "A"},
                {"old_str": "b", "new_str": "B"}
            ]),
        )
        .await;

        assert!(report.contains("edits[1]: exact"), "{report}");
    }

    /// Past the receipt budget the rest is counted, and named as applied.
    #[tokio::test]
    async fn receipts_past_the_budget_are_counted_as_edits() {
        let lines = (0..30).map(|n| format!("l{n}\n")).collect::<String>();
        let dir = dir_holding("receipt-budget", &lines);
        let edits = (0..30)
            .map(|n| json!({"old_str": format!("l{n}\n"), "new_str": format!("L{n}\n")}))
            .collect::<Vec<_>>();

        let report = edit(&dir, json!(edits)).await;

        assert!(report.ends_with("… and 6 more edits applied"), "{report}");
    }

    #[tokio::test]
    async fn replace_all_rewrites_every_occurrence() {
        let dir = dir_holding("replace-all", "a\nb\na\nb\n");
        edit(
            &dir,
            json!([{"old_str": "a\nb", "new_str": "A\nB", "replace_all": true}]),
        )
        .await;

        assert_eq!(body(&dir), "A\nB\nA\nB\n");
    }

    #[tokio::test]
    async fn replace_all_reports_how_many_places_it_changed() {
        let dir = dir_holding("replace-all-report", "a\nb\na\nb\n");
        let report = edit(
            &dir,
            json!([{"old_str": "a\nb", "new_str": "A\nB", "replace_all": true}]),
        )
        .await;

        assert!(report.contains("edits[0]: exact in 2 places"), "{report}");
    }

    #[tokio::test]
    async fn replace_all_with_one_place_names_the_count() {
        let dir = dir_holding("replace-all-one", "alpha\nbeta\ngamma\n");
        let report = edit(
            &dir,
            json!([{"old_str": "beta", "new_str": "B", "replace_all": true}]),
        )
        .await;

        assert_eq!(
            report,
            format!(
                "applied 1 edit to {}\nedits[0]: exact in 1 place",
                dir.join("file.txt").display()
            )
        );
    }

    #[tokio::test]
    async fn replace_all_fallback_names_its_one_place() {
        let dir = dir_holding("replace-all-fallback-one", "alpha   \nbeta\n");
        let report = edit(
            &dir,
            json!([{
                "old_str": "alpha\nbeta",
                "new_str": "A\nB",
                "replace_all": true
            }]),
        )
        .await;

        assert!(
            report.contains("edits[0]: trailing whitespace ignored in 1 place:"),
            "{report}"
        );
    }

    /// A repeated anchor whose bytes differ only in trailing whitespace is one
    /// edit under the fallback pass, not two separate refusals.
    #[tokio::test]
    async fn replace_all_uses_the_fallback_when_exact_bytes_are_absent() {
        let dir = dir_holding("replace-all-fallback", "alpha   \nalpha   \n");
        edit(
            &dir,
            json!([{"old_str": "alpha\nalpha", "new_str": "A", "replace_all": true}]),
        )
        .await;

        assert_eq!(body(&dir), "A\n");
    }

    #[tokio::test]
    async fn replace_all_quotes_each_fallback_region() {
        let dir = dir_holding(
            "replace-all-regions",
            "alpha   \nbeta   \nalpha   \nbeta   \n",
        );
        let report = edit(
            &dir,
            json!([{"old_str": "alpha\nbeta", "new_str": "A\nB", "replace_all": true}]),
        )
        .await;

        assert_eq!(report.matches("- alpha   ").count(), 2);
    }

    #[tokio::test]
    async fn replace_all_into_a_crlf_file_keeps_its_endings() {
        let dir = dir_holding("replace-all-crlf", "a\r\nb\r\na\r\nb\r\n");
        edit(
            &dir,
            json!([{"old_str": "a\nb", "new_str": "A\nB", "replace_all": true}]),
        )
        .await;

        assert_eq!(body(&dir), "A\r\nB\r\nA\r\nB\r\n");
    }

    #[tokio::test]
    async fn a_later_replace_all_sees_what_an_earlier_one_wrote() {
        let dir = dir_holding("replace-all-order", "a\nb\na\nb\n");
        edit(
            &dir,
            json!([
                {"old_str": "a", "new_str": "A", "replace_all": true},
                {"old_str": "A", "new_str": "B", "replace_all": true}
            ]),
        )
        .await;

        assert_eq!(body(&dir), "B\nb\nB\nb\n");
    }

    /// Without the flag, the same repeated anchor is still the ambiguity this
    /// tool refuses rather than a silent batch.
    #[tokio::test]
    async fn a_repeated_anchor_is_ambiguous_without_replace_all() {
        let dir = dir_holding("replace-all-default", "a\na\n");
        let error = refusal(&dir, json!([{"old_str": "a", "new_str": "b"}])).await;

        assert!(error.contains("matches 2 places"), "{error}");
    }

    #[tokio::test]
    async fn replace_all_leaves_the_file_untouched_on_a_miss() {
        let dir = dir_holding("replace-all-miss", "alpha\nbeta\n");
        refusal(
            &dir,
            json!([{"old_str": "missing", "new_str": "x", "replace_all": true}]),
        )
        .await;

        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn replace_all_refuses_overlapping_occurrences() {
        let dir = dir_holding("replace-all-overlap", "aaa\n");
        let error = refusal(
            &dir,
            json!([{"old_str": "aa", "new_str": "x", "replace_all": true}]),
        )
        .await;

        assert!(error.contains("matches 2 places"), "{error}");
    }

    /// An overlap ends the exact pass, so the coarser one never names the single
    /// whole-line occurrence it can see. Without that, this call would rewrite
    /// the second line and report the anchor replaced.
    #[tokio::test]
    async fn replace_all_does_not_fall_through_an_overlap_to_a_coarser_pass() {
        let dir = dir_holding("replace-all-overlap-fallback", "aaaa\naa\n");
        let error = refusal(
            &dir,
            json!([{"old_str": "aa", "new_str": "X", "replace_all": true}]),
        )
        .await;

        assert!(error.contains("matches 4 places"), "{error}");
    }

    /// A `replace_all` refusal asks for disjoint occurrences, not a unique place.
    #[tokio::test]
    async fn replace_all_overlap_advises_disjoint_occurrences() {
        let dir = dir_holding("replace-all-overlap-advice", "aaaa\naa\n");
        let error = refusal(
            &dir,
            json!([{"old_str": "aa", "new_str": "X", "replace_all": true}]),
        )
        .await;

        assert!(
            error.contains("so its occurrences no longer overlap"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn replace_all_overlap_leaves_the_file_untouched() {
        let dir = dir_holding("replace-all-overlap-untouched", "aaaa\naa\n");
        refusal(
            &dir,
            json!([{"old_str": "aa", "new_str": "X", "replace_all": true}]),
        )
        .await;

        assert_eq!(body(&dir), "aaaa\naa\n");
    }

    /// The schema is the whole contract the model sees, so compare it whole: a
    /// new field cannot be added in code without describing it here.
    #[test]
    fn the_schema_describes_the_one_edit_shape() {
        assert_eq!(
            EditTool.schema(),
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": PATH_DESCRIPTION
                    },
                    "edits": {
                        "type": "array",
                        "description": "Ordered list of search-and-replace operations, applied one after another in this order.",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_str": {
                                    "type": "string",
                                    "minLength": 1,
                                    "description": OLD_STR_DESCRIPTION
                                },
                                "new_str": {
                                    "type": "string",
                                    "description": NEW_STR_DESCRIPTION
                                },
                                "replace_all": {
                                    "type": "boolean",
                                    "default": false,
                                    "description": "If true, replace every occurrence of old_str. Defaults to false, which requires old_str to match exactly one place."
                                }
                            },
                            "required": ["old_str", "new_str"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            })
        );
    }
}
