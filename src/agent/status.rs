//! One line of process state, aimed at the status line rather than the
//! stream. The spinner stays silent in comint; these sentinels replace it
//! as the busy heartbeat.

use crate::output;

/// OSC 9;4 progress report — the ConEmu/Windows Terminal convention. State
/// 1 is indeterminate ("something is happening"), state 0 clears it. Emacs
/// does not act on these, but a small filter can lift the state into the
/// buffer's mode line.
const BUSY: &str = "\x1b]9;4;1;0\x07";
const IDLE: &str = "\x1b]9;4;0;0\x07";

/// Marks a model turn as busy for as long as it lives. A no-op outside
/// comint, where the spinner already shows the state. `Drop` runs on every
/// exit path — return, break, `?`, or unwind — so a turn can never leave
/// the status line stuck on thinking.
pub(crate) struct TurnStatus;

impl TurnStatus {
    pub fn new() -> Self {
        if output::comint() {
            eprint!("{BUSY}");
        }
        Self
    }
}

impl Drop for TurnStatus {
    fn drop(&mut self) {
        if output::comint() {
            eprint!("{IDLE}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The busy and clear sentinels differ, and neither is a prefix of the
    /// other — a filter can tell them apart without ordering assumptions.
    #[test]
    fn sentinels_are_distinct_osc_9_4_reports() {
        assert!(BUSY.starts_with("\x1b]9;4;"));
        assert!(IDLE.starts_with("\x1b]9;4;"));
        assert_ne!(BUSY, IDLE);
        assert!(BUSY.ends_with('\x07'));
        assert!(IDLE.ends_with('\x07'));
    }
}
