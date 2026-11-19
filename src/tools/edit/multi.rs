//! The `multi_edit` tool: several replacements in one file, applied in order.
//!
//! Each edit is matched against what the one before it wrote, so "applied
//! sequentially from top to bottom" is the whole rule — no snapshot to reconcile
//! and no overlap to refuse. The file is written once, after the last edit
//! landed, so a refusal anywhere leaves it as it was.
//!
//! A receipt names its edit by position — `edits[1]: exact` — rather than by
//! line: one edit moves the next one's lines, and a position does not.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::path::resolve_path;
use crate::tools::{
    AgentContext, PATH_ARGUMENT, PATH_DESCRIPTION, Tool, ToolResult, path_trace_label, replace_file,
};

use super::matcher;
use super::refusal::Refusal;
use super::{
    Fault, Found, NEW_STR_DESCRIPTION, OLD_STR_DESCRIPTION, answered, locate, quoted, read_text,
};

/// Several anchors in one file, each matched against what the one before wrote.
pub struct MultiEditTool;

/// The most lines one result spends on receipts: the result is re-sent on every
/// later turn, so what does not fit is counted rather than dropped in silence.
const RECEIPT_LINES: usize = 24;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiEditInput {
    path: String,
    edits: Vec<Replacement>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    old_str: String,
    new_str: String,
}

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &'static str {
        "multi_edit"
    }

    fn description(&self) -> String {
        "Apply several search-and-replace edits to one file in a single call. Each edit \
         is matched against what the one before it wrote, so edits may overlap, and a \
         later one may anchor on text an earlier one wrote. If any edit is refused, \
         nothing is written. Use this when making related changes to different parts of \
         the same file (e.g. adding an import at the top and using it below)."
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

    /// The same key `edit` answers, so a `multi_edit` and an `edit` on one file
    /// take turns behind one lock instead of each writing over the other's read.
    fn lock_path(&self) -> Option<&'static str> {
        Some(PATH_ARGUMENT)
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        answered(multi_edit(input, &ctx.cwd).await)
    }
}

/// One call, as the steps it takes: check the arguments, read the file once, apply
/// each edit to the running copy, then write that copy once — so a refusal in the
/// last edit leaves the file as it was.
async fn multi_edit(input: Value, cwd: &Path) -> Result<String, Fault> {
    let input = parse_input(input)?;
    let path = resolve_path(cwd, &input.path);

    let mut updated = read_text(&path).await?;
    let mut receipts = Vec::with_capacity(input.edits.len());
    for (at, edit) in input.edits.iter().enumerate() {
        // Matched against what the edit before it wrote: that is what applying
        // them "sequentially from top to bottom" means.
        let found = locate(&updated, &edit.old_str, &path, Some(at))?;
        // Rendered now, while the running copy still holds the anchor this edit
        // matched; after the splice those bytes are gone.
        receipts.push(receipt(at, &updated, &found, &edit.new_str));
        let (next, _) = matcher::splice(&updated, &found.span, &edit.new_str);
        updated = next;
    }

    replace_file(&path, &updated).await.map_err(Fault::Broken)?;

    Ok(report(&path, &receipts))
}

/// The arguments, checked for what a provider may ignore in the schema: an empty
/// anchor would reach the matcher, where it matches once per byte and is reported
/// as ambiguous rather than wrong, and a blank path would resolve to the working
/// directory.
fn parse_input(input: Value) -> Result<MultiEditInput, Refusal> {
    let input: MultiEditInput = serde_json::from_value(input)
        .map_err(|error| Refusal::bad_multi_input(&error.to_string()))?;
    if input.edits.is_empty() {
        return Err(Refusal::no_edits());
    }
    if input.path.trim().is_empty() {
        return Err(Refusal::no_path());
    }
    if let Some(index) = input.edits.iter().position(|edit| edit.old_str.is_empty()) {
        return Err(Refusal::empty_anchor(Some(index)));
    }
    Ok(input)
}

/// One edit's line in the result: the position it was sent at and the matcher that
/// carried it, with the region a fallback matched quoted underneath.
fn receipt(at: usize, before: &str, found: &Found, text: &str) -> String {
    let landed = format!("edits[{at}]: {}", found.matcher);
    match quoted(before, found, text) {
        Some(region) => format!("{landed}:\n{region}"),
        None => landed,
    }
}

