//! One line of process state, aimed at the status line rather than the
//! stream. The spinner stays silent in comint; these reports replace it
//! as the busy heartbeat.
//!
//! Two facts, two writers. The *title* is the registry's active alias and is
//! announced from `ModelRegistry` the moment it changes — nowhere else, so it
//! cannot drift from the model requests actually use. The *state* is busy or
//! idle and belongs to a turn, so a [`TurnStatus`] guard owns it. Every
//! report repeats the title, so the mode line keeps naming the model through
//! idle as well as working.
//!
//! [`State`] holds the two facts and renders them; the free functions below
//! are the locking, terminal-aware shell around it, which is what keeps the
//! rendering testable without a terminal or a lock.

use std::sync::Mutex;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::output;

/// OSC 7501, the Program Status Protocol: a program reports its own state to
/// the terminal as `key=value` pairs. Emacs does not act on it, but a small
/// filter can lift the state and the model alias into the buffer's mode line.
///
/// The state vocabulary is `idle`/`working`/`done`/`blocked`/`error`; `clear`
/// removes the record. `app` is required on every report because a report
/// replaces its record whole. `title` carries the current model alias — the
/// only place the alias belongs, since `id` addresses a record rather than
/// naming a value. `msg` stays absent: a filter that wants the model reads
/// `title`, and terminals that show `msg` never need to.
///
/// Reports are ST-terminated.
fn report(state: &str, title: &str) -> String {
    let encoded = STANDARD.encode(title);
    format!("\x1b]7501;state={state}:app=oneloop:title={encoded}\x1b\\")
}

/// What the mode line shows: the model it names, whether a turn runs, and
/// whether some other worker has borrowed the title for the duration of a
/// turn (see [`TurnStatus::named`]).
#[derive(Default)]
struct State {
    title: String,
    busy: bool,
    borrowed_title: Option<String>,
}

impl State {
    /// The record for the state as it stands. `borrowed_title` wins because a
    /// borrowed turn is the thing running right now.
    fn render(&self) -> String {
        let verb = if self.busy { "working" } else { "idle" };
        let title = self.borrowed_title.as_deref().unwrap_or(&self.title);
        report(verb, title)
    }

    /// Point the title at `alias`; true when it actually moved. Repeated for
    /// the same alias — every response commits the model that answered — and
    /// returning false lets the caller skip a report it need not send.
    fn set_title(&mut self, alias: &str) -> bool {
        if self.title == alias {
            return false;
        }
        self.title = alias.to_string();
        true
    }

    fn begin_turn(&mut self, borrowed: Option<&str>) {
        self.busy = true;
        self.borrowed_title = borrowed.map(str::to_string);
    }

    fn end_turn(&mut self) {
        self.busy = false;
        self.borrowed_title = None;
    }
}

/// One process has one terminal, so one instance; the `Mutex` is for the two
/// writers that are not the same task — a turn and a model change. A poisoned
/// lock means a panic mid-report, and a stale status line is never worth
/// panicking the agent over: every writer takes the lock leniently and gives
/// up quietly instead.
static STATE: Mutex<State> = Mutex::new(State {
    title: String::new(),
    busy: false,
    borrowed_title: None,
});

/// Send the record. Outside comint no one reads it, and a report would just
/// be stray bytes in a pipe.
fn emit(state: &State) {
    if output::comint() {
        eprint!("{}", state.render());
    }
}

/// Name `alias` on the status line, keeping the current state.
///
/// The registry calls this whenever the active model moves, so the title is
/// never set from anywhere else. Callers that move the active model announce
/// through here rather than by hand, which is what keeps the title from
/// drifting away from the model requests actually use.
pub(crate) fn model_changed(alias: &str) {
    let Ok(mut state) = STATE.lock() else { return };
    if state.set_title(alias) {
        emit(&state);
    }
}

/// Marks a turn as busy for as long as it lives. A no-op outside comint,
/// where the spinner already shows the state. `Drop` runs on every exit path
/// — return, break, `?`, or unwind — so a turn can never leave the status
/// line stuck on thinking.
pub(crate) struct TurnStatus;

