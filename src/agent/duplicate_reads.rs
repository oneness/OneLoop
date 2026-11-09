//! Answering a `read` whose payload the transcript already holds with a pointer
//! to the earlier copy.
//!
//! Cheap reads, expensive re-sends: a conversation is append-only, so a payload
//! sent once is carried on every remaining turn. This is an optimisation of
//! tokens, not of I/O — the file is read either way — and its guard is the
//! payload itself, never the request, so a file that changed underneath cannot
//! produce a matching pointer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::messages;
use crate::output;
use crate::tools::batch::file_key;
use crate::tools::{ToolRegistry, ToolResult};

/// Payloads a `read` has already put in the transcript, keyed by the file they
/// came from and a digest of exactly what the model was shown.
///
/// Derived from the session on every batch rather than kept in a side table:
/// the transcript *is* the cache. `/clear` empties it and the set with it, a
/// resumed session refills it from disk for free, and if tool output is ever
/// elided from the history the pointer disappears along with the content it
/// pointed at. A side table would need invalidating in all three cases, and
/// getting that wrong means telling the model it has seen something it cannot
/// see — the one way this optimisation can make an answer worse.
fn read_payloads_in_context(
    registry: &ToolRegistry,
    cwd: &Path,
    history: &[messages::Message],
) -> HashSet<(PathBuf, [u8; 32])> {
    let mut paths: HashMap<&str, PathBuf> = HashMap::new();
    let mut seen = HashSet::new();

    for message in history {
        match message {
            messages::Message::ToolCall(call) if call.name == "read" => {
                if let Some(key) = file_key(registry, cwd, &call.name, &call.arguments) {
                    paths.insert(call.id.as_str(), key);
                }
            }
            messages::Message::ToolResult(result)
                if result.tool_name == "read" && !result.is_error =>
            {
                if let Some(key) = paths.get(result.tool_call_id.as_str())
                    && !result.content.is_empty()
                {
                    seen.insert((key.clone(), payload_digest(&result.content)));
                }
            }
            _ => {}
        }
    }

    seen
}

fn payload_digest(content: &str) -> [u8; 32] {
    Sha256::digest(content.as_bytes()).into()
}

/// Replace a `read` whose payload the transcript already holds with a pointer
/// to it, and report how many were answered that way.
///
/// The model loses nothing — the bytes are still above, verbatim and readable —
/// and stops paying to receive them a second time. In an append-only
/// conversation "a second time" means every remaining turn, which is how one
/// duplicated read became 36% of a session's input tokens.
///
/// Identity is the *payload*, not the request: a file that changed underneath
/// cannot produce a matching digest, so a stale pointer is impossible by
/// construction and no mtime bookkeeping is needed. This is an optimisation of
/// tokens, not of I/O — the file is read either way, and re-reading it costs a
/// millisecond against the thousands of tokens a re-send costs.
pub(super) fn suppress_duplicate_reads(
    registry: &ToolRegistry,
    cwd: &Path,
    calls: &[messages::ToolCall],
    results: &mut [ToolResult],
    history: &[messages::Message],
) -> usize {
    let mut seen = read_payloads_in_context(registry, cwd, history);
    let mut suppressed = 0;

    for (call, result) in calls.iter().zip(results.iter_mut()) {
        if call.name != "read" || result.is_error || result.content.is_empty() {
            continue;
        }
        // An escape hatch, not a flag the tool needs: some models re-read on
        // purpose to bring material back into recent attention, and that is a
        // legitimate thing to want. Denying it would trade a token saving for
        // a behaviour change; taxing it by one call does not.
        let forced = call.arguments.get("force").and_then(Value::as_bool) == Some(true);

        let Some(key) = file_key(registry, cwd, &call.name, &call.arguments) else {
            continue;
        };
        let entry = (key, payload_digest(&result.content));

        if !forced && seen.contains(&entry) {
            result.content = duplicate_read_notice(&entry.0, &result.content, cwd);
            suppressed += 1;
        }
        // Recorded either way, so a forced re-read still guards the next one,
        // and so a twin in the same batch — which cannot see the other's
        // result in the history — is still answered with a pointer.
        seen.insert(entry);
    }

    suppressed
}

