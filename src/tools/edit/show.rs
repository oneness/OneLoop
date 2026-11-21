//! Every line range the model reads — around a place, an ambiguity's candidates,
//! the region an edit changed, the lines a miss almost hit — rendered from what
//! `matcher` found. The constants here are those renderings' sizes.

use super::matcher::{self, Span};

/// Enough of a line to copy; past this it is not something to retype, and a
/// minified file would otherwise be pasted back.
const MAX_CHARS: usize = 96;

/// Two lines of context each side of the anchor, for a hint.
const AROUND: usize = 2;

/// Candidate lines a refusal lists before counting the rest.
const LISTED: usize = 5;

/// The most lines of either side a changed region shows.
const SIDE: usize = 4;

/// One line, marked by what it is: `|` for context, `-` for text the edit
/// removed, `+` for text it wrote — numbered where the number is what the model
/// acts on, and bare in a change, where the marks already say which side a line
/// is on. A line too long to show whole ends in `…`: half a line is not one to
/// copy, and pasting one back is how a retry fails twice.
fn marked(number: Option<usize>, marker: char, text: &str) -> String {
    let mut shown: String = text.chars().take(MAX_CHARS).collect();
    // A prefix shorter than the line it came from is a line that was cut.
    if shown.len() < text.len() {
        shown.push('…');
    }
    match number {
        Some(number) => format!("{number:4} {marker} {shown}"),
        None => format!("{marker} {shown}"),
    }
}

/// The lines either side of `line` (1-based), numbered, and whether one was too
/// long to show whole.
fn lines_around(content: &str, line: usize) -> (String, bool) {
    let file = matcher::file_lines(content);
    let first = line.saturating_sub(1 + AROUND);
    let shown = file
        .iter()
        .skip(first)
        .take(AROUND * 2 + 1)
        .map(|(_, text)| text)
        .collect::<Vec<_>>();
    let cut = shown.iter().any(|text| text.chars().count() > MAX_CHARS);
    let lines = shown
        .iter()
        .enumerate()
        .map(|(offset, text)| marked(Some(first + offset + 1), '|', text))
        .collect::<Vec<_>>()
        .join("\n");
    (lines, cut)
}

/// The candidate lines a refusal shows, and how many it left out: one value rather
/// than a tuple, because the two are printed together.
pub struct Excerpt {
    /// The lines, numbered.
    pub lines: String,
    /// Candidate lines `lines` does not include.
    pub hidden: usize,
}

/// The candidate lines a refusal shows, and how many it left out.
pub fn candidates(content: &str, numbers: &[usize]) -> Excerpt {
    let file = matcher::file_lines(content);
    let shown = &numbers[..numbers.len().min(LISTED)];
    let lines = shown
        .iter()
        .filter_map(|number| {
            file.get(number.saturating_sub(1))
                .map(|(_, text)| marked(Some(*number), '|', text))
        })
        .collect::<Vec<_>>()
        .join("\n");
    Excerpt {
        lines,
        hidden: numbers.len() - shown.len(),
    }
}

/// The region an edit changed: the lines it replaced, then the lines it wrote.
/// This is what a fallback receipt is for — an exact anchor needs no evidence,
/// but a fallback matched text the model did not send. Unnumbered: the marks say
/// which side a line is on, and in a call with several edits no one line number
/// is right for all of them.
pub fn change(original: &str, span: &Span, text: &str) -> String {
    let mut out = Vec::new();
    let removed = removed_lines(original, span);
    for text in removed.iter().take(SIDE) {
        out.push(marked(None, '-', text));
    }
    if removed.len() > SIDE {
        out.push(format!("  … {} more removed", removed.len() - SIDE));
    }
    let added = added_lines(text);
    for text in added.iter().take(SIDE) {
        out.push(marked(None, '+', text));
    }
    if added.len() > SIDE {
        out.push(format!("  … {} more written", added.len() - SIDE));
    }
    out.join("\n")
}

