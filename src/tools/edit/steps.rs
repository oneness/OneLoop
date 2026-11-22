//! The steps an edit takes after its arguments are checked: read the file,
//! locate an anchor in the running copy, and render what landed.
//!
//! Kept apart from the tool so a call reads as the steps it takes. Every edit
//! sees the bytes the one before it wrote, and a receipt is rendered before the
//! splice removes the anchor it quotes.

use std::path::Path;

use anyhow::Result;

use super::Fault;
use super::matcher;
use super::refusal::Refusal;
use super::show;

/// The most lines one result spends on receipts. The result is re-sent on every
/// later turn, so what does not fit is counted rather than dropped in silence.
const RECEIPT_LINES: usize = 24;

/// An anchor that landed: the pass that found it and every byte range it covers.
/// One range for a single edit, all of them for `replace_all`.
#[derive(Debug, Clone)]
pub(super) struct Found {
    pub(super) pass: &'static str,
    pub(super) spans: Vec<matcher::Span>,
}

impl Found {
    /// Whether the model already knows what landed here: an exact anchor *is* the
    /// bytes it sent, so a receipt would only repeat them, while a fallback
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

/// The anchor's place in the running copy, or the refusal that says why it names
/// none. Every matcher reads that copy, and one refusal is final: nothing is
/// written unless every anchor names one place.
pub(super) fn locate(
    content: &str,
    old_str: &str,
    path: &Path,
    at: usize,
) -> Result<Found, Refusal> {
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
                at,
                path,
                pass,
                places,
                &shown.lines,
                shown.hidden,
            ))
        }
        matcher::Located::Miss => Err(missed(content, path, at, old_str)),
    }
}

/// Every place named by `old_str`, or the refusal that says why it names no
/// writable set. Used only for `replace_all`, where several places are the work
/// rather than a refusal.
pub(super) fn locate_all(
    content: &str,
    old_str: &str,
    path: &Path,
    at: usize,
) -> Result<Found, Refusal> {
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
                at,
                path,
                pass,
                places,
                &shown.lines,
                shown.hidden,
            ))
        }
        matcher::LocatedAll::Miss => Err(missed(content, path, at, old_str)),
    }
}

/// The refusal an anchor earns for naming no place, with the lines that almost
/// carried it.
fn missed(content: &str, path: &Path, at: usize, old_str: &str) -> Refusal {
    Refusal::miss(
        at,
        path,
        &show::hint(content, &matcher::probes(content, old_str)),
    )
}

/// One edit's line in the result: the position it was sent at and the pass that
/// carried it. A `replace_all` edit also names the places it rewrote, including
/// one, so the model can tell the flag was honoured. A fallback quotes the region
/// it matched underneath.
pub(super) fn receipt(
    at: usize,
    before: &str,
    found: &Found,
    text: &str,
    replace_all: bool,
) -> String {
    let landed = if replace_all {
        let places = found.spans.len();
        let word = if places == 1 { "place" } else { "places" };
        format!("edits[{at}]: {} in {places} {word}", found.pass)
    } else {
        format!("edits[{at}]: {}", found.pass)
    };
    match quoted(before, found, text) {
        Some(region) => format!("{landed}:\n{region}"),
        None => landed,
    }
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

/// What the tool did, in the words the model reads: how many edits landed and
/// where, then one receipt per edit that fits. The result is re-sent on every
/// later turn, so the receipts are bounded in lines — and the edits that do not
/// fit are counted as applied, so a cut is never read as work still to do.
pub(super) fn report(path: &Path, receipts: &[String]) -> String {
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
        report.push_str(&format!("\n… and {rest} more {} applied", noun(rest)));
    }
    report
}

/// `edit` for one, `edits` for the rest: the counts in a receipt are read aloud,
/// so they have to agree with what they count.
fn noun(count: usize) -> &'static str {
    if count == 1 { "edit" } else { "edits" }
}