impl TurnStatus {
    /// Begins a model turn, reporting `working`. The title is whatever the
    /// registry last announced; a model turn never names a model of its own.
    pub fn new() -> Self {
        Self::begin(None)
    }

    /// Begins a turn run by something the registry does not model — Claude
    /// Code, reached by shelling out. The borrowed title lasts only as long
    /// as the guard: `Drop` restores the registry's, so a review cannot leave
    /// the mode line naming the wrong model afterwards.
    pub fn named(title: &str) -> Self {
        Self::begin(Some(title))
    }

    fn begin(title: Option<&str>) -> Self {
        if let Ok(mut state) = STATE.lock() {
            state.begin_turn(title);
            emit(&state);
        }
        Self
    }
}

impl Drop for TurnStatus {
    fn drop(&mut self) {
        // Reading the shared title rather than holding one means a fallback
        // mid-turn is named correctly: the idle report can never point at the
        // model that failed.
        if let Ok(mut state) = STATE.lock() {
            state.end_turn();
            emit(&state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report is a well-formed OSC 7501 sequence: introducer, `state`
    /// first, base64 `title`, and the ST terminator.
    #[test]
    fn report_is_a_well_formed_osc_7501_sequence() {
        let busy = report("working", "qwen");
        assert_eq!(
            busy,
            "\x1b]7501;state=working:app=oneloop:title=cXdlbg==\x1b\\"
        );
        assert!(busy.starts_with("\x1b]7501;"));
        assert!(busy.ends_with("\x1b\\"));
    }

    /// A title is base64 across its bytes, so a non-ASCII alias survives the
    /// round trip the model aliases in `config.json` require.
    #[test]
    fn a_non_ascii_title_encodes_its_utf8_bytes() {
        assert_eq!(
            STANDARD.encode("ünïcode"),
            STANDARD.encode("ünïcode".as_bytes())
        );
    }

    /// `working` and `idle` differ only in the state, so a filter reads the
    /// state and keeps the same model through both.
    #[test]
    fn a_turn_toggles_the_verb_and_keeps_the_title() {
        let mut state = State::default();
        state.set_title("qwen");
        assert!(state.render().contains("state=idle"));
        state.begin_turn(None);
        assert!(state.render().contains("state=working"));
        assert!(state.render().contains(":title=cXdlbg=="));
        state.end_turn();
        assert!(state.render().contains("state=idle"));
    }

    /// A repeat of the same alias is not a change: the caller gets false and
    /// skips the report, so an unchanged turn stays two writes.
    #[test]
    fn repeating_the_active_alias_is_not_a_change() {
        let mut state = State::default();
        assert!(state.set_title("qwen"));
        assert!(!state.set_title("qwen"));
        assert!(state.set_title("flash"));
    }

    /// The bug this design fixes: a model change mid-turn (a fallback, or
    /// `/model` at the prompt) must be named by the idle report, not the
    /// model that began the turn.
    #[test]
    fn a_model_change_during_a_turn_is_named_on_idle() {
        let mut state = State::default();
        state.set_title("qwen");
        state.begin_turn(None);
        state.set_title("flash");
        assert!(state.render().contains(&STANDARD.encode("flash")));
        state.end_turn();
        assert!(state.render().contains(&STANDARD.encode("flash")));
    }

    /// A borrowed title wins while its turn runs — including over a model
    /// change during it — and the registry's title returns afterwards, so a
    /// review never leaves the mode line naming the wrong model.
    #[test]
    fn a_named_turn_borrows_the_title_and_returns_it() {
        let mut state = State::default();
        state.set_title("qwen");
        state.begin_turn(Some("claude"));
        assert!(state.render().contains(&STANDARD.encode("claude")));
        state.end_turn();
        assert!(state.render().contains(&STANDARD.encode("qwen")));
        assert!(state.borrowed_title.is_none());
    }
}