/// What the tool did: a line for each exact edit, the region for a fallback. Bounded
/// in lines rather than edits because the result is re-sent every later turn; what
/// does not fit is counted, so the model still learns the rest was applied.
fn report(path: &Path, receipts: &[String]) -> String {
    let count = receipts.len();
    let mut report = format!("applied {count} {} to {}", noun(count), path.display());
    let mut spent = 0;
    let mut shown = 0;
    for receipt in receipts {
        let cost = receipt.lines().count();
        if spent + cost > RECEIPT_LINES {
            break;
        }
        spent += cost;
        shown += 1;
        report.push('\n');
        report.push_str(receipt);
    }
    if shown < count {
        let rest = count - shown;
        report.push_str(&format!("\n… and {rest} more {}", noun(rest)));
    }
    report
}

/// `edit` for one, `edits` for the rest: the counts in a receipt are read aloud,
/// so they have to agree with what they count.
fn noun(count: usize) -> &'static str {
    if count == 1 { "edit" } else { "edits" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

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
        MultiEditTool
            .execute(arguments, &ctx)
            .await
            .expect("the call must be answered")
    }

    #[tokio::test]
    async fn every_edit_lands() {
        let dir = dir_holding("multi-all", "a\nb\nc\n");
        let report = call(
            &dir,
            json!({"path": "file.txt", "edits": [
                {"old_str": "a\n", "new_str": "A\n"},
                {"old_str": "c\n", "new_str": "C\n"}
            ]}),
        )
        .await;

        assert!(!report.is_error, "{}", report.content);
        assert_eq!(body(&dir), "A\nb\nC\n");
        assert!(
            report.content.starts_with("applied 2 edits to "),
            "{}",
            report.content
        );
    }

    /// The order in the array is the order they apply in, so one edit can anchor on
    /// what the one before it wrote — which no pair of separate calls guarantees.
    #[tokio::test]
    async fn a_later_edit_sees_what_an_earlier_one_wrote() {
        let dir = dir_holding("multi-order", "A\n");
        let report = call(
            &dir,
            json!({"path": "file.txt", "edits": [
                {"old_str": "A", "new_str": "B"},
                {"old_str": "B", "new_str": "C"}
            ]}),
        )
        .await;

        assert!(!report.is_error, "{}", report.content);
        assert_eq!(body(&dir), "C\n");
    }

    /// A refusal in the middle writes nothing: the copy the edits build is only
    /// written once, after the last one landed.
    #[tokio::test]
    async fn a_refusal_in_the_middle_writes_nothing() {
        let dir = dir_holding("multi-refusal", "a\nb\n");
        let refusal = call(
            &dir,
            json!({"path": "file.txt", "edits": [
                {"old_str": "a\n", "new_str": "A\n"},
                {"old_str": "missing", "new_str": "x"},
                {"old_str": "b\n", "new_str": "B\n"}
            ]}),
        )
        .await;

        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal.content.starts_with("edits[1]: "),
            "{}",
            refusal.content
        );
        assert_eq!(body(&dir), "a\nb\n");
    }

    /// An empty anchor is named by the position it was sent in, because the fix is
    /// to fill in that edit rather than to work out which one it was.
    #[tokio::test]
    async fn an_empty_anchor_is_refused_by_position() {
        let dir = dir_holding("multi-empty-anchor", "a\n");
        let refusal = call(
            &dir,
            json!({"path": "file.txt", "edits": [
                {"old_str": "a", "new_str": "A"},
                {"old_str": "", "new_str": "x"}
            ]}),
        )
        .await;

        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal
                .content
                .contains("edits[1].old_str must not be empty (nothing was written)"),
            "{}",
            refusal.content
        );
        assert_eq!(body(&dir), "a\n");
    }

    #[tokio::test]
    async fn an_empty_batch_is_refused() {
        let dir = dir_holding("multi-empty", "a\n");
        let refusal = call(&dir, json!({"path": "file.txt", "edits": []})).await;

        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal.content.contains("at least one replacement"),
            "{}",
            refusal.content
        );
        assert_eq!(body(&dir), "a\n");
    }

    /// The schema says no property it does not name, and the tool enforces it: a
    /// call carrying `edit`'s fields is refused by name.
    #[tokio::test]
    async fn an_unknown_field_is_refused() {
        let dir = dir_holding("multi-unknown", "a\n");
        let refusal = call(
            &dir,
            json!({"path": "file.txt", "old_str": "a", "edits": [
                {"old_str": "a", "new_str": "A"}
            ]}),
        )
        .await;

        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal.content.contains("unknown field `old_str`"),
            "{}",
            refusal.content
        );
        assert_eq!(body(&dir), "a\n");
    }

    /// A fallback in the middle still quotes what it matched: the receipt is built
    /// from the text that edit saw, before the splice removed it.
    #[tokio::test]
    async fn a_fallback_quotes_the_text_that_edit_saw() {
        let dir = dir_holding("multi-fallback", "alpha   \nbeta\n");
        let report = call(
            &dir,
            json!({"path": "file.txt", "edits": [
                {"old_str": "alph", "new_str": "ALPH"},
                {"old_str": "ALPHa\nbeta", "new_str": "ALPHA\nBETA"}
            ]}),
        )
        .await;

        assert!(!report.is_error, "{}", report.content);
        assert!(
            report.content.contains("trailing whitespace ignored"),
            "{}",
            report.content
        );
        assert!(report.content.contains("- ALPHa   "), "{}", report.content);
        assert_eq!(body(&dir), "ALPHA\nBETA\n");
    }

    /// A receipt names the position its edit was sent at, so a fallback is
    /// traceable to the edit in the call that caused it — the same prefix an
    /// `edits[1]:` refusal uses.
    #[tokio::test]
    async fn a_receipt_names_the_position_it_was_sent_at() {
        let dir = dir_holding("multi-position", "a\nb\n");
        let report = call(
            &dir,
            json!({"path": "file.txt", "edits": [
                {"old_str": "a", "new_str": "A"},
                {"old_str": "b", "new_str": "B"}
            ]}),
        )
        .await;

        assert!(
            report.content.contains("edits[0]: exact"),
            "{}",
            report.content
        );
        assert!(
            report.content.contains("edits[1]: exact"),
            "{}",
            report.content
        );
    }

    /// Past the receipt budget the rest is counted, and it is counted as edits:
    /// they are edits, whatever each of them turned out to be.
    #[tokio::test]
    async fn receipts_past_the_budget_are_counted_as_edits() {
        let lines = (0..30).map(|n| format!("l{n}\n")).collect::<String>();
        let dir = dir_holding("multi-budget", &lines);
        let edits = (0..30)
            .map(|n| json!({"old_str": format!("l{n}\n"), "new_str": format!("L{n}\n")}))
            .collect::<Vec<_>>();

        let report = call(&dir, json!({"path": "file.txt", "edits": edits})).await;

        assert!(!report.is_error, "{}", report.content);
        assert!(
            report.content.ends_with("… and 6 more edits"),
            "{}",
            report.content
        );
        assert_eq!(body(&dir).lines().count(), 30);
    }

    #[test]
    fn the_schema_names_a_path_and_an_edit_list() {
        let schema = MultiEditTool.schema();
        let edit = crate::tools::edit::EditTool.schema();

        assert_eq!(schema["required"], json!(["path", "edits"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["edits"]["minItems"], 1);
        assert_eq!(
            schema["properties"]["edits"]["items"]["required"],
            json!(["old_str", "new_str"])
        );
        assert_eq!(
            schema["properties"]["edits"]["items"]["additionalProperties"],
            false
        );
        // One rule for the anchor, stated once: the two tools may not drift.
        assert_eq!(
            schema["properties"]["edits"]["items"]["properties"]["old_str"]["description"],
            edit["properties"]["old_str"]["description"]
        );
        assert_eq!(
            schema["properties"]["edits"]["items"]["properties"]["new_str"]["description"],
            edit["properties"]["new_str"]["description"]
        );
        assert_eq!(
            schema["properties"]["path"]["description"],
            edit["properties"]["path"]["description"]
        );
    }
}
