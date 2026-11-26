//! One batch of tool calls, and the locks that keep two calls on one file from
//! losing each other.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures::future::join_all;
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};

use crate::messages::ToolCall;
use crate::path::{normalize, resolve_path};
use crate::tools::{AgentContext, ToolRegistry, ToolResult};

/// One tool call: its result, and the span it covered.
///
/// The span is taken here and nowhere else. What the trace prints beside a call
/// and what the ledger records about the batch are both read from these
/// measurements, so a second clock beside them would only be a second number to
/// keep in agreement. It runs from the moment the batch admits the call —
/// which includes waiting for an earlier call's lock on the same file, because
/// that wait is part of how long the call took.
pub struct Called {
    pub result: ToolResult,
    started: Instant,
    pub elapsed: Duration,
}

impl Called {
    /// When the call stopped, reconstructed from its own measurement.
    fn ended(&self) -> Instant {
        self.started + self.elapsed
    }
}

/// How long the batch as a whole took: from the first call's start to the last
/// call's end. Calls run at once, so adding their spans together would count
/// the same seconds twice; the span between the ends is what the batch cost.
pub fn span(calls: &[Called]) -> Duration {
    match (
        calls.iter().map(|call| call.started).min(),
        calls.iter().map(Called::ended).max(),
    ) {
        (Some(first), Some(last)) => last.saturating_duration_since(first),
        _ => Duration::ZERO,
    }
}

/// Runs one batch of tool calls. Calls that name the same file take turns in
/// the order they were sent: each holds that file's lock from the moment it is
/// taken until its call finishes, so the next call on that file reads what the
/// previous one wrote. Calls on different files, and tools that cannot be
/// reasoned about by path (bash, elisp), still run together.
pub async fn run(tool_registry: ToolRegistry, tool_calls: &[ToolCall], cwd: &Path) -> Vec<Called> {
    let mut locks: HashMap<PathBuf, Arc<Mutex<()>>> = HashMap::new();
    // The baton for each file: the previous call on it sends here when done.
    // A task waits on its baton *inside* its task, so waiting never delays the
    // spawning of later calls on other files.
    let mut turns: HashMap<PathBuf, oneshot::Receiver<()>> = HashMap::new();
    let mut handles = Vec::with_capacity(tool_calls.len());

    for tc in tool_calls {
        let name = tc.name.clone();
        let arguments = tc.arguments.clone();
        // A call whose arguments never decoded cannot be run, but it can be
        // answered. Reported as a tool failure it reaches the model as a
        // result it can act on, instead of ending the run.
        let parse_error = tc.parse_error.clone();
        let ctx = AgentContext {
            cwd: cwd.to_path_buf(),
        };
        let registry = tool_registry.clone();
        // The clock starts before the wait: a call queued behind another on
        // this file spent that time waiting, and the span has to say so.
        let started = Instant::now();
        let key = file_key(&tool_registry, cwd, &tc.name, &tc.arguments);
        let turn = key.as_ref().and_then(|path| turns.remove(path));
        let (done, next) = oneshot::channel();
        if let Some(path) = &key {
            turns.insert(path.clone(), next);
        }
        let lock = key.map(|path| locks.entry(path).or_default().clone());
        // The guard belongs to this call's task, so the next call on the same
        // file cannot take the lock until this one has finished.
        handles.push(tokio::spawn(async move {
            // Wait for the earlier call on this file, not for the loop that
            // spawns the rest of the batch.
            if let Some(turn) = turn {
                let _ = turn.await;
            }
            let _held = match lock {
                Some(lock) => Some(lock.lock_owned().await),
                None => None,
            };
            // An error rather than an early return: the span has to come
            // back with the outcome, so every path out of this task is the
            // same shape.
            let outcome = match parse_error {
                Some(err) => Err(anyhow!(
                    "arguments were not valid JSON ({err}). Send the call \
                     again with complete, valid JSON arguments."
                )),
                None => registry.execute(&name, arguments, &ctx).await,
            };
            let _ = done.send(());
            (outcome, started, started.elapsed())
        }));
    }

    join_all(handles)
        .await
        .into_iter()
        .map(|res| match res {
            Ok((Ok(result), started, elapsed)) => Called {
                result,
                started,
                elapsed,
            },
            Ok((Err(e), started, elapsed)) => Called {
                result: ToolResult {
                    content: format!("Tool execution failed: {e:#}"),
                    is_error: true,
                },
                started,
                elapsed,
            },
            // The task itself died, so nothing ran to completion and there is
            // no span to keep: it is filed at the point the failure was seen,
            // rather than given a time it did not take.
            Err(join_err) => Called {
                result: ToolResult {
                    content: format!("Tool task failed: {join_err}"),
                    is_error: true,
                },
                started: Instant::now(),
                elapsed: Duration::ZERO,
            },
        })
        .collect()
}

