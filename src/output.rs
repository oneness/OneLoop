use std::sync::OnceLock;
use std::time::Duration;

pub const DEFAULT_MAX_BYTES: usize = 128 * 1024;
pub const DEFAULT_MAX_LINES: usize = 1000;

// ── Terminal output ───────────────────────────────────────────────────
//
// The single home for ANSI escapes — CSI and OSC — and for the shape of a
// status line: a color-scheme change, a `--no-color` flag, or `NO_COLOR`
// support only ever touches this block.
//
// The streams are split by what the text *is*, not by how bad it is:
// **stdout carries the model's answer and nothing else**, so
// `ol "..." > answer.md` captures the answer while the tool trace, the
// warnings, and the errors stay on the terminal where they are read.
// Everything OneLoop says about itself goes to stderr.

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[90m";
/// Erase the current line (spinner redraws over it).
pub const CLEAR_LINE: &str = "\x1b[2K";

/// An OSC string — `ESC ] n ; body ST`. The framing every non-CSI escape this
/// project writes shares, and the one thing about an OSC that is terminal
/// syntax rather than protocol: what a sequence number *means* belongs to
/// whoever speaks it, as [`crate::status`] does for 7501, but how one is
/// opened and terminated belongs here beside the CSI escapes above.
pub fn osc(sequence: u16, body: &str) -> String {
    format!("\x1b]{sequence};{body}\x1b\\")
}

// Private: what an alarm looks like is this module's to decide, and a call
// site that could reach for red is a call site that can drift.
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";

/// True when the terminal cannot take cursor-addressing output. Comint runs
/// its children with `TERM=dumb` and marks them in `INSIDE_EMACS`; there a
/// spinner's per-frame rewrite is not animation but a firehose that Emacs's
/// single thread grinds on in `comint-output-filter`, even when the buffer
/// is not displayed. Checked once — one process has one terminal.
pub fn plain() -> bool {
    static PLAIN: OnceLock<bool> = OnceLock::new();
    *PLAIN.get_or_init(|| {
        plain_terminal(
            std::env::var("INSIDE_EMACS").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    })
}

/// The pure half of [`plain`], split out so the predicate is testable
/// without touching process environment. `INSIDE_EMACS` is a comma-separated
/// token list, so `comint` must be a whole token.
fn plain_terminal(inside_emacs: Option<&str>, term: Option<&str>) -> bool {
    inside_emacs_has(inside_emacs, "comint") || term.is_some_and(|term| term == "dumb")
}

/// True only under comint — the one consumer of the OSC 7501 reports that
/// [`crate::status`] emits. A dumb terminal outside Emacs has no consumer,
/// and there the spinner suppression is all the state a user gets.
pub fn comint() -> bool {
    static COMINT: OnceLock<bool> = OnceLock::new();
    *COMINT
        .get_or_init(|| inside_emacs_has(std::env::var("INSIDE_EMACS").ok().as_deref(), "comint"))
}

/// Whole-token check against the comma-separated `INSIDE_EMACS` list.
fn inside_emacs_has(inside_emacs: Option<&str>, feature: &str) -> bool {
    inside_emacs.is_some_and(|inside| inside.split(',').any(|token| token == feature))
}

/// Two-space indent, an optional glyph, then the message — the one shape
/// every status line has.
fn line(style: &str, glyph: &str, message: &str) -> String {
    if glyph.is_empty() {
        format!("{style}  {message}{RESET}")
    } else {
        format!("{style}  {glyph} {message}{RESET}")
    }
}

/// Something failed. Red `✗`.
pub fn fail(message: &str) {
    eprintln!("{}", line(RED, "✗", message));
}

/// Something wants attention, but the run goes on. Yellow `⚠`.
pub fn warn(message: &str) {
    eprintln!("{}", line(YELLOW, "⚠", message));
}

/// The user interrupted the run. Yellow `⏹`.
pub fn stopped(message: &str) {
    eprintln!("{}", line(YELLOW, "⏹", message));
}

/// Something the user asked for succeeded. Green `✓`.
pub fn ok(message: &str) {
    eprintln!("{}", line(GREEN, "✓", message));
}

/// A step of the running trace finished. Dim `✓` — a trace should recede,
/// which is what separates this from [`ok`].
pub fn tick(message: &str) {
    eprintln!("{}", line(DIM, "✓", message));
}

/// A step that has started and has not finished, named while it is still
/// running: the request being made, or the call being executed. Dim `▸`.
pub fn running(message: &str) {
    eprintln!("{}", line(DIM, "▸", message));
}

/// Something happened that is worth mentioning in passing. Dim `→`.
pub fn step(message: &str) {
    eprintln!("{}", line(DIM, "→", message));
}

/// An aside — dim, and no glyph, because it is usually a second line
/// qualifying the one above it.
pub fn note(message: &str) {
    eprintln!("{}", line(DIM, "", message));
}

/// How long a step took, as a trace line says it: `0.4s`, `12s`, `2m 05s`.
///
/// A tenth below ten seconds, where a tool call is worth reading to a tenth;
/// whole seconds above it, where the digit stops being something a reader acts
/// on. Minutes are spelled as minutes, because `134s` is a number nobody can
/// picture. One helper, so a duration is spelled one way wherever it appears.
pub fn elapsed(took: Duration) -> String {
    let seconds = took.as_secs_f64();
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else if seconds < 60.0 {
        // Truncated, not rounded: rounding would let 59.6s print as `60s`,
        // one line before the same duration prints as `1m 00s`.
        format!("{}s", took.as_secs())
    } else {
        format!("{}m {:02}s", took.as_secs() / 60, took.as_secs() % 60)
    }
}

/// A section heading above an interactive block: `── message ──`.
pub fn head(message: &str) {
    eprintln!("{BOLD}  ── {message} ──{RESET}");
}

/// Truncate keeping the first lines, appending a notice when content was dropped.
pub fn truncate_head(input: &str, max_bytes: usize, max_lines: usize) -> String {
    with_notice(truncate(input, max_bytes, max_lines, Keep::Head))
}

/// Truncate keeping the last lines, appending a notice when content was dropped.
pub fn truncate_tail(input: &str, max_bytes: usize, max_lines: usize) -> String {
    with_notice(truncate(input, max_bytes, max_lines, Keep::Tail))
}

/// One line-window of a file, head-truncated to the byte cap, with a notice
/// that names the absolute range and the `offset` that continues from it.
///
/// `first` and `last` are 1-based and inclusive, in the *file's* numbering
/// rather than the window's: a resume hint is only usable if `offset` means
/// what it will mean on the next call. `file_lines` is the whole file, so the
/// notice can say how much of it is still unseen.
///
/// A window that stops short of the cap because the caller asked for a small
/// range is not "truncated" — it is exactly what was requested — but it still
/// gets the hint, because the model's next question is always the same one.
/// Every notice it can end with is recognised by [`window_notice`].
pub fn truncate_window(file_lines: &[&str], first: usize, last: usize, max_bytes: usize) -> String {
    let total = file_lines.len();
    // No caller produces a window that names no line, but an empty payload
    // with no explanation is the one answer that leaves the model worse off
    // than a sentence saying why there is nothing to show.
    if first == 0 || last < first || first > total {
        return format!(
            "[no lines: the window {first}-{last} names nothing in a {total}-line file]"
        );
    }

    let mut shown = (last - first + 1).min(total - first + 1);
    let window = &file_lines[first - 1..first - 1 + shown];
    while shown > 0 && joined_len(&window[..shown]) > max_bytes {
        shown -= 1;
    }

    // One line wider than the whole byte budget. Dropping it would hand back
    // an empty result and a resume offset that never advances — the next call
    // would ask for the same line and get the same nothing — so return the
    // widest slice that fits and say plainly that no window reaches the rest.
    // It opens the way every notice here does, so [`window_notice`] can tell
    // it apart from file content.
    if shown == 0 {
        return format!(
            "{}\n[output truncated at the {max_bytes}-byte cap: line {first} alone exceeds \
             it, so no line window reaches the rest · use bash to view it in pieces]",
            truncate_at_char_boundary(file_lines[first - 1], max_bytes)
        );
    }

    let mut content = window[..shown].join("\n");
    let last_shown = first + shown - 1;
    if shown < window.len() {
        content.push_str(&format!(
            "\n[output truncated at the {max_bytes}-byte cap: showing lines \
             {first}-{last_shown} of {total} · re-read with offset={}]",
            last_shown + 1
        ));
    } else if last_shown < total {
        content.push_str(&format!(
            "\n[showing lines {first}-{last_shown} of {total} · continue with offset={}]",
            last_shown + 1
        ));
    }
    content
}

/// How every notice [`truncate_window`] appends opens: a payload whose last
/// line starts with one is telling the model where the rest of the file is.
/// `[showing lines` is the range hint for a window that stopped where it was
/// asked to; the rest are the byte cap, which is the only thing that ever cuts
/// content short.
const NOTICE_OPENERS: [&str; 2] = ["[output truncated", "[showing lines"];

/// The resume hint a window payload ends with, or `None` when it ends in file
/// content.
///
/// A caller that wants to carry the hint somewhere else asks here rather than
/// matching the text at a distance, so a notice shape added to
/// [`truncate_window`] is recognised everywhere by being written once.
pub fn window_notice(line: &str) -> Option<&str> {
    NOTICE_OPENERS
        .iter()
        .any(|opener| line.starts_with(opener))
        .then_some(line)
}

/// Longest suffix of `input` at most `max_bytes` long that starts on a UTF-8
/// character boundary. The counterpart of [`truncate_at_char_boundary`], for a
/// payload whose end is the part worth keeping.
pub fn truncate_tail_at_char_boundary(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut start = input.len() - max_bytes;
    while start < input.len() && !input.is_char_boundary(start) {
        start += 1;
    }
    &input[start..]
}

/// Longest prefix of `input` at most `max_bytes` long that ends on a UTF-8
/// character boundary — plain byte-index slicing panics mid-character.
pub fn truncate_at_char_boundary(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

struct TruncationResult {
    content: String,
    truncated: bool,
    original_bytes: usize,
    original_lines: usize,
    shown_bytes: usize,
    shown_lines: usize,
}

#[derive(Clone, Copy)]
enum Keep {
    Head,
    Tail,
}

fn truncate(input: &str, max_bytes: usize, max_lines: usize, keep: Keep) -> TruncationResult {
    let lines: Vec<&str> = input.lines().collect();
    let original_bytes = input.len();
    let original_lines = lines.len();
    if original_bytes <= max_bytes && original_lines <= max_lines {
        return TruncationResult {
            content: input.to_string(),
            truncated: false,
            original_bytes,
            original_lines,
            shown_bytes: original_bytes,
            shown_lines: original_lines,
        };
    }

    let mut chosen: Vec<&str> = match keep {
        Keep::Head => lines.iter().copied().take(max_lines).collect(),
        Keep::Tail => {
            let start = original_lines.saturating_sub(max_lines);
            lines.iter().copied().skip(start).collect()
        }
    };

    while joined_len(&chosen) > max_bytes && !chosen.is_empty() {
        match keep {
            Keep::Head => {
                chosen.pop();
            }
            Keep::Tail => {
                chosen.remove(0);
            }
        }
    }

    let mut content = chosen.join("\n");
    if chosen.is_empty() && original_lines > 0 && max_lines > 0 && max_bytes > 0 {
        let line = match keep {
            Keep::Head => lines[0],
            Keep::Tail => lines[original_lines - 1],
        };
        content = match keep {
            Keep::Head => truncate_at_char_boundary(line, max_bytes).to_string(),
            Keep::Tail => truncate_tail_at_char_boundary(line, max_bytes).to_string(),
        };
    }
    let shown_bytes = content.len();
    let shown_lines = if chosen.is_empty() {
        usize::from(!content.is_empty())
    } else {
        chosen.len()
    };
    let truncated = shown_bytes < original_bytes || original_lines > max_lines;

    TruncationResult {
        content,
        truncated,
        original_bytes,
        original_lines,
        shown_bytes,
        shown_lines,
    }
}

fn joined_len(lines: &[&str]) -> usize {
    if lines.is_empty() {
        0
    } else {
        lines.iter().map(|line| line.len()).sum::<usize>() + (lines.len() - 1)
    }
}

fn with_notice(result: TruncationResult) -> String {
    if !result.truncated {
        return result.content;
    }

    let mut content = result.content;
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&format!(
        "[output truncated: showing {} of {} lines, {} of {} bytes]",
        result.shown_lines, result.original_lines, result.shown_bytes, result.original_bytes
    ));
    content
}

// ── Trace lines ───────────────────────────────────────────────────────
//
// A tool names its own call — [`crate::tools::Tool::trace_label`] decides
// what that call reads like — and these are the two shapes it can reach for:
// a path stays whole, a command or an expression is cut. They live here
// because they are rendering, and because a tool reaching into the agent for
// them would point the dependency the wrong way.

/// How much of a command or expression the trace shows: enough of it to
/// recognise what ran, not enough to fill the screen with a here-document.
pub const TRACE_WIDTH: usize = 240;

/// What a cut line ends with. One marker and one spelling, so a reader never
/// has to tell an ellipsis from three dots.
pub const ELISION: &str = "...";

/// One line, always: a multi-line argument would otherwise scroll the
/// spinner's line away mid-run.
pub fn first_line(argument: &str) -> String {
    match argument.split_once('\n') {
        Some((first, _)) => format!("{}{ELISION}", first.trim_end()),
        None => argument.to_string(),
    }
}

/// A command or an expression as at most one line of [`TRACE_WIDTH`]
/// characters. The trace records that a call ran, not the call itself — and
/// the model's own words can be a 1,800-character here-document or a whole
/// elisp program. Newlines become spaces so the budget measures the argument
/// rather than its first line, and whatever is dropped — trailing characters,
/// or the line breaks that were flattened — ends the line with [`ELISION`].
pub fn elide(command: &str) -> String {
    let one_line = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let shown: String = one_line.chars().take(TRACE_WIDTH).collect();
    if shown == one_line && one_line == command.trim() {
        return one_line;
    }
    format!("{}{ELISION}", shown.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_original_when_input_fits_limits() {
        let input = "one\r\ntwo\r\n";
        assert_eq!(truncate_head(input, 64, 4), input);
        assert_eq!(truncate_tail(input, 64, 4), input);
    }

    #[test]
    fn head_truncation_slices_a_single_oversized_line() {
        let output = truncate_head("0123456789", 4, 10);
        assert_eq!(
            output,
            "0123\n[output truncated: showing 1 of 1 lines, 4 of 10 bytes]"
        );
    }

    #[test]
    fn tail_truncation_slices_a_single_oversized_line_from_the_end() {
        let output = truncate_tail("0123456789", 4, 10);
        assert_eq!(
            output,
            "6789\n[output truncated: showing 1 of 1 lines, 4 of 10 bytes]"
        );
    }

    #[test]
    fn oversized_line_truncation_preserves_utf8_boundaries() {
        assert_eq!(
            truncate_head("aéé", 2, 10),
            "a\n[output truncated: showing 1 of 1 lines, 1 of 5 bytes]"
        );
    }

    /// A window that stops early must say where to resume, in the file's own
    /// numbering — an offset the model cannot act on is worse than no hint.
    #[test]
    fn a_window_notice_names_the_offset_that_continues_it() {
        let lines = ["a", "b", "c", "d"];
        let window = truncate_window(&lines, 2, 3, 1024);
        assert_eq!(
            window,
            "b\nc\n[showing lines 2-3 of 4 · continue with offset=4]"
        );
    }

    /// The last window is the end of the file: no notice, and so no invitation
    /// to ask for lines that do not exist.
    #[test]
    fn a_window_that_reaches_the_end_carries_no_notice() {
        let lines = ["a", "b", "c", "d"];
        assert_eq!(truncate_window(&lines, 3, 4, 1024), "c\nd");
    }

    /// The byte cap outranks the requested window, and the notice says which
    /// of the two stopped it.
    #[test]
    fn a_byte_capped_window_says_so_and_still_offers_a_resume() {
        let lines = ["aaaa", "bbbb", "cccc"];
        let window = truncate_window(&lines, 1, 3, 9);
        assert_eq!(
            window,
            "aaaa\nbbbb\n[output truncated at the 9-byte cap: showing lines 1-2 of 3 · re-read with offset=3]"
        );
    }

    /// One line wider than the whole budget: dropping it would return nothing
    /// and hint an offset that never advances, so the line is cut instead and
    /// the notice says a window cannot reach the rest.
    #[test]
    fn a_line_wider_than_the_byte_cap_is_never_dropped_silently() {
        let lines = ["aaaaaaaaaa", "b"];
        let window = truncate_window(&lines, 1, 2, 4);
        assert!(window.starts_with("aaaa\n"), "{window}");
        assert!(
            window.contains("no line window reaches the rest"),
            "{window}"
        );
    }

    /// Every notice a window can end with is recognisable, so a caller can
    /// carry the hint over — and file content never is, so a file whose last
    /// line merely looks like a notice cannot be mistaken for one.
    #[test]
    fn every_notice_a_window_appends_is_recognised_as_one() {
        let long = "x".repeat(40);
        let stopped_short = truncate_window(&["a", "b", "c"], 1, 2, 1024);
        let byte_capped = truncate_window(&["aaaa", "bbbb", "cccc"], 1, 3, 9);
        let too_wide = truncate_window(&[long.as_str(), "b"], 1, 2, 8);

        for window in [stopped_short, byte_capped, too_wide] {
            let last = window.lines().next_back().unwrap_or_default();
            assert_eq!(window_notice(last), Some(last), "{window}");
        }

        assert_eq!(window_notice("fn main() {}"), None);
    }

    /// Bounds that name no line are answered rather than silently emptied: a
    /// blank payload reads as an empty file and invites the same call again.
    #[test]
    fn a_window_that_names_no_line_says_so() {
        let lines = ["a", "b", "c"];
        assert!(truncate_window(&lines, 0, 2, 1024).contains("names nothing"));
        assert!(truncate_window(&lines, 2, 1, 1024).contains("names nothing"));
        assert!(truncate_window(&lines, 4, 4, 1024).contains("names nothing"));
    }

    #[test]
    fn char_boundary_truncation_never_splits_a_character() {
        // "é" is two bytes; byte index 3 falls mid-character.
        let truncated = truncate_at_char_boundary("aaéé", 3);
        assert_eq!(truncated, "aa");
    }

    #[test]
    fn char_boundary_truncation_returns_short_input_whole() {
        assert_eq!(truncate_at_char_boundary("abc", 200), "abc");
    }

    #[test]
    fn a_status_line_is_indented_glyphed_and_closed() {
        assert_eq!(line(RED, "✗", "boom"), "\x1b[31m  ✗ boom\x1b[0m");
    }

    /// An aside carries no glyph, and must not carry its space either.
    #[test]
    fn a_glyphless_line_keeps_the_indent_and_nothing_more() {
        assert_eq!(line(DIM, "", "aside"), "\x1b[90m  aside\x1b[0m");
    }

    /// Every helper closes the escape it opens, or the color bleeds into
    /// whatever the model prints next.
    #[test]
    fn every_style_is_reset() {
        for style in [RED, GREEN, YELLOW, DIM, BOLD] {
            assert!(line(style, "✓", "x").ends_with(RESET));
        }
    }

    /// A duration is spelled one way wherever one is shown: a tenth for the
    /// range a call is worth reading to a tenth in, whole seconds above.
    #[test]
    fn a_duration_is_a_tenth_a_second_or_a_minute_and_a_half() {
        assert_eq!(elapsed(Duration::from_millis(430)), "0.4s");
        assert_eq!(elapsed(Duration::from_millis(12_400)), "12s");
        assert_eq!(elapsed(Duration::from_secs(125)), "2m 05s");
        assert_eq!(elapsed(Duration::from_millis(59_600)), "59s");
    }

    /// An OSC string is opened by its sequence number and closed by ST, so a
    /// report cannot run on into the next thing the buffer shows.
    #[test]
    fn an_osc_string_is_framed_by_its_number_and_closed() {
        assert_eq!(osc(7501, "state=idle"), "\x1b]7501;state=idle\x1b\\");
    }

    /// A comint child announces itself in `INSIDE_EMACS` regardless of what
    /// `TERM` says, and any `TERM=dumb` terminal gets the same treatment.
    #[test]
    fn comint_and_dumb_terminals_are_plain() {
        assert!(plain_terminal(Some("31.1,comint"), Some("dumb")));
        assert!(plain_terminal(Some("31.1,comint"), None));
        assert!(plain_terminal(None, Some("dumb")));
    }

    /// Whole-token matching keeps a name that merely contains "comint"
    /// from triggering the gate, and other Emacs contexts stay plain only
    /// by their own tokens.
    #[test]
    fn an_inside_emacs_without_the_comint_token_is_not_plain() {
        assert!(!plain_terminal(Some("31.1,shell-mode"), Some("xterm")));
        assert!(!plain_terminal(Some("comint-extension"), None));
    }

    /// An interactive terminal keeps the spinner.
    #[test]
    fn a_real_terminal_is_not_plain() {
        assert!(!plain_terminal(None, Some("xterm-256color")));
        assert!(!plain_terminal(None, None));
    }
}
