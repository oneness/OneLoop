//! Finding an edit's anchor, and splicing its replacement — with line endings
//! settled here, not in the tool. Matchers are a table; the ladder stops at the
//! first pass that matches at all, so an ambiguous anchor is refused, not passed on.

use std::borrow::Cow;
use std::ops::Range;

/// The bytes a replacement occupies.
pub type Span = Range<usize>;

/// The name of the byte-exact matcher, which is also the name a result uses to
/// say that a fallback was not needed.
pub const EXACT: &str = "exact";

/// One way of finding `old_text` in a file.
#[derive(Debug)]
struct Matcher {
    /// How a result and a refusal name it, so a session log can count it.
    name: &'static str,
    scan: fn(&str, &str) -> Vec<Span>,
}

/// Applied in order; the first one matching at all decides. See the module
/// comment: ambiguous is an answer, not a reason to keep looking.
const MATCHERS: &[Matcher] = &[
    Matcher {
        name: EXACT,
        scan: exact,
    },
    Matcher {
        name: "trailing whitespace ignored",
        scan: trailing_whitespace,
    },
];

/// Reported on a miss, never applied: each waits for a logged miss to earn it.
///
/// `line breaks ignored` is the row a rewrap earns. An anchor copied from what the
/// text meant to say, in a file whose lines end somewhere else, matches no line at
/// all, and every comparison above this table works in whole lines.
const PROBES: &[Matcher] = &[
    Matcher {
        name: "indentation ignored",
        scan: indentation,
    },
    Matcher {
        name: "line breaks ignored",
        scan: line_breaks,
    },
];

/// What [`locate`] found.
#[derive(Debug, PartialEq)]
pub enum Located {
    /// One span, and the matcher that found it.
    Found { matcher: &'static str, span: Span },
    /// More places than one, so the anchor names no single target.
    Ambiguous {
        /// The pass that found them.
        matcher: &'static str,
        /// How many matched, which is not how many lines they start on.
        places: usize,
        /// The distinct lines they start on, in file order.
        lines: Vec<usize>,
    },
    /// No matcher found it.
    Miss,
}

/// Two replacements that collide, named by their positions in the batch so a
/// refusal can say which two the model has to combine.
#[derive(Debug, PartialEq)]
pub struct Overlap {
    /// The replacement already written when the collision was seen.
    pub earlier: usize,
    /// The replacement that overlapped it.
    pub later: usize,
}

/// The place `old_text` names, or how it fell short. `old_text` must not be
/// empty — the tool refuses that first, and an empty anchor matches per byte.
pub fn locate(content: &str, old_text: &str) -> Located {
    debug_assert!(
        !old_text.is_empty(),
        "an empty anchor is refused before it reaches the matcher"
    );
    for anchor in anchors(old_text, file_eol(content)) {
        match ladder(content, &anchor) {
            // Only a miss is worth respelling: a file that mixes endings keeps a
            // literal spelling worth trying, and every other outcome is final.
            Located::Miss => {}
            found => return found,
        }
    }
    Located::Miss
}

/// The spellings of `old_text` worth trying in a file that spells its lines
/// `eol`: the file's, then the literal one. They differ only when the two
/// disagree about `\r`, so a file that already agrees is searched once.
fn anchors<'a>(old_text: &'a str, eol: &str) -> Vec<Cow<'a, str>> {
    let spelled = with_eol(old_text, eol);
    if spelled == old_text {
        return vec![spelled];
    }
    vec![spelled, Cow::Borrowed(old_text)]
}

/// The table, applied in order: the first matcher to find anything decides.
fn ladder(content: &str, old_text: &str) -> Located {
    for matcher in MATCHERS {
        match (matcher.scan)(content, old_text).as_slice() {
            [span] => {
                return Located::Found {
                    matcher: matcher.name,
                    span: span.clone(),
                };
            }
            [] => {}
            spans => {
                // Sorted rather than trusting the matcher to be ordered: a
                // refusal lists these lines, and `dedup` only sees neighbours.
                let mut lines = spans
                    .iter()
                    .map(|span| line_of(content, span.start))
                    .collect::<Vec<_>>();
                lines.sort_unstable();
                lines.dedup();
                return Located::Ambiguous {
                    matcher: matcher.name,
                    places: spans.len(),
                    lines,
                };
            }
        }
    }
    Located::Miss
}

/// The probes that would have carried `old_text`, and the first place each one
/// found. Data rather than a sentence: the refusal renders it.
pub fn probes(content: &str, old_text: &str) -> Vec<(&'static str, Span)> {
    PROBES
        .iter()
        .filter_map(|probe| {
            let span = (probe.scan)(content, old_text).into_iter().next()?;
            Some((probe.name, span))
        })
        .collect()
}

