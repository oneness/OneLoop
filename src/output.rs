pub const DEFAULT_MAX_BYTES: usize = 32 * 1024;
pub const DEFAULT_MAX_LINES: usize = 200;

#[derive(Debug, Clone)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub original_bytes: usize,
    pub original_lines: usize,
    pub shown_bytes: usize,
    pub shown_lines: usize,
}

pub fn truncate_head(input: &str, max_bytes: usize, max_lines: usize) -> TruncationResult {
    truncate(input, max_bytes, max_lines, Keep::Head)
}

pub fn truncate_tail(input: &str, max_bytes: usize, max_lines: usize) -> TruncationResult {
    truncate(input, max_bytes, max_lines, Keep::Tail)
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

pub fn truncation_notice(result: &TruncationResult) -> String {
    format!(
        "[output truncated: showing {} of {} lines, {} of {} bytes]",
        result.shown_lines, result.original_lines, result.shown_bytes, result.original_bytes
    )
}
