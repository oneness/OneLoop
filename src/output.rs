pub const DEFAULT_MAX_BYTES: usize = 128 * 1024;
pub const DEFAULT_MAX_LINES: usize = 1000;

/// Truncate keeping the first lines, appending a notice when content was dropped.
pub fn truncate_head(input: &str, max_bytes: usize, max_lines: usize) -> String {
    with_notice(truncate(input, max_bytes, max_lines, Keep::Head))
}

/// Truncate keeping the last lines, appending a notice when content was dropped.
pub fn truncate_tail(input: &str, max_bytes: usize, max_lines: usize) -> String {
    with_notice(truncate(input, max_bytes, max_lines, Keep::Tail))
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

enum Keep {
    Head,
    Tail,
}

fn truncate(input: &str, max_bytes: usize, max_lines: usize, keep: Keep) -> TruncationResult {
    let lines: Vec<&str> = input.lines().collect();
    let normalized_input = lines.join("\n");
    let original_bytes = normalized_input.len();
    let original_lines = lines.len();

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

    let content = chosen.join("\n");
    let shown_bytes = content.len();
    let shown_lines = chosen.len();
    let truncated = shown_lines < original_lines || shown_bytes < original_bytes;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