/// The lines of `content` that `span` covers.
fn removed_lines<'a>(content: &'a str, span: &Span) -> Vec<&'a str> {
    if span.start >= span.end {
        return Vec::new();
    }
    let covered = &content[span.start..span.end];
    let breaks = covered.matches('\n').count();
    // A span ending at a line break ends where the next line starts, and that
    // line is not one it replaced.
    let count = if covered.ends_with('\n') {
        breaks
    } else {
        breaks + 1
    };
    let first = matcher::line_of(content, span.start);
    matcher::file_lines(content)
        .iter()
        .skip(first - 1)
        .take(count)
        .map(|(_, text)| *text)
        .collect()
}

/// The lines `text` puts in the file. An empty replacement puts none, one trailing
/// newline contributes no empty line of its own, and `\r` is dropped for the same
/// reason [`matcher::file_lines`] drops it: a reader does not see it.
fn added_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// What a miss almost hit: the probes that would have carried it, with the lines
/// to copy. A model corrects an anchor from this instead of reading the file.
pub fn hint(content: &str, probes: &[(&'static str, Span)]) -> String {
    let Some((_, span)) = probes.first() else {
        return "old_str matched no line. If you copied it from an earlier read, re-read \
                the file and copy the block again: an edit since that read may have \
                changed it."
            .to_string();
    };
    let ignored = probes
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(" and ");
    let line = matcher::line_of(content, span.start);
    let (excerpt, cut) = lines_around(content, line);
    // A line cut to `MAX_CHARS` cannot be copied back, so the hint asks for the
    // file instead of handing the model an anchor that is bound to miss.
    let advice = if cut {
        format!("Read the file at line {line} and copy the block from there:")
    } else {
        "Copy these lines exactly:".to_string()
    };
    format!("The file holds this text with {ignored}. {advice}\n{excerpt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_around_shows_the_lines_either_side() {
        assert_eq!(
            lines_around("a\nb\nc\nd\ne\n", 3).0,
            "   1 | a\n   2 | b\n   3 | c\n   4 | d\n   5 | e"
        );
    }

    #[test]
    fn lines_around_the_first_line_starts_at_the_first_line() {
        assert_eq!(
            lines_around("a\nb\nc\n", 1).0,
            "   1 | a\n   2 | b\n   3 | c"
        );
    }

    /// A line longer than the excerpt shows ends in `…`, and the flag says so:
    /// a truncated line is not one the model can copy back.
    #[test]
    fn a_cut_line_ends_in_an_ellipsis_and_is_marked_as_cut() {
        let long = "a".repeat(MAX_CHARS + 10);
        let (excerpt, cut) = lines_around(&long, 1);

        assert!(cut);
        assert!(excerpt.ends_with('…'), "{excerpt}");
        assert_eq!(excerpt.chars().count(), 7 + MAX_CHARS + 1);
    }

    /// A line that fits is not marked as cut, so the hint can keep telling the
    /// model to copy it without hedging.
    #[test]
    fn a_line_that_fits_is_not_marked_as_cut() {
        let (excerpt, cut) = lines_around("alpha\n", 1);

        assert!(!cut);
        assert!(!excerpt.contains('…'), "{excerpt}");
    }

    /// A CRLF file renders without its `\r`: an excerpt is for copying, and the
    /// carriage return is invisible anyway.
    #[test]
    fn an_excerpt_hides_carriage_returns() {
        assert!(!lines_around("alpha\r\nbeta\r\n", 1).0.contains('\r'));
    }

    #[test]
    fn candidates_show_only_the_lines_they_name() {
        let excerpt = candidates("a\nb\nc\n", &[1, 3]);
        assert_eq!(excerpt.lines, "   1 | a\n   3 | c");
        assert_eq!(excerpt.hidden, 0);
    }

    /// A refusal names a handful of candidate lines and counts the rest, so a
    /// one-character anchor cannot paste a file back.
    #[test]
    fn candidates_count_the_lines_they_leave_out() {
        let excerpt = candidates("a\nb\nc\nd\ne\nf\n", &[1, 2, 3, 4, 5, 6]);
        assert_eq!(excerpt.lines.lines().count(), LISTED);
        assert_eq!(excerpt.hidden, 1);
    }

    /// The receipt for a fallback shows the lines the file actually held and the
    /// lines that replaced them — the trailing space the model did not send is
    /// the whole reason this receipt exists.
    #[test]
    fn a_change_shows_what_was_replaced_and_what_was_written() {
        let change = change("alpha   \nbeta\n", &(0..8), "alpha");
        assert_eq!(change, "- alpha   \n+ alpha");
    }

    /// Both sides keep the file's order: every removed line first, then every
    /// written one, which is what the marks mean.
    #[test]
    fn a_change_shows_what_was_written_after_what_it_replaced() {
        let change = change("a\nb\nc\n", &(2..3), "B1\nB2");
        assert_eq!(change, "- b\n+ B1\n+ B2");
    }

    #[test]
    fn a_deletion_shows_only_what_it_removed() {
        assert_eq!(change("a\nb\nc\n", &(2..4), ""), "- b");
    }

    /// Both sides of a receipt drop `\r`: one reads the file's lines and the other
    /// the replacement it was sent, but a reader sees neither carriage return.
    #[test]
    fn a_change_into_a_crlf_file_shows_no_carriage_return() {
        let change = change("alpha   \r\n", &(0..8), "A1\r\nA2");
        assert!(!change.contains('\r'), "{change:?}");
    }

    /// A region larger than the receipt shows is capped on both sides and the
    /// remainder is counted rather than dropped in silence.
    #[test]
    fn a_change_caps_either_side_and_counts_the_rest() {
        let original = (1..=9).map(|n| format!("old{n}\n")).collect::<String>();
        let written = (1..=9)
            .map(|n| format!("new{n}\n"))
            .collect::<Vec<_>>()
            .join("");
        let change = change(&original, &(0..original.len()), &written);

        assert!(change.contains("… 5 more removed"), "{change}");
        assert!(change.contains("… 5 more written"), "{change}");
    }

    /// The hint names the normalization that would have carried the anchor, so
    /// the model rewrites one block instead of re-reading the file.
    #[test]
    fn a_hint_names_the_probe_that_would_have_matched() {
        let content = "fn main() {\n    let x = 1;\n}\n";
        let hint = hint(content, &matcher::probes(content, "  fn main() {"));
        assert!(hint.contains("indentation ignored"), "{hint}");
        assert!(hint.contains("   1 | fn main() {"), "{hint}");
    }

    #[test]
    fn a_hint_without_a_probe_asks_for_a_fresh_copy() {
        let hint = hint("fn main() {}\n", &[]);
        assert!(
            hint.contains("re-read the file and copy the block again"),
            "{hint}"
        );
        assert!(
            hint.contains("an edit since that read may have changed it"),
            "{hint}"
        );
    }

    /// A line cut to `MAX_CHARS` cannot be copied back, so the hint stops asking
    /// for a copy and names the line to read instead.
    #[test]
    fn a_hint_over_a_cut_line_asks_for_the_file() {
        let long = format!("let x = \"{}\";", "a".repeat(MAX_CHARS + 10));
        let content = format!("    {long}\n");
        let hint = hint(&content, &matcher::probes(&content, long.as_str()));

        assert!(hint.contains("Read the file at line 1"), "{hint}");
        assert!(!hint.contains("Copy these lines exactly"), "{hint}");
    }

    /// A probe row exists to turn "nothing resembles this" into "here it is": the
    /// hint shows the lines the rewrapped anchor was copied from.
    #[test]
    fn a_hint_names_the_line_breaks_a_rewrap_moved() {
        let content = "alpha beta\ngamma delta\n";
        let hint = hint(
            content,
            &matcher::probes(content, "alpha beta gamma\ndelta"),
        );

        assert!(hint.contains("line breaks ignored"), "{hint}");
        assert!(hint.contains("   1 | alpha beta"), "{hint}");
        assert!(hint.contains("   2 | gamma delta"), "{hint}");
    }
}