/// What a suppressed read says instead of its payload: that the bytes are
/// still above, how to reach whatever the copy there did not cover, and how to
/// insist on them anyway.
///
/// It names the file and not the message the copy sits at. The model has no
/// message numbers to count against, so a number would be an unanswerable
/// puzzle in the one sentence the model has to trust; what it needs to know is
/// that the content it asked for is already in the conversation, which is what
/// the second line says. Nothing here claims the earlier copy was the whole
/// file — it may have been one window of it, and the hint below is how the
/// model learns that.
fn duplicate_read_notice(path: &Path, payload: &str, cwd: &Path) -> String {
    let shown = path.strip_prefix(cwd).unwrap_or(path).display();
    let mut notice = format!(
        "read {shown}\nByte-identical to an earlier read of {shown} in this \
         conversation — not re-sent."
    );

    // The earlier copy's own notice is the one line of the payload worth
    // keeping: it is what tells the model how to reach the part it still does
    // not have. Carrying it over beats inventing a second, subtly different
    // hint — and [`output::window_notice`] decides what counts as one, so a
    // notice shape the tool learns to write is carried here for free.
    if let Some(hint) = payload.lines().next_back().and_then(output::window_notice) {
        notice.push('\n');
        notice.push_str(hint);
    }
    notice.push_str("\nPass force:true to re-send it anyway.");
    notice
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builtin set, for the tests that only need a tool's own answer about
    /// which argument names the file it locks. Nothing here touches the disk.
    fn registry() -> ToolRegistry {
        ToolRegistry::with_builtin_tools(Path::new("/repo")).unwrap()
    }

    fn read_call(id: &str, path: &str) -> messages::ToolCall {
        messages::ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": path }),
            parse_error: None,
        }
    }

    /// A transcript holding one completed read of `path` whose payload was
    /// `body`, exactly as a previous turn would have left it.
    fn transcript_with_one_read(path: &str, body: &str) -> Vec<messages::Message> {
        vec![
            messages::Message::ToolCall(read_call("earlier", path)),
            messages::Message::ToolResult(messages::ToolResultMessage {
                tool_call_id: "earlier".to_string(),
                tool_name: "read".to_string(),
                content: body.to_string(),
                is_error: false,
            }),
        ]
    }

    fn duplicate_payload() -> &'static str {
        "fn main() {}\n[showing lines 1-1 of 400 · continue with offset=2]"
    }

    /// The case that cost 36% of a session's input tokens: the same file read
    /// twice, the second copy carried on every remaining turn. The model keeps
    /// the content — from the first copy — and gets a pointer instead of the
    /// second.
    #[test]
    fn a_repeated_read_is_answered_with_a_pointer_to_the_original() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "src/main.rs")];
        let mut results = vec![ToolResult {
            content: duplicate_payload().to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", duplicate_payload());

        let suppressed = suppress_duplicate_reads(&registry(), cwd, &calls, &mut results, &history);

        assert_eq!(suppressed, 1);
        assert!(
            results[0].content.contains("src/main.rs"),
            "{}",
            results[0].content
        );
        assert!(
            results[0].content.contains("not re-sent"),
            "{}",
            results[0].content
        );
        assert!(
            !results[0].content.contains("fn main"),
            "payload was re-sent"
        );
        // The hint to reach the part the model does not have survives, and so
        // does the way to insist on the payload.
        assert!(
            results[0].content.contains("continue with offset=2"),
            "{}",
            results[0].content
        );
        assert!(
            results[0].content.contains("force:true"),
            "{}",
            results[0].content
        );
    }

    /// A payload that ended at the end of its own window carried no hint, and
    /// the pointer must not invent one — nor claim a completeness the earlier
    /// copy may not have had.
    #[test]
    fn a_pointer_to_a_payload_with_no_hint_claims_nothing_about_the_file() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "src/main.rs")];
        let mut results = vec![ToolResult {
            content: "fn main() {}".to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", "fn main() {}");

        assert_eq!(
            suppress_duplicate_reads(&registry(), cwd, &calls, &mut results, &history),
            1
        );
        assert!(
            !results[0].content.contains("complete file"),
            "{}",
            results[0].content
        );
        assert!(
            !results[0].content.contains("offset="),
            "{}",
            results[0].content
        );
    }

    /// Every notice the `read` tool can end a payload with survives the
    /// pointer, including the one for a line too wide for any window to reach
    /// its end — dropping that one would tell the model it has the whole file
    /// when the only way to see the rest is `bash`.
    #[test]
    fn a_hint_that_no_window_reaches_the_rest_survives_the_pointer() {
        let long = "x".repeat(64);
        let payload = output::truncate_window(&[long.as_str(), "b"], 1, 2, 8);
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "wide.txt")];
        let mut results = vec![ToolResult {
            content: payload.clone(),
            is_error: false,
        }];
        let history = transcript_with_one_read("wide.txt", &payload);

        assert_eq!(
            suppress_duplicate_reads(&registry(), cwd, &calls, &mut results, &history),
            1
        );
        assert!(
            results[0]
                .content
                .contains("no line window reaches the rest"),
            "{}",
            results[0].content
        );
        assert!(
            results[0].content.contains("use bash"),
            "{}",
            results[0].content
        );
    }

    /// Nothing is suppressed unless the bytes are genuinely still above: a
    /// file that changed between reads must come back whole, or the model is
    /// told it has content that no longer exists.
    #[test]
    fn a_changed_file_is_never_answered_with_a_pointer() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "src/main.rs")];
        let mut results = vec![ToolResult {
            content: "fn main() { changed() }".to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", duplicate_payload());

        assert_eq!(
            suppress_duplicate_reads(&registry(), cwd, &calls, &mut results, &history),
            0
        );
        assert_eq!(results[0].content, "fn main() { changed() }");
    }

    /// The same file under a different spelling is the same file: the guard
    /// keys on the resolved path, so `./src/main.rs` and `src/main.rs` meet.
    #[test]
    fn the_guard_keys_on_the_resolved_path() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "./src/../src/main.rs")];
        let mut results = vec![ToolResult {
            content: duplicate_payload().to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", duplicate_payload());

        assert_eq!(
            suppress_duplicate_reads(&registry(), cwd, &calls, &mut results, &history),
            1
        );
    }

    /// Re-reading to refresh attention is a real behaviour, so the payload is
    /// taxed rather than forbidden: `force` gets it back untouched.
    #[test]
    fn force_brings_the_payload_back() {
        let cwd = Path::new("/repo");
        let mut call = read_call("now", "src/main.rs");
        call.arguments = serde_json::json!({ "path": "src/main.rs", "force": true });
        let calls = vec![call];
        let mut results = vec![ToolResult {
            content: duplicate_payload().to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", duplicate_payload());

        assert_eq!(
            suppress_duplicate_reads(&registry(), cwd, &calls, &mut results, &history),
            0
        );
        assert_eq!(results[0].content, duplicate_payload());
    }

    /// Two identical reads in one batch run concurrently, so neither can see
    /// the other in the transcript. The second is still answered with a
    /// pointer, because the first one's payload is in the batch's results.
    #[test]
    fn a_twin_read_in_one_batch_is_answered_with_a_pointer_too() {
        let cwd = Path::new("/repo");
        let calls = vec![
            read_call("first", "src/main.rs"),
            read_call("second", "src/main.rs"),
        ];
        let mut results = vec![
            ToolResult {
                content: duplicate_payload().to_string(),
                is_error: false,
            },
            ToolResult {
                content: duplicate_payload().to_string(),
                is_error: false,
            },
        ];

        let suppressed = suppress_duplicate_reads(&registry(), cwd, &calls, &mut results, &[]);

        assert_eq!(suppressed, 1);
        assert_eq!(results[0].content, duplicate_payload());
        assert!(
            results[1].content.contains("not re-sent"),
            "{}",
            results[1].content
        );
    }

    /// A failed read carries no payload worth pointing at, and a pointer to a
    /// failure would read as one more failure.
    #[test]
    fn a_failed_read_is_never_answered_with_a_pointer() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "src/main.rs")];
        let mut results = vec![ToolResult {
            content: "file not found: /repo/src/main.rs".to_string(),
            is_error: true,
        }];
        let history = transcript_with_one_read("src/main.rs", "file not found: /repo/src/main.rs");

        assert_eq!(
            suppress_duplicate_reads(&registry(), cwd, &calls, &mut results, &history),
            0
        );
    }
}