/// Every occurrence of `old_text`, one character past the last start so that an
/// anchor overlapping itself (`aa` in `aaa`) reports two places rather than one.
fn exact(content: &str, old_text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut from = 0;
    while from <= content.len() {
        let Some(at) = content[from..].find(old_text) else {
            break;
        };
        let start = from + at;
        spans.push(start..start + old_text.len());
        from = start + content[start..].chars().next().map_or(1, char::len_utf8);
    }
    spans
}

fn trailing_whitespace(content: &str, old_text: &str) -> Vec<Span> {
    line_windows(content, old_text, str::trim_end)
}

fn indentation(content: &str, old_text: &str) -> Vec<Span> {
    line_windows(content, old_text, str::trim)
}

/// Spans holding `old_text`'s words in `old_text`'s order, wherever the lines
/// break: what a rewrap leaves behind, and what no whole-line comparison sees.
/// Indentation and blank lines take part in neither side, so this is a probe and
/// not a matcher — it says where the text is, and never writes over it.
fn line_breaks(content: &str, old_text: &str) -> Vec<Span> {
    let needle = old_text.split_whitespace().collect::<Vec<_>>();
    if needle.is_empty() {
        return Vec::new();
    }
    let file = words(content);
    if needle.len() > file.len() {
        return Vec::new();
    }
    (0..=file.len() - needle.len())
        .filter(|start| (0..needle.len()).all(|at| file[start + at].0 == needle[at]))
        .map(|start| file[start].1.start..file[start + needle.len() - 1].1.end)
        .collect()
}

/// Each run of non-whitespace in `text`, with the bytes it occupies. The same
/// predicate `split_whitespace` uses, so an anchor's words and the file's are cut
/// the same way whatever the file's own spacing is.
fn words(text: &str) -> Vec<(&str, Span)> {
    let mut out = Vec::new();
    let mut from = None;
    for (offset, at) in text.char_indices() {
        match (at.is_whitespace(), from) {
            (false, None) => from = Some(offset),
            (true, Some(start)) => {
                out.push((&text[start..offset], start..offset));
                from = None;
            }
            _ => {}
        }
    }
    if let Some(start) = from {
        out.push((&text[start..], start..text.len()));
    }
    out
}

/// Line windows matching `old_text` under `key`. A span stops before the last
/// line's break — keeping it when the anchor claimed one — so line counts survive.
fn line_windows(content: &str, old_text: &str, key: fn(&str) -> &str) -> Vec<Span> {
    let needle = old_text.strip_suffix('\n').unwrap_or(old_text);
    let claims_break = needle.len() != old_text.len();
    let file = file_lines(content);
    // `split`, not `lines`: an anchor of one newline is one blank line, and
    // `lines` would hand back no lines at all for it.
    let old = needle.split('\n').collect::<Vec<_>>();
    if old.len() > file.len() {
        return Vec::new();
    }
    (0..=file.len() - old.len())
        .filter(|start| (0..old.len()).all(|at| key(file[start + at].1) == key(old[at])))
        .map(|start| {
            let last = &file[start + old.len() - 1];
            let end = last.0 + last.1.len();
            let kept = if claims_break {
                break_len(content, end)
            } else {
                0
            };
            file[start].0..end + kept
        })
        .collect()
}

/// The break at `end`: 2 for `\r\n`, 1 for `\n`, 0 for the end of the file.
fn break_len(content: &str, end: usize) -> usize {
    let rest = &content.as_bytes()[end..];
    if rest.starts_with(b"\r\n") {
        2
    } else {
        usize::from(rest.first() == Some(&b'\n'))
    }
}

/// How a file spells its line endings: CRLF if it holds one anywhere, because a
/// file that mixes both should converge rather than split.
fn file_eol(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// `text` respelled in `eol`, borrowed when it already agrees. Collapsing before
/// expanding keeps text that already carries CRLF from being doubled.
fn with_eol<'a>(text: &'a str, eol: &str) -> Cow<'a, str> {
    if eol == "\n" && !text.contains('\r') {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace("\r\n", "\n").replace('\n', eol))
}

/// Each line of `text` with its byte offset and ending removed, so an anchor
/// written with `\n` still lines up with a `\r\n` file. No line after a final
/// newline: counting one would make a blank-line anchor ambiguous.
pub(super) fn file_lines(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        out.push((start, line.trim_end_matches(['\r', '\n'])));
        start += line.len();
    }
    out
}

