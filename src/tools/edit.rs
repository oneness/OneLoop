//! The `edit` tool: one file, one replacement, one write.
//!
//! A call names `path`, `old_str`, and `new_str`, and may set `replace_all`.
//! The anchor is matched against the file's own bytes — exact first, then a pass
//! that ignores trailing whitespace — and refused when it names no place or,
//! without `replace_all`, more than one. A refusal is an answer, not a failure:
//! it carries the words the model needs to retry. The file is written only after
//! the anchor matched, so a refusal leaves it as it was.

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

/// One call to the one file tool: a path and one replacement.
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
        "Change part of an existing file with one exact search-and-replace. \
         `old_str` must match one place, or every place when `replace_all` is \
         set; otherwise nothing is written. Several changes to one file may be \
         sent in one message; calls naming the same file apply in the order \
         sent, each against what the previous one wrote. Wait for a result only \
         when you need it to write the next anchor. Prefer this tool over \
         `write` for an existing file."
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
            "required": ["path", "old_str", "new_str"],
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

/// Why the step stopped: a [`Refusal`] is an answer the model can act on and returns
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
/// the anchor, render what landed, then write once — so a refusal leaves the
/// file as it was.
async fn edit(input: Value, cwd: &Path) -> Result<String, Fault> {
    let input = parse_input(input)?;
    let path = resolve_path(cwd, &input.path);

    let original = steps::read_text(&path).await?;
    let found = if input.replace_all {
        steps::locate_all(&original, &input.old_str, &path)?
    } else {
        steps::locate(&original, &input.old_str, &path)?
    };
    // Rendered before the splice: a fallback quotes the bytes the anchor matched.
    let report = steps::report(&path, &original, &found, &input.new_str, input.replace_all);
    let updated = matcher::splice(&original, &found.spans, &input.new_str);

    replace_file(&path, &updated).await.map_err(Fault::Broken)?;

    Ok(report)
}

