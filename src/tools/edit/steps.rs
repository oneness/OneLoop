//! The steps an edit takes after its arguments are checked: read the file,
//! locate the anchor, and render what landed.
//!
//! Kept apart from the tool so a call reads as the steps it takes. The report is
//! rendered before the splice removes the bytes a fallback quoted.

use std::path::Path;

use anyhow::Result;

use super::Fault;
use super::matcher;
use super::refusal::Refusal;
use super::show;

/// The anchor that landed: the pass that found it and every byte range it covers.
/// One range for a single edit, all of them for `replace_all`.
#[derive(Debug, Clone)]
pub(super) struct Found {
    pub(super) pass: &'static str,
    pub(super) spans: Vec<matcher::Span>,
}

impl Found {
    /// Whether the model already knows what landed here: an exact anchor *is* the
    /// bytes it sent, so a report would only repeat them, while a fallback
    /// matched something it did not send.
    fn is_fallback(&self) -> bool {
        self.pass != matcher::EXACT
    }
}

/// The file's contents, with a missing or undecodable file refused and anything
/// else a failure. One call, not an existence check beside it: two answers to one
/// question leave a window in which the file can go away.
pub(super) async fn read_text(path: &Path) -> Result<String, Fault> {
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

/// The anchor's place in the file, or the refusal that says why it names none.
pub(super) fn locate(content: &str, old_str: &str, path: &Path) -> Result<Found, Refusal> {
    match matcher::locate(content, old_str) {
        matcher::Located::Found {
            matcher: pass,
            span,
        } => Ok(Found {
            pass,
            spans: vec![span],
        }),
        matcher::Located::Ambiguous {
            matcher: pass,
            places,
            lines,
        } => {
            let shown = show::candidates(content, &lines);
            Err(Refusal::ambiguous(
                path,
                pass,
                places,
                &shown.lines,
                shown.hidden,
            ))
        }
        matcher::Located::Miss => Err(missed(content, path, old_str)),
    }
}

/// Every place named by `old_str`, or the refusal that says why it names no
/// writable set. Used only for `replace_all`, where several places are the work
/// rather than a refusal.
pub(super) fn locate_all(content: &str, old_str: &str, path: &Path) -> Result<Found, Refusal> {
    match matcher::locate_all(content, old_str) {
        matcher::LocatedAll::Found {
            matcher: pass,
            spans,
        } => Ok(Found { pass, spans }),
        matcher::LocatedAll::Ambiguous {
            matcher: pass,
            places,
            lines,
        } => {
            let shown = show::candidates(content, &lines);
            Err(Refusal::overlapping(
                path,
                pass,
                places,
                &shown.lines,
                shown.hidden,
            ))
        }
        matcher::LocatedAll::Miss => Err(missed(content, path, old_str)),
    }
}

/// The refusal an anchor earns for naming no place, with the lines that almost
/// carried it.
fn missed(content: &str, path: &Path, old_str: &str) -> Refusal {
    Refusal::miss(
        path,
        &show::hint(content, &matcher::probes(content, old_str)),
    )
}

/// What the tool did, in the words the model reads: the pass that matched, the
/// places a `replace_all` rewrote, and — for a fallback — the region it changed.
pub(super) fn report(
    path: &Path,
    before: &str,
    found: &Found,
    text: &str,
    replace_all: bool,
) -> String {
    let mut report = format!("applied edit to {}: {}", path.display(), found.pass);
    if replace_all {
        let places = found.spans.len();
        let word = if places == 1 { "place" } else { "places" };
        report.push_str(&format!(" in {places} {word}"));
    }
    if let Some(region) = quoted(before, found, text) {
        report.push_str(":\n");
        report.push_str(&region);
    }
    report
}

/// The evidence a fallback owes the model — the regions it really matched —
/// and nothing for an exact anchor, which is the bytes the model sent.
fn quoted(original: &str, found: &Found, text: &str) -> Option<String> {
    found.is_fallback().then(|| {
        found
            .spans
            .iter()
            .map(|span| show::change(original, span, text))
            .collect::<Vec<_>>()
            .join("\n")
    })
}