/// The 1-based line holding `offset`.
pub(super) fn line_of(content: &str, offset: usize) -> usize {
    content[..offset].matches('\n').count() + 1
}

/// `content` with every span replaced in one pass — no offset adjusted after a
/// length change — and the line each replacement landed on. Spans may sit end to
/// end but not overlap, or the result would depend on which was applied first.
pub fn splice(
    content: &str,
    replacements: &[(Span, &str)],
) -> Result<(String, Vec<usize>), Overlap> {
    let mut order = (0..replacements.len()).collect::<Vec<_>>();
    order.sort_unstable_by_key(|index| replacements[*index].0.start);

    let eol = file_eol(content);
    let mut updated = String::with_capacity(content.len());
    let mut landed = vec![0; replacements.len()];
    // The span written last, as its index and where it ended: the running cursor
    // and the name a collision needs, so there is no second variable to keep in
    // agreement and no sentinel index standing for "nothing written yet".
    let mut placed: Option<(usize, usize)> = None;
    for &index in &order {
        let (span, text) = &replacements[index];
        debug_assert!(
            span.end <= content.len(),
            "a matcher returned a span outside the file"
        );
        let cursor = match placed {
            Some((earlier, end)) if span.start < end => {
                return Err(Overlap {
                    earlier,
                    later: index,
                });
            }
            Some((_, end)) => end,
            None => 0,
        };
        updated.push_str(&content[cursor..span.start]);
        landed[index] = line_of(&updated, updated.len());
        updated.push_str(&with_eol(text, eol));
        placed = Some((index, span.end));
    }
    updated.push_str(&content[placed.map_or(0, |(_, end)| end)..]);
    Ok((updated, landed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_finds_the_place_it_names() {
        assert_eq!(exact("a\nbeta\nc\n", "beta"), vec![2..6]);
    }

    /// An anchor that overlaps itself is two places, not one: `match_indices`
    /// would report a single hit and replace an arbitrary one of them.
    #[test]
    fn a_self_overlapping_anchor_reports_two_places() {
        assert_eq!(exact("aaa\n", "aa"), vec![0..2, 1..3]);
    }

    /// A span stops at the last matched line, so the replacement does not eat
    /// the newline and run into the line below.
    #[test]
    fn a_span_stops_before_the_last_line_break() {
        assert_eq!(trailing_whitespace("a\nb\nc\n", "a\nb"), vec![0..3]);
    }

    /// An anchor ending in a newline owns that break, so a replacement ending in
    /// one keeps the file's line count.
    #[test]
    fn an_anchor_ending_in_a_newline_owns_that_break() {
        assert_eq!(trailing_whitespace("a\nb\nc\n", "a\nb\n"), vec![0..4]);
    }

    /// An anchor claiming a break keeps `\r\n` whole, so a CRLF file stays CRLF.
    #[test]
    fn a_break_kept_by_the_anchor_keeps_both_bytes() {
        assert_eq!(trailing_whitespace("a\r\nb\r\n", "a\n"), vec![0..3]);
    }

    #[test]
    fn trailing_space_in_the_file_is_ignored() {
        assert_eq!(
            trailing_whitespace("alpha beta  \ngamma\n", "alpha beta"),
            vec![0..12]
        );
    }

    /// The applied matchers deliberately stop at trailing space: leading space
    /// is what the anchor's indentation should have matched.
    #[test]
    fn leading_space_is_not_ignored_by_the_applied_matchers() {
        assert!(trailing_whitespace("    beta\n", "beta").is_empty());
    }

    #[test]
    fn the_indentation_probe_ignores_leading_space() {
        assert_eq!(indentation("    beta\n", "beta"), vec![0..8]);
    }

    /// The row a rewrap earns: every word is there, in order, and only where the
    /// lines end differs — which defeats `exact` and both whole-line fallbacks.
    #[test]
    fn the_line_breaks_probe_matches_text_wrapped_differently() {
        let content = "such flag. The answer is printed, then recorded as\nreference material attributed to the user.\n";
        let anchor =
            "The answer is printed, then recorded as reference material\nattributed to the user.";

        assert!(exact(content, anchor).is_empty());
        assert!(trailing_whitespace(content, anchor).is_empty());
        assert!(indentation(content, anchor).is_empty());

        let spans = line_breaks(content, anchor);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(
            &content[spans[0].clone()],
            "The answer is printed, then recorded as\nreference material attributed to the user."
        );
    }

    /// Only the breaks are forgiven: a word the file does not hold is still a miss,
    /// or the hint would send the model to a region that does not say what it wants.
    #[test]
    fn the_line_breaks_probe_sees_a_changed_word() {
        assert!(line_breaks("alpha beta\ngamma\n", "alpha beta delta").is_empty());
    }

    #[test]
    fn the_line_breaks_probe_reports_every_window_in_order() {
        assert_eq!(line_breaks("x\ny x y\n", "x y"), vec![0..3, 4..7]);
    }

    /// An anchor naming no word names no place: a blank line is the line matchers'
    /// to describe, and there is nothing here for a word comparison to find.
    #[test]
    fn the_line_breaks_probe_ignores_an_anchor_of_whitespace() {
        assert!(line_breaks("a\n\nb\n", "\n").is_empty());
        assert!(line_breaks("a\n", "   ").is_empty());
    }

    #[test]
    fn a_blank_line_anchor_reaches_the_first_line() {
        assert_eq!(trailing_whitespace("a\n\nb\n", "\n"), vec![2..3]);
    }

    #[test]
    fn a_carriage_return_does_not_hide_a_line() {
        assert_eq!(file_lines("a\r\nb"), vec![(0, "a"), (3, "b")]);
    }

    /// Every matcher reports its spans in file order, which is what lets a
    /// refusal deduplicate the lines they start on.
    #[test]
    fn every_matcher_reports_ascending_spans() {
        let content = "aa\n a \nbb aa\n";
        for matcher in MATCHERS.iter().chain(PROBES) {
            for anchor in ["a", "aa", "\n", "a\n", "bb", "bb aa"] {
                let spans = (matcher.scan)(content, anchor);
                assert!(
                    spans.windows(2).all(|pair| pair[0].start < pair[1].start),
                    "{} on {anchor:?} reported {spans:?}",
                    matcher.name
                );
            }
        }
    }

    #[test]
    fn one_span_is_found_and_named_by_its_matcher() {
        assert_eq!(
            locate("a\nbeta\nc\n", "beta"),
            Located::Found {
                matcher: EXACT,
                span: 2..6
            }
        );
    }

    /// The ladder refuses on an ambiguous matcher rather than trying a coarser
    /// one, even when that one would name a single place: `line_windows` compares
    /// whole lines and cannot see the mid-line `beta` that made `exact` ambiguous.
    #[test]
    fn an_ambiguous_matcher_ends_the_ladder() {
        let content = "beta alpha\nbeta\n";
        assert_eq!(exact(content, "beta"), vec![0..4, 11..15]);
        assert_eq!(trailing_whitespace(content, "beta"), vec![11..15]);
        assert_eq!(
            locate(content, "beta"),
            Located::Ambiguous {
                matcher: EXACT,
                places: 2,
                lines: vec![1, 2],
            }
        );
    }

    #[test]
    fn two_places_are_ambiguous() {
        assert_eq!(
            locate("same\nother\nsame\n", "same"),
            Located::Ambiguous {
                matcher: EXACT,
                places: 2,
                lines: vec![1, 3],
            }
        );
    }

    #[test]
    fn an_absent_anchor_is_a_miss() {
        assert_eq!(locate("alpha\n", "omega"), Located::Miss);
    }

    /// A read renders a CRLF file without its `\r`; respelling the anchor is what
    /// lets the bytes meet. It must land as `exact`, or the count that decides
    /// whether the whitespace matcher earns its row measures the wrong thing.
    #[test]
    fn an_anchor_copied_from_a_crlf_file_matches_exactly() {
        assert_eq!(
            locate("alpha\r\nbeta\r\n", "alpha\nbeta"),
            Located::Found {
                matcher: EXACT,
                span: 0..11
            }
        );
    }

    /// A file that mixes endings is served by the literal spelling when the
    /// file's own spelling does not reach the region the anchor came from.
    #[test]
    fn a_mixed_ending_file_still_finds_a_literal_anchor() {
        assert_eq!(
            locate("one\r\ntwo\n", "one\ntwo"),
            Located::Found {
                matcher: EXACT,
                span: 0..8
            }
        );
        assert_eq!(
            locate("one\r\ntwo\n", "one\r\ntwo"),
            Located::Found {
                matcher: EXACT,
                span: 0..8
            }
        );
    }

    /// A file that already spells its lines the way the anchor does is searched
    /// once, not twice.
    #[test]
    fn an_anchor_already_in_the_files_spelling_is_tried_once() {
        let spellings = anchors("a\nb", "\n");
        assert_eq!(spellings.len(), 1, "{spellings:?}");
        assert_eq!(spellings[0], "a\nb");

        let no_breaks = anchors("beta", "\r\n");
        assert_eq!(no_breaks.len(), 1, "{no_breaks:?}");
        assert_eq!(no_breaks[0], "beta");
    }

    /// A file that disagrees about `\r` gets both spellings, the file's first.
    #[test]
    fn a_disagreeing_anchor_is_tried_in_the_files_spelling_first() {
        let spellings = anchors("a\nb", "\r\n");
        assert_eq!(spellings.len(), 2, "{spellings:?}");
        assert_eq!(spellings[0], "a\r\nb");
        assert_eq!(spellings[1], "a\nb");
    }

    /// The probes are handed on as data, so the refusal that renders them owns
    /// only the sentence. Both fire on this anchor: its indentation is wrong and
    /// its words are nonetheless all there.
    #[test]
    fn a_probe_reports_its_name_and_first_place() {
        let found = probes("fn main() {\n    let x = 1;\n}\n", "  fn main() {");
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].0, "indentation ignored");
        assert_eq!(found[0].1, 0..11);
        assert_eq!(found[1].0, "line breaks ignored");
        assert_eq!(found[1].1, 0..11);
    }

    #[test]
    fn a_probe_that_finds_nothing_reports_nothing() {
        assert!(probes("fn main() {}\n", "let x = 1;").is_empty());
    }

    /// Both replacements land as written when an earlier one changed length,
    /// because the string is assembled from the original offsets.
    #[test]
    fn splice_assembles_from_the_top_without_adjusting_offsets() {
        let (updated, landed) =
            splice("a middle z\n", &[(0..1, "long beginning"), (1..8, "")]).unwrap();
        assert_eq!(updated, "long beginning z\n");
        assert_eq!(landed, vec![1, 1]);
    }

    #[test]
    fn splice_names_the_line_each_replacement_landed_on() {
        let (_, landed) = splice("a\nb\nc\n", &[(4..5, "C"), (0..1, "A")]).unwrap();
        assert_eq!(landed, vec![3, 1]);
    }

    /// Two ranges measured in the same original collide whatever their order in
    /// the list, and the message can name both.
    #[test]
    fn splice_names_the_colliding_pair() {
        assert_eq!(
            splice("abcdef\n", &[(6..7, "x"), (0..4, "y"), (2..7, "z")]),
            Err(Overlap {
                earlier: 1,
                later: 2
            })
        );
    }

    /// Adjacent spans are two edits sitting end to end, not a collision.
    #[test]
    fn disjoint_replacements_do_not_collide() {
        assert!(splice("ab\n", &[(0..1, "x"), (1..2, "y")]).is_ok());
    }

    /// A replacement is spelled the way the file spells its lines, so writing
    /// multiline text into a CRLF file does not split its endings.
    #[test]
    fn a_replacement_takes_the_files_line_endings() {
        let (updated, _) = splice("alpha\r\nbeta\r\n", &[(7..13, "B1\nB2\n")]).unwrap();
        assert_eq!(updated, "alpha\r\nB1\r\nB2\r\n");
    }

    /// An LF file stays LF even when the replacement arrived carrying CRLF.
    #[test]
    fn a_replacement_into_an_lf_file_drops_carriage_returns() {
        let (updated, _) = splice("alpha\nbeta\n", &[(6..11, "B1\r\nB2\r\n")]).unwrap();
        assert_eq!(updated, "alpha\nB1\nB2\n");
    }

    /// The blame is a running cursor, so the pair named is only right while spans
    /// are ordered by start. Every ordered triple over a six-byte file is checked
    /// outright: either nothing collides, or the pair blamed really does.
    #[test]
    fn the_overlap_check_blames_a_pair_that_really_collides() {
        fn collides(one: &Span, other: &Span) -> bool {
            one.start < other.end && other.start < one.end
        }
        fn any_collision(spans: &[Span]) -> bool {
            spans
                .iter()
                .enumerate()
                .any(|(at, one)| spans[at + 1..].iter().any(|other| collides(one, other)))
        }

        let every_span = (0..6)
            .flat_map(|start| ((start + 1)..=6).map(move |end| start..end))
            .collect::<Vec<_>>();
        for first in &every_span {
            for second in &every_span {
                for third in &every_span {
                    let spans = [first.clone(), second.clone(), third.clone()];
                    let replacements = spans
                        .iter()
                        .cloned()
                        .map(|span| (span, "x"))
                        .collect::<Vec<_>>();
                    match splice("abcdef", &replacements) {
                        Ok(_) => {
                            assert!(!any_collision(&spans), "an overlap was accepted: {spans:?}")
                        }
                        Err(overlap) => assert!(
                            collides(&spans[overlap.earlier], &spans[overlap.later]),
                            "a pair was blamed that does not collide: {spans:?} -> {overlap:?}"
                        ),
                    }
                }
            }
        }
    }
}