/// The arguments, checked for what a provider may ignore in the schema: an empty
/// anchor would reach the matcher, where it matches once per byte, and a blank
/// path would resolve to the working directory.
fn parse_input(input: Value) -> Result<EditInput, Refusal> {
    let input: EditInput =
        serde_json::from_value(input).map_err(|error| Refusal::bad_input(&error.to_string()))?;
    if input.path.trim().is_empty() {
        return Err(Refusal::no_path());
    }
    if input.old_str.is_empty() {
        return Err(Refusal::empty_anchor());
    }
    if input.old_str == input.new_str {
        return Err(Refusal::no_op());
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

    /// Most tests name `file.txt`; the ones about `path` build the full call.
    async fn apply(cwd: &Path, arguments: Value) -> ToolResult {
        let mut arguments = arguments;
        arguments
            .as_object_mut()
            .expect("edit arguments must be an object")
            .insert("path".to_string(), json!("file.txt"));
        call(cwd, arguments).await
    }

    /// The helpers below assert the precondition a test assumes — that a call
    /// succeeded or was refused — so each test has one assertion of its own.
    async fn edit(cwd: &Path, arguments: Value) -> String {
        let result = apply(cwd, arguments).await;
        assert!(!result.is_error, "{}", result.content);
        result.content
    }

    async fn refusal(cwd: &Path, arguments: Value) -> String {
        let result = apply(cwd, arguments).await;
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
        edit(&dir, json!({"old_str": "beta", "new_str": "gamma"})).await;

        assert_eq!(body(&dir), "alpha\ngamma\n");
    }

    #[tokio::test]
    async fn a_replacement_can_change_length_or_delete() {
        let dir = dir_holding("length", "a middle z\n");
        edit(&dir, json!({"old_str": " middle", "new_str": ""})).await;

        assert_eq!(body(&dir), "a z\n");
    }

    #[tokio::test]
    async fn an_exact_edit_is_reported() {
        let dir = dir_holding("report-exact", "alpha\nbeta\ngamma\n");
        let report = edit(&dir, json!({"old_str": "beta", "new_str": "replaced"})).await;

        assert_eq!(
            report,
            format!("applied edit to {}: exact", dir.join("file.txt").display())
        );
    }

    /// A fallback is worth quoting: it matched text the model did not send, so
    /// the result shows the lines the file really held.
    #[tokio::test]
    async fn a_fallback_reports_the_region_it_changed() {
        let dir = dir_holding("report-fallback", "alpha   \nbeta\n");
        let report = edit(
            &dir,
            json!({"old_str": "alpha\nbeta", "new_str": "ALPHA\nBETA"}),
        )
        .await;

        assert_eq!(
            report,
            format!(
                "applied edit to {}: trailing whitespace ignored:\n- alpha   \n- beta\n+ ALPHA\n+ BETA",
                dir.join("file.txt").display()
            )
        );
    }

    #[tokio::test]
    async fn trailing_whitespace_in_the_file_still_matches() {
        let dir = dir_holding("trailing", "alpha   \nbeta\n");
        edit(
            &dir,
            json!({"old_str": "alpha\nbeta", "new_str": "alpha\nbeta\ngamma"}),
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
        edit(&dir, json!({"old_str": "beta", "new_str": "B1\nB2"})).await;

        assert_eq!(body(&dir), "alpha\r\nB1\r\nB2\r\ngamma\r\n");
    }

    #[tokio::test]
    async fn a_miss_names_the_normalization_that_would_have_matched() {
        let dir = dir_holding("miss-indent", "fn main() {\n    let x = 1;\n}\n");
        let error = refusal(
            &dir,
            json!({"old_str": "  fn main() {\n    let x = 1;\n}", "new_str": "x"}),
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
            json!({
                "old_str": "The answer is printed, then recorded as reference\nmaterial.",
                "new_str": "x"
            }),
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
            json!({
                "old_str": "The answer is printed, then recorded as reference\nmaterial.",
                "new_str": "x"
            }),
        )
        .await;

        assert!(error.contains("   2 | reference material."), "{error}");
    }

    #[tokio::test]
    async fn a_missing_anchor_says_nothing_was_written() {
        let dir = dir_holding("untouched-refusal", "alpha\nbeta\n");
        let error = refusal(&dir, json!({"old_str": "omega", "new_str": "x"})).await;

        assert!(error.contains("nothing was written"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_anchor_leaves_the_file_untouched() {
        let dir = dir_holding("untouched", "alpha\nbeta\n");
        refusal(&dir, json!({"old_str": "omega", "new_str": "x"})).await;

        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn an_ambiguous_edit_names_the_first_candidate() {
        let dir = dir_holding("ambiguous-first", "same\nother\nsame\n");
        let error = refusal(&dir, json!({"old_str": "same", "new_str": "x"})).await;

        assert!(error.contains("   1 | same"), "{error}");
    }

    #[tokio::test]
    async fn an_ambiguous_edit_names_the_last_candidate() {
        let dir = dir_holding("ambiguous-last", "same\nother\nsame\n");
        let error = refusal(&dir, json!({"old_str": "same", "new_str": "x"})).await;

        assert!(error.contains("   3 | same"), "{error}");
    }

    #[tokio::test]
    async fn a_self_overlapping_anchor_is_refused_without_writing() {
        let dir = dir_holding("self-overlap", "aaa\n");
        let error = refusal(&dir, json!({"old_str": "aa", "new_str": "x"})).await;

        assert!(error.contains("2 places"), "{error}");
    }

    #[tokio::test]
    async fn a_no_op_edit_is_refused() {
        let dir = dir_holding("no-op", "alpha\n");
        let error = refusal(&dir, json!({"old_str": "alpha", "new_str": "alpha"})).await;

        assert!(
            error.contains("old_str and new_str are identical"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_no_op_edit_leaves_the_file_untouched() {
        let dir = dir_holding("no-op-untouched", "alpha\n");
        refusal(&dir, json!({"old_str": "alpha", "new_str": "alpha"})).await;

        assert_eq!(body(&dir), "alpha\n");
    }

    #[tokio::test]
    async fn an_empty_anchor_is_refused() {
        let dir = dir_holding("empty-anchor", "a\n");
        let error = refusal(&dir, json!({"old_str": "", "new_str": "x"})).await;

        assert!(
            error.contains("old_str must not be empty (nothing was written)"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_empty_anchor_leaves_the_file_untouched() {
        let dir = dir_holding("empty-anchor-untouched", "a\n");
        refusal(&dir, json!({"old_str": "", "new_str": "x"})).await;

        assert_eq!(body(&dir), "a\n");
    }

    #[tokio::test]
    async fn a_blank_path_is_refused() {
        let dir = dir_holding("blank-path", "alpha\n");
        let error = refusal_call(
            &dir,
            json!({
                "path": "  ",
                "old_str": "alpha",
                "new_str": "beta"
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
                "old_str": "alpha",
                "new_str": "beta"
            }),
        )
        .await;

        assert_eq!(body(&dir), "alpha\n");
    }

    #[tokio::test]
    async fn a_missing_file_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-missing-file", None);
        let error = refusal(&dir, json!({"old_str": "a", "new_str": "b"})).await;

        assert!(error.contains("file does not exist"), "{error}");
    }

    #[tokio::test]
    async fn a_file_that_is_not_text_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-binary", None);
        std::fs::write(dir.join("file.txt"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let error = refusal(&dir, json!({"old_str": "a", "new_str": "b"})).await;

        assert!(error.contains("not a UTF-8 text file"), "{error}");
    }

    #[tokio::test]
    async fn a_directory_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-directory", None);
        std::fs::create_dir(dir.join("file.txt")).unwrap();
        let error = refusal_call(
            &dir,
            json!({"path": "file.txt", "old_str": "a", "new_str": "b"}),
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
                json!({"path": "file.txt", "old_str": "a", "new_str": "b"}),
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

    /// A malformed call is refused by the field it left out, because that is the
    /// one thing the model has to add before the call can work.
    #[tokio::test]
    async fn a_malformed_call_is_refused_by_the_field_it_left_out() {
        let dir = dir_holding("bad-input", "content\n");
        let error = refusal_call(&dir, json!({"path": "file.txt", "old_str": "content"})).await;

        assert!(error.contains("missing field `new_str`"), "{}", error);
    }

    /// The array shape is gone: a call still carrying it is refused by the field
    /// it should replace with the flat arguments.
    #[tokio::test]
    async fn an_array_of_edits_is_refused_with_the_flat_shape() {
        let dir = dir_holding("old-shape", "alpha\n");
        let error = refusal_call(
            &dir,
            json!({
                "path": "file.txt",
                "edits": [{"old_str": "alpha", "new_str": "beta"}]
            }),
        )
        .await;

        assert!(error.contains("unknown field `edits`"), "{}", error);
    }

    #[tokio::test]
    async fn a_field_the_schema_does_not_name_is_refused() {
        let dir = dir_holding("unknown-field", "alpha\n");
        let error = refusal_call(
            &dir,
            json!({
                "path": "file.txt",
                "old_str": "alpha",
                "new_str": "beta",
                "new_text": "beta"
            }),
        )
        .await;

        assert!(error.contains("unknown field `new_text`"), "{}", error);
    }

    #[tokio::test]
    async fn replace_all_rewrites_every_occurrence() {
        let dir = dir_holding("replace-all", "a\nb\na\nb\n");
        edit(
            &dir,
            json!({"old_str": "a\nb", "new_str": "A\nB", "replace_all": true}),
        )
        .await;

        assert_eq!(body(&dir), "A\nB\nA\nB\n");
    }

    #[tokio::test]
    async fn replace_all_reports_how_many_places_it_changed() {
        let dir = dir_holding("replace-all-report", "a\nb\na\nb\n");
        let report = edit(
            &dir,
            json!({"old_str": "a\nb", "new_str": "A\nB", "replace_all": true}),
        )
        .await;

        assert_eq!(
            report,
            format!(
                "applied edit to {}: exact in 2 places",
                dir.join("file.txt").display()
            )
        );
    }

    #[tokio::test]
    async fn replace_all_with_one_place_names_the_count() {
        let dir = dir_holding("replace-all-one", "alpha\nbeta\ngamma\n");
        let report = edit(
            &dir,
            json!({"old_str": "beta", "new_str": "B", "replace_all": true}),
        )
        .await;

        assert_eq!(
            report,
            format!(
                "applied edit to {}: exact in 1 place",
                dir.join("file.txt").display()
            )
        );
    }

    #[tokio::test]
    async fn replace_all_fallback_names_its_one_place() {
        let dir = dir_holding("replace-all-fallback-one", "alpha   \nbeta\n");
        let report = edit(
            &dir,
            json!({
                "old_str": "alpha\nbeta",
                "new_str": "A\nB",
                "replace_all": true
            }),
        )
        .await;

        assert!(
            report.contains("trailing whitespace ignored in 1 place:"),
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
            json!({"old_str": "alpha\nalpha", "new_str": "A", "replace_all": true}),
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
            json!({
                "old_str": "alpha\nbeta",
                "new_str": "A\nB",
                "replace_all": true
            }),
        )
        .await;

        assert_eq!(report.matches("- alpha   ").count(), 2);
    }

    #[tokio::test]
    async fn replace_all_into_a_crlf_file_keeps_its_endings() {
        let dir = dir_holding("replace-all-crlf", "a\r\nb\r\na\r\nb\r\n");
        edit(
            &dir,
            json!({"old_str": "a\nb", "new_str": "A\nB", "replace_all": true}),
        )
        .await;

        assert_eq!(body(&dir), "A\r\nB\r\nA\r\nB\r\n");
    }

    /// Without the flag, the same repeated anchor is still the ambiguity this
    /// tool refuses rather than a silent batch.
    #[tokio::test]
    async fn a_repeated_anchor_is_ambiguous_without_replace_all() {
        let dir = dir_holding("replace-all-default", "a\na\n");
        let error = refusal(&dir, json!({"old_str": "a", "new_str": "b"})).await;

        assert!(error.contains("matches 2 places"), "{error}");
    }

    #[tokio::test]
    async fn replace_all_leaves_the_file_untouched_on_a_miss() {
        let dir = dir_holding("replace-all-miss", "alpha\nbeta\n");
        refusal(
            &dir,
            json!({"old_str": "missing", "new_str": "x", "replace_all": true}),
        )
        .await;

        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn replace_all_refuses_overlapping_occurrences() {
        let dir = dir_holding("replace-all-overlap", "aaa\n");
        let error = refusal(
            &dir,
            json!({"old_str": "aa", "new_str": "x", "replace_all": true}),
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
            json!({"old_str": "aa", "new_str": "X", "replace_all": true}),
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
            json!({"old_str": "aa", "new_str": "X", "replace_all": true}),
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
            json!({"old_str": "aa", "new_str": "X", "replace_all": true}),
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
                "required": ["path", "old_str", "new_str"],
                "additionalProperties": false
            })
        );
    }
}