/// The lock key for a tool call: the file it names, resolved and normalized,
/// for a tool that operates on one path. `None` for the rest, and for a name no
/// tool in the registry answers to.
///
/// The tool decides — [`crate::tools::Tool::lock_path`] — because the key has to
/// agree with the argument that tool actually reads. A name-keyed table here
/// would be a second place to teach, and the first place to forget.
pub fn file_key(
    registry: &ToolRegistry,
    cwd: &Path,
    name: &str,
    arguments: &Value,
) -> Option<PathBuf> {
    let field = registry.get(name)?.lock_path()?;
    let raw = arguments.get(field)?.as_str()?;
    Some(normalize(&resolve_path(cwd, raw)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn project_dir(name: &str) -> PathBuf {
        crate::config::test_dir(name, None)
    }

    fn called(started: Instant, elapsed: Duration) -> Called {
        Called {
            result: ToolResult {
                content: String::new(),
                is_error: false,
            },
            started,
            elapsed,
        }
    }

    /// Calls run at once, so the batch's span is the distance between the
    /// earliest start and the latest end — not the sum, which would count the
    /// same seconds once per overlapping call.
    #[test]
    fn the_batch_span_runs_from_the_first_start_to_the_last_end() {
        let base = Instant::now();
        let calls = vec![
            called(base, Duration::from_millis(500)),
            called(
                base + Duration::from_millis(300),
                Duration::from_millis(800),
            ),
        ];

        assert_eq!(span(&calls), Duration::from_millis(1_100));
    }

    /// A batch that ran nothing has no span, rather than a panic or a
    /// borrowed number from somewhere else.
    #[test]
    fn an_empty_batch_has_no_span() {
        assert_eq!(span(&[]), Duration::ZERO);
    }

    /// Two calls that name one file through different spellings take the same
    /// lock, and the one sent first wins: the second finds its anchor gone.
    #[tokio::test]
    async fn separate_edits_to_one_file_still_take_the_file_lock() {
        let dir = project_dir("edit-call-lock");
        fs::write(dir.join("note.txt"), "foo").unwrap();

        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();
        let calls = vec![
            ToolCall {
                id: "call-1".to_string(),
                name: "edit".to_string(),
                arguments: serde_json::json!({
                    "path": "note.txt",
                    "old_str": "foo",
                    "new_str": "bar"
                }),
                parse_error: None,
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "edit".to_string(),
                arguments: serde_json::json!({
                    "path": "sub/../note.txt",
                    "old_str": "foo",
                    "new_str": "baz"
                }),
                parse_error: None,
            },
        ];

        let results = run(registry, &calls, &dir).await;
        let errors = results.iter().filter(|call| call.result.is_error).count();
        let final_content = fs::read_to_string(dir.join("note.txt")).unwrap();

        assert_eq!(errors, 1);
        assert_eq!(final_content, "bar");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Five calls to one file in one batch: each runs in its own task, they take
    /// turns behind the file's lock, and every change lands. Without the lock each
    /// would read the original and the last write would drop the other four.
    #[tokio::test]
    async fn five_edit_calls_to_one_file_all_land() {
        let dir = project_dir("five-edits-one-file");
        fs::write(dir.join("note.txt"), "l0\nl1\nl2\nl3\nl4\n").unwrap();

        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();
        let calls = (0..5)
            .map(|n| ToolCall {
                id: format!("call-{n}"),
                name: "edit".to_string(),
                arguments: serde_json::json!({
                    "path": "note.txt",
                    "old_str": format!("l{n}\n"),
                    "new_str": format!("L{n}\n")
                }),
                parse_error: None,
            })
            .collect::<Vec<_>>();

        let results = run(registry, &calls, &dir).await;
        let errors = results.iter().filter(|call| call.result.is_error).count();

        assert_eq!(errors, 0);
        assert_eq!(
            fs::read_to_string(dir.join("note.txt")).unwrap(),
            "L0\nL1\nL2\nL3\nL4\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A later call can anchor on what an earlier one wrote: calls take the
    /// file's lock in the order sent, not in whichever task arrives first.
    #[tokio::test]
    async fn a_later_edit_can_anchor_on_what_an_earlier_one_wrote() {
        let dir = project_dir("ordered-edits");
        fs::write(dir.join("note.txt"), "foo\n").unwrap();

        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();
        let calls = vec![
            ToolCall {
                id: "call-1".to_string(),
                name: "edit".to_string(),
                arguments: serde_json::json!({
                    "path": "note.txt",
                    "old_str": "foo",
                    "new_str": "bar"
                }),
                parse_error: None,
            },
            ToolCall {
                id: "call-2".to_string(),
                name: "edit".to_string(),
                arguments: serde_json::json!({
                    "path": "note.txt",
                    "old_str": "bar",
                    "new_str": "baz"
                }),
                parse_error: None,
            },
        ];

        let results = run(registry, &calls, &dir).await;
        let errors = results.iter().filter(|call| call.result.is_error).count();

        assert_eq!(errors, 0);
        assert_eq!(fs::read_to_string(dir.join("note.txt")).unwrap(), "baz\n");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A tool with a file lock that runs long enough to queue a second call
    /// on that file, and marks the moment it finished.
    struct SlowFile(Arc<AtomicBool>);

    #[async_trait]
    impl Tool for SlowFile {
        fn name(&self) -> &'static str {
            "slowfile"
        }
        fn description(&self) -> String {
            "a slow file tool".to_string()
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn lock_path(&self) -> Option<&'static str> {
            Some("path")
        }
        async fn execute(&self, _input: Value, _ctx: &AgentContext) -> Result<ToolResult> {
            tokio::time::sleep(Duration::from_millis(150)).await;
            self.0.store(true, Ordering::SeqCst);
            Ok(ToolResult {
                content: String::new(),
                is_error: false,
            })
        }
    }

    /// A tool with no lock that says whether the slow call had finished when
    /// it got to run — what a bash or elisp call would report here.
    struct FreeProbe(Arc<AtomicBool>);

    #[async_trait]
    impl Tool for FreeProbe {
        fn name(&self) -> &'static str {
            "freeprobe"
        }
        fn description(&self) -> String {
            "an unlockable probe".to_string()
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _input: Value, _ctx: &AgentContext) -> Result<ToolResult> {
            Ok(ToolResult {
                content: if self.0.load(Ordering::SeqCst) {
                    "late".to_string()
                } else {
                    "early".to_string()
                },
                is_error: false,
            })
        }
    }

    /// Waiting for a file lock happens inside the queued task, not in the loop
    /// that spawns the rest of the batch: a call naming no file still starts
    /// while an earlier call on one file is running.
    #[tokio::test]
    async fn a_queued_file_call_does_not_hold_up_unlockable_calls() {
        let finished = Arc::new(AtomicBool::new(false));
        let registry = ToolRegistry::from_tools(vec![
            Arc::new(SlowFile(Arc::clone(&finished))),
            Arc::new(FreeProbe(Arc::clone(&finished))),
        ]);
        let calls = (0..2)
            .map(|n| ToolCall {
                id: format!("slow-{n}"),
                name: "slowfile".to_string(),
                arguments: json!({"path": "note.txt"}),
                parse_error: None,
            })
            .chain(std::iter::once(ToolCall {
                id: "probe".to_string(),
                name: "freeprobe".to_string(),
                arguments: json!({}),
                parse_error: None,
            }))
            .collect::<Vec<_>>();

        let results = run(registry, &calls, Path::new("/tmp")).await;

        assert_eq!(results[2].result.content, "early");
    }

    /// The lock key is the tool's own answer: the argument it reads, resolved
    /// against the working directory. A tool that names no file takes no lock,
    /// and a name nothing answers to is not a file either.
    #[test]
    fn the_lock_key_is_the_argument_the_tool_names() {
        let dir = project_dir("lock-key");
        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();

        assert_eq!(
            file_key(
                &registry,
                &dir,
                "read",
                &serde_json::json!({"path": "sub/../a.txt"})
            ),
            Some(dir.join("a.txt"))
        );
        assert!(
            file_key(
                &registry,
                &dir,
                "bash",
                &serde_json::json!({"command": "ls"})
            )
            .is_none()
        );
        assert!(
            file_key(
                &registry,
                &dir,
                "nonesuch",
                &serde_json::json!({"path": "a.txt"})
            )
            .is_none()
        );
        assert_eq!(registry.get("edit").and_then(Tool::lock_path), Some("path"));
        let _ = fs::remove_dir_all(&dir);
    }
}
