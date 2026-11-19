//! The `edit` tool: one file, one anchor, one replacement. Its sibling
//! `multi_edit` borrows the steps below — reading, locating, and answering —
//! because several edits to one file are this tool repeated, not a second
//! mechanism.
//!
//! A call names `path`, `old_str` and `new_str`. The anchor is matched against
//! the file's own bytes — exact first, then a pass that ignores trailing
//! whitespace — and refused when it names no place or more than one. Nothing is
//! written unless the anchor landed, so a refusal leaves the file as it was.
//!
//! A refusal is an answer, not a failure: it carries the words the model needs to
//! retry.

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
mod multi;
mod refusal;
mod show;

pub use multi::MultiEditTool;

/// One anchor, matched against the file's own bytes.
pub struct EditTool;

/// The rule both file-changing tools state for the anchor and its replacement,
/// written once: a model reading two tools must not be taught two versions of
/// one requirement.
const OLD_STR_DESCRIPTION: &str = "The exact existing text to replace. It must \
                                   uniquely match one place in the file: include 2 \
                                   to 3 lines of unchanged code immediately before \
                                   and after the change to anchor the match, and \
                                   never leave it empty.";
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
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> String {
        "Modify an existing file by replacing an exact block of lines (`old_str`) with \
         new lines (`new_str`). Always prefer this tool over `write` for modifying an \
         existing file; for several changes to one file, use `multi_edit`."
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
    /// one snapshot safe to rewrite. A caller reaching `execute` without it — the
    /// registry's own is one — does not get that guarantee.
    fn lock_path(&self) -> Option<&'static str> {
        Some(PATH_ARGUMENT)
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        answered(edit(input, &ctx.cwd).await)
    }
}

/// The answer a call gets, from either file tool: a report, a refusal rendered as
/// the model's next instruction, or the tool's own failure. A refusal travels
/// back the way a success does and only `is_error` says otherwise.
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

/// One call, as the steps it takes: check the arguments, read the file, locate the
/// anchor in that snapshot, splice the replacement, write it, and report where it
/// landed.
async fn edit(input: Value, cwd: &Path) -> Result<String, Fault> {
    let input = parse_input(input)?;
    let path = resolve_path(cwd, &input.path);

    let content = read_text(&path).await?;
    let found = locate(&content, &input.old_str, &path, None)?;
    let (updated, line) = matcher::splice(&content, &found.span, &input.new_str);

    replace_file(&path, &updated).await.map_err(Fault::Broken)?;

    Ok(report(&path, &content, &found, &input.new_str, line))
}

