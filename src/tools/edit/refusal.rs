//! Why an edit did not happen, in the words the model reads. A refusal is its
//! sentence — one constructor per reason — and it travels back as an answered call
//! with `is_error` set; an `Err` is kept for the tool's own failures.

use std::fmt;
use std::path::Path;

/// A call that was answered with "no", and what to do instead.
#[derive(Debug)]
pub struct Refusal(String);

/// The prefix a refusal uses when the call carried several edits: `edits[2]: `,
/// or nothing when there is only one edit and no position to count to.
fn at_prefix(at: Option<usize>) -> String {
    at.map_or_else(String::new, |index| format!("edits[{index}]: "))
}

impl Refusal {
    /// The arguments did not decode, so the call named no work. The shape is
    /// spelled out because a provider that ignored the schema still owes the
    /// model the reason it was wrong.
    pub fn bad_input(reason: &str) -> Self {
        Self(format!(
            "invalid edit input; expected {{ path: string, old_str: non-empty \
             string, new_str: string }} ({reason})"
        ))
    }

    /// A call whose edits did not decode. Its own shape, because the fix is to
    /// give each edit the two fields rather than to change the tool.
    pub fn bad_multi_input(reason: &str) -> Self {
        Self(format!(
            "invalid multi_edit input; expected {{ path: string, edits: [{{ old_str: \
             non-empty string, new_str: string }}] }} ({reason})"
        ))
    }

    /// `edits` was empty, which names no work.
    pub fn no_edits() -> Self {
        Self("edits must contain at least one replacement (nothing was written)".to_string())
    }

    /// The anchor was empty, which names no place in any file. Named by position
    /// when the call carried several edits, and told, like every other refusal
    /// that answered a call, that nothing was written.
    pub fn empty_anchor(at: Option<usize>) -> Self {
        Self(match at {
            None => "old_str must not be empty: it names no place in the file (nothing \
                     was written)"
                .to_string(),
            Some(index) => {
                format!("edits[{index}].old_str must not be empty (nothing was written)")
            }
        })
    }

    /// The path is blank, which resolves to no file at all.
    pub fn no_path() -> Self {
        Self("path must name a file: it was blank (nothing was written)".to_string())
    }

    /// The path is not there. Nothing has been written, because the file is read
    /// before it is written.
    pub fn not_found(path: &Path) -> Self {
        Self(format!(
            "file does not exist: {} (nothing was written)",
            path.display()
        ))
    }

    /// The bytes are not UTF-8, so an anchor cannot be found in them.
    pub fn not_text(path: &Path) -> Self {
        Self(format!(
            "not a UTF-8 text file, so it cannot be edited by anchor: {} \
             (nothing was written)",
            path.display()
        ))
    }

    /// The path is a directory, so there is no file to read or to edit.
    pub fn not_a_file(path: &Path) -> Self {
        Self(format!(
            "that path is a directory, not a file to edit: {} (nothing was written)",
            path.display()
        ))
    }

    /// The anchor matched nothing. The hint names the lines that almost carried
    /// it, so the retry is a rewrite rather than a re-read.
    pub fn miss(at: Option<usize>, path: &Path, hint: &str) -> Self {
        Self(format!(
            "{}old_str not found in file: {} (nothing was written)\n{hint}",
            at_prefix(at),
            path.display()
        ))
    }

    /// The anchor matched several places, so it names no single one. `places`
    /// counts matches and the excerpt shows the lines they start on, which is
    /// why the two can disagree.
    pub fn ambiguous(
        at: Option<usize>,
        path: &Path,
        matcher: &str,
        places: usize,
        excerpt: &str,
        hidden: usize,
    ) -> Self {
        let mut message = format!(
            "{}old_str matches {places} places in file: {} under {matcher} (nothing \
             was written)\n{excerpt}",
            at_prefix(at),
            path.display()
        );
        if hidden > 0 {
            message.push_str(&format!(
                "\n… and {hidden} more line{} not shown",
                if hidden == 1 { "" } else { "s" }
            ));
        }
        message.push_str(
            "\nExtend old_str with the lines that differ between the matches, \
             so it names one place.",
        );
        Self(message)
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> &'static Path {
        Path::new("/repo/file.rs")
    }

    /// A refusal says what happened, where, and that the file is unchanged — the
    /// last part is what tells the model a retry is safe.
    #[test]
    fn a_miss_names_the_path_and_the_absence_of_a_write() {
        assert_eq!(
            Refusal::miss(None, file(), "look at line 4").to_string(),
            "old_str not found in file: /repo/file.rs (nothing was written)\n\
             look at line 4"
        );
    }

    /// The count is of places and the excerpt is of lines, so the two can
    /// disagree; the tail counts the lines the excerpt left out.
    #[test]
    fn an_ambiguous_refusal_counts_the_lines_it_did_not_show() {
        let text = Refusal::ambiguous(None, file(), "exact", 7, "   1 | x", 3).to_string();
        assert!(text.contains("matches 7 places"), "{text}");
        assert!(text.contains("… and 3 more lines not shown"), "{text}");
    }

    /// One hidden line reads as a line, because the sentence is quoted back.
    #[test]
    fn a_single_hidden_line_is_not_pluralised() {
        let text = Refusal::ambiguous(None, file(), "exact", 2, "   1 | x", 1).to_string();
        assert!(text.contains("… and 1 more line not shown"), "{text}");
    }

    #[test]
    fn a_bad_input_names_the_shape_it_expected() {
        let text = Refusal::bad_input("missing field `old_str`").to_string();
        assert!(text.contains("expected { path: string, old_str:"), "{text}");
        assert!(text.contains("missing field `old_str`"), "{text}");
    }

    /// In a call with several edits, the position is what tells the model which
    /// one to fix, so it goes in front of the sentence.
    #[test]
    fn a_position_prefixes_the_sentence() {
        assert_eq!(
            Refusal::miss(Some(2), file(), "look").to_string(),
            "edits[2]: old_str not found in file: /repo/file.rs (nothing was written)\nlook"
        );
        assert_eq!(
            Refusal::empty_anchor(Some(1)).to_string(),
            "edits[1].old_str must not be empty (nothing was written)"
        );
    }

    /// An empty anchor and a blank path read differently because the fix for one
    /// is not the fix for the other, and both say the file was left alone.
    #[test]
    fn an_empty_anchor_and_a_blank_path_read_differently() {
        assert!(
            Refusal::empty_anchor(None)
                .to_string()
                .contains("old_str must not be empty")
        );
        assert!(
            Refusal::no_path()
                .to_string()
                .contains("path must name a file")
        );
        assert!(
            Refusal::empty_anchor(Some(1))
                .to_string()
                .contains("nothing was written")
        );
    }
}