/// The arguments, checked for what a provider may ignore in the schema: an empty
/// anchor would otherwise reach the matcher, where it matches once per byte and is
/// reported as ambiguous rather than wrong, and a blank path would resolve to the
/// working directory.
fn parse_input(input: Value) -> Result<EditInput, Refusal> {
    let input: EditInput =
        serde_json::from_value(input).map_err(|error| Refusal::bad_input(&error.to_string()))?;
    if input.old_str.is_empty() {
        return Err(Refusal::empty_anchor(None));
    }
    if input.path.trim().is_empty() {
        return Err(Refusal::no_path());
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
            std::io::ErrorKind::IsADirectory => Fault::Refused(Refusal::not_a_file(path)),
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

/// The anchor's place in the snapshot, or the refusal that says why it names
/// none. Every matcher reads the original bytes, and one refusal is final:
/// nothing is written unless the anchor names one place.
fn locate(content: &str, old_str: &str, path: &Path, at: Option<usize>) -> Result<Found, Refusal> {
    match matcher::locate(content, old_str) {
        matcher::Located::Found { matcher, span } => Ok(Found { matcher, span }),
        matcher::Located::Ambiguous {
            matcher: matched,
            places,
            lines,
        } => {
            let shown = show::candidates(content, &lines);
            Err(Refusal::ambiguous(
                at,
                path,
                matched,
                places,
                &shown.lines,
                shown.hidden,
            ))
        }
        matcher::Located::Miss => {
            let hint = show::hint(content, &matcher::probes(content, old_str));
            Err(Refusal::miss(at, path, &hint))
        }
    }
}

/// What the tool did: the line an exact anchor landed on, and the region a
/// fallback changed. Only the fallback is quoted, because it matched text the
/// model did not send.
fn report(path: &Path, original: &str, found: &Found, text: &str, line: usize) -> String {
    let header = format!("applied 1 edit to {}", path.display());
    let landed = format!("{header}\n{} at line {line}", found.matcher);
    match quoted(original, found, text) {
        Some(region) => format!("{landed}:\n{region}"),
        None => landed,
    }
}

/// The evidence a fallback owes the model — the region it really matched — and
/// nothing for an exact anchor, which is the bytes the model sent.
fn quoted(original: &str, found: &Found, text: &str) -> Option<String> {
    found
        .is_fallback()
        .then(|| show::change(original, &found.span, text))
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

    /// The tool called the way a model calls it, with whatever arguments it sent —
    /// for the calls whose arguments are what is under test.
    async fn call(cwd: &Path, arguments: Value) -> ToolResult {
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        EditTool
            .execute(arguments, &ctx)
            .await
            .expect("the call must be answered")
    }

    async fn edit(cwd: &Path, old_str: &str, new_str: &str) -> String {
        let report = call(
            cwd,
            json!({ "path": "file.txt", "old_str": old_str, "new_str": new_str }),
        )
        .await;
        assert!(
            !report.is_error,
            "the edit must succeed: {}",
            report.content
        );
        report.content
    }

    /// A refusal is an answered call, not a failed one: the result carries the
    /// instruction and `is_error` is the only thing that differs, so the batch
    /// does not prefix it with a report about the machine.
    async fn refusal(cwd: &Path, old_str: &str, new_str: &str) -> String {
        let result = call(
            cwd,
            json!({ "path": "file.txt", "old_str": old_str, "new_str": new_str }),
        )
        .await;
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
        edit(&dir, "beta", "gamma").await;

        assert_eq!(body(&dir), "alpha\ngamma\n");
    }

    /// Applying from the original offsets lets one replacement change length and
    /// another delete text, which is what `splice` assembles without adjusting.
    #[tokio::test]
    async fn a_replacement_can_change_length_or_delete() {
        let dir = dir_holding("length", "a middle z\n");
        edit(&dir, " middle", "").await;

        assert_eq!(body(&dir), "a z\n");
    }

    /// An exact anchor is the text the model sent, so the receipt is the line it
    /// landed on and nothing else: the file is not quoted back at the model that
    /// just quoted it.
    #[tokio::test]
    async fn an_exact_edit_is_reported_as_one_line() {
        let dir = dir_holding("report-exact", "alpha\nbeta\ngamma\n");
        let report = edit(&dir, "beta", "replaced").await;

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
        let report = edit(&dir, "alpha\nbeta", "ALPHA\nBETA").await;

        assert!(report.contains("trailing whitespace ignored"), "{report}");
        assert!(report.contains("- alpha   "), "{report}");
        assert!(report.contains("+ ALPHA"), "{report}");
    }

    /// A trailing space in the file is not a wrong anchor: the fallback matcher
    /// lands the edit and the result says which one did.
    #[tokio::test]
    async fn trailing_whitespace_in_the_file_still_matches() {
        let dir = dir_holding("trailing", "alpha   \nbeta\n");
        let report = edit(&dir, "alpha\nbeta", "alpha\nbeta\ngamma").await;

        assert_eq!(body(&dir), "alpha\nbeta\ngamma\n");
        assert!(report.contains("trailing whitespace ignored"), "{report}");
    }

    /// A file that spells its lines CRLF is edited through an anchor copied from a
    /// read, which carries only `\n`: the anchor is respelled, the replacement
    /// written back the same way, and the report says `exact`, not whitespace.
    #[tokio::test]
    async fn a_crlf_file_is_edited_and_stays_crlf() {
        let dir = dir_holding("crlf", "alpha\r\nbeta\r\ngamma\r\n");
        let report = edit(&dir, "beta", "B1\nB2").await;

        assert_eq!(body(&dir), "alpha\r\nB1\r\nB2\r\ngamma\r\n");
        assert!(report.contains("exact at line 2"), "{report}");
    }

    /// An anchor whose indentation is off is refused, but the refusal names the
    /// normalization that would have carried it, so the retry is a rewrite
    /// rather than a re-read.
    #[tokio::test]
    async fn a_miss_names_the_normalization_that_would_have_matched() {
        let dir = dir_holding("miss-indent", "fn main() {\n    let x = 1;\n}\n");
        let error = refusal(&dir, "  fn main() {\n    let x = 1;\n}", "x").await;

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
            "The answer is printed, then recorded as reference\nmaterial.",
            "x",
        )
        .await;

        assert!(error.contains("line breaks ignored"), "{error}");
        assert!(error.contains("   2 | reference material."), "{error}");
        assert_eq!(body(&dir), content);
    }

    #[tokio::test]
    async fn a_missing_anchor_leaves_the_file_untouched() {
        let dir = dir_holding("untouched", "alpha\nbeta\n");
        let error = refusal(&dir, "omega", "x").await;

        assert!(error.contains("nothing was written"), "{error}");
        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    /// Ambiguity is refused rather than resolved by taking the first match, and
    /// the refusal names every candidate line.
    #[tokio::test]
    async fn an_ambiguous_edit_names_every_candidate_line() {
        let dir = dir_holding("ambiguous", "same\nother\nsame\n");
        let error = refusal(&dir, "same", "x").await;

        assert!(error.contains("   1 | same"), "{error}");
        assert!(error.contains("   3 | same"), "{error}");
        assert_eq!(body(&dir), "same\nother\nsame\n");
    }

    /// A substring with overlapping starting positions is ambiguous even when
    /// non-overlapping iteration would report only one occurrence.
    #[tokio::test]
    async fn a_self_overlapping_anchor_is_refused_without_writing() {
        let dir = dir_holding("self-overlap", "aaa\n");
        let error = refusal(&dir, "aa", "x").await;

        assert!(error.contains("2 places"), "{error}");
        assert_eq!(body(&dir), "aaa\n");
    }

    #[tokio::test]
    async fn an_empty_old_str_is_refused_without_changing_the_file() {
        let dir = dir_holding("empty-old-text", "alpha\nbeta\n");
        let error = refusal(&dir, "", "x").await;

        assert!(error.contains("old_str must not be empty"), "{error}");
        assert_eq!(body(&dir), "alpha\nbeta\n");
    }

    /// A blank path resolves to the working directory, which is not a file to
    /// edit, so it is refused by name before anything is read.
    #[tokio::test]
    async fn a_blank_path_is_refused() {
        let dir = dir_holding("blank-path", "alpha\n");
        let error = call(
            &dir,
            json!({"path": "  ", "old_str": "alpha", "new_str": "beta"}),
        )
        .await;

        assert!(error.is_error, "{}", error.content);
        assert!(
            error.content.contains("path must name a file"),
            "{}",
            error.content
        );
        assert_eq!(body(&dir), "alpha\n");
    }

    /// A path that is not there is named as such, rather than as a read failure
    /// whose io message the model has to decode.
    #[tokio::test]
    async fn a_missing_file_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-missing-file", None);
        let error = refusal(&dir, "a", "b").await;

        assert!(error.contains("file does not exist"), "{error}");
    }

    /// An anchor cannot be found in bytes the tool cannot decode, so the refusal
    /// says what the file is instead of reporting a failed read.
    #[tokio::test]
    async fn a_file_that_is_not_text_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-binary", None);
        std::fs::write(dir.join("file.txt"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let error = refusal(&dir, "a", "b").await;

        assert!(error.contains("not a UTF-8 text file"), "{error}");
    }

    /// A directory where a file should be is the model's mistake, not the tool's,
    /// so it is refused by name: the trace stays a working tool that said no.
    #[tokio::test]
    async fn a_directory_is_refused_by_name() {
        let dir = crate::config::test_dir("edit-directory", None);
        std::fs::create_dir(dir.join("file.txt")).unwrap();
        let refusal = call(
            &dir,
            json!({"path": "file.txt", "old_str": "a", "new_str": "b"}),
        )
        .await;

        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal.content.contains("is a directory, not a file"),
            "{}",
            refusal.content
        );
    }

    /// A path the tool cannot read for a reason it cannot explain — a file whose
    /// permissions forbid reading it — is a failure, not a refusal: it returns as
    /// an error, so the trace says the tool broke.
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
                json!({ "path": "file.txt", "old_str": "a", "new_str": "b" }),
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

    /// Arguments that do not decode are a refusal like any other: the model is
    /// told the shape it owed, not shown a deserializer's complaint alone.
    #[tokio::test]
    async fn malformed_arguments_are_refused_with_the_shape_expected() {
        let dir = dir_holding("bad-input", "content\n");
        let error = call(&dir, json!({ "path": "file.txt", "old_str": "content" })).await;

        assert!(error.is_error, "{}", error.content);
        assert!(
            error.content.contains("invalid edit input"),
            "{}",
            error.content
        );
        assert!(error.content.contains("new_str"), "{}", error.content);
        assert_eq!(body(&dir), "content\n");
    }

    #[test]
    fn the_schema_names_the_three_fields() {
        let schema = EditTool.schema();
        assert_eq!(schema["required"], json!(["path", "old_str", "new_str"]));
        assert_eq!(schema["properties"]["old_str"]["minLength"], 1);
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            schema["properties"]["edits"].is_null(),
            "one edit per call, so there is no batch to describe: {schema}"
        );
    }

    /// The schema says no property it does not name, and the tool enforces it: a
    /// call still carrying the array shape is refused by the field it should not
    /// have sent, rather than quietly ignored.
    #[tokio::test]
    async fn a_field_the_schema_does_not_name_is_refused() {
        let dir = dir_holding("unknown-field", "alpha\n");
        let error = call(
            &dir,
            json!({
                "path": "file.txt",
                "old_str": "alpha",
                "new_str": "beta",
                "edits": [{"old_str": "alpha", "new_str": "beta"}]
            }),
        )
        .await;

        assert!(error.is_error, "{}", error.content);
        assert!(
            error.content.contains("unknown field `edits`"),
            "{}",
            error.content
        );
        assert_eq!(body(&dir), "alpha\n");
    }
}
