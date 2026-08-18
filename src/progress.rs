//! `sync`'s status output (decisions/0020): a pinned `indicatif::MultiProgress`
//! bar showing overall `[k/n] branches` progress plus the branch/direction/
//! step currently in flight, with every finished branch-operation printed as
//! a permanent colored line above it via `MultiProgress::println` —
//! Gradle-rich-console-style, not a bespoke layout.
//!
//! Falls back to plain sequential lines whenever stderr isn't a terminal
//! (the same non-interactive case design/playbooks/0001 already documents).
//! The pinned bar itself is already hidden automatically in that case by
//! indicatif's own `ProgressDrawTarget::stderr()` auto-detection — no need to
//! hand-roll that part. But `MultiProgress::println` is NOT left to that same
//! auto-detection: its own doc comment says plainly that once its draw target
//! is hidden, `println` "will not do anything" at all — relying on that would
//! make CI output go silent instead of degrading to today's plain lines, so
//! [`Reporter`] checks `console::user_attended_stderr()` itself, once, at
//! construction, and prints plain text directly in the non-tty case rather
//! than routing through `MultiProgress` at all.

use console::{Color, style, user_attended_stderr};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Which of `sync::run`'s two phases (decisions/0017) a branch-operation
/// belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    SourceToDest,
    DestToSource,
}

impl Direction {
    /// The word an arrow points at in a colored completed line, e.g. `main →
    /// dest` for source→dest (decisions/0020's own example line shape).
    fn arrow_target(self) -> &'static str {
        match self {
            Direction::SourceToDest => "dest",
            Direction::DestToSource => "source",
        }
    }

    /// The ASCII phrase used in the plain-text (non-tty) fallback — no
    /// unicode arrow assumed for a dumb terminal or a redirected CI log.
    fn phrase(self) -> &'static str {
        match self {
            Direction::SourceToDest => "source -> dest",
            Direction::DestToSource => "dest -> source",
        }
    }
}

/// How one finished branch-operation turned out (decisions/0020): colored
/// green/yellow/red respectively — the convention Cargo, Gradle, and GitHub
/// Actions all already agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Skipped,
    Error,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Skipped => "skipped",
            Outcome::Error => "error",
        }
    }

    fn color(self) -> Color {
        match self {
            Outcome::Done => Color::Green,
            Outcome::Skipped => Color::Yellow,
            Outcome::Error => Color::Red,
        }
    }
}

/// Cyan if `branch` is round-tripped (named in `config.branches`), or `None`
/// (plain terminal foreground) if it's a decisions/0017 mirror-only branch —
/// deliberately never dimmed. decisions/0020 argues this through explicitly:
/// dimming would misrepresent the mirror-only majority as de-emphasized, when
/// it's actually the ordinary, zero-config case decisions/0017 was built for,
/// and a dim line reads worse on a fast scan than a plain one does. A pure
/// function so the color decision itself is unit-testable without a live
/// terminal.
fn branch_color(round_tripped: bool) -> Option<Color> {
    round_tripped.then_some(Color::Cyan)
}

/// The exact wording [`Reporter::step`] falls back to when stderr isn't a
/// terminal — identical in content to today's `eprintln!("{branch}:
/// {message}")` call sites, since a step-in-progress message never had a
/// colored/split form to begin with (only completed lines do — decisions/0020
/// point 6 is scoped to those).
fn plain_step_line(branch: &str, message: &str) -> String {
    format!("{branch}: {message}")
}

/// The pinned bar's message for one in-flight step (decisions/0020 point 2's
/// own example: `main → dest: fetching dest`). Direction is included
/// deliberately, not just decorative: `sync_pair_to_dest`'s round-tripped
/// fetch and `sync_pair_from_dest`'s fetch both start with the literal words
/// "fetching dest", so without a direction indicator someone watching the
/// live bar can't tell which phase is in flight for a given branch from the
/// step line alone.
fn tty_step_message(branch: &str, direction: Direction, message: &str) -> String {
    format!("{branch} → {}: {message}", direction.arrow_target())
}

/// The non-tty fallback for one finished branch-operation: a single
/// undecorated line. decisions/0020 point 6 explicitly rules out the
/// two-line summary/note split for this case ("not a two-line summary/note
/// split that only makes sense with a terminal's line-wrapping"), so `note`
/// is folded back into the one line instead of placed on its own — mirroring
/// today's single-sentence `eprintln!` shape, even though the exact wording
/// isn't required to match byte-for-byte (decisions/0020 leaves that
/// unsettled).
fn plain_complete_line(
    outcome: Outcome,
    branch: &str,
    direction: Direction,
    note: Option<&str>,
) -> String {
    match note {
        Some(note) => format!(
            "{branch}: {} ({}) — {note}",
            outcome.label(),
            direction.phrase()
        ),
        None => format!("{branch}: {} ({})", outcome.label(), direction.phrase()),
    }
}

/// The terminal form of one finished branch-operation: a short colored
/// summary line (result + branch name + direction), with any `note` demoted
/// to its own indented line beneath — Cargo's verb/noun-plus-note convention
/// (decisions/0020 point 5), kept separate from the two-line non-tty output
/// [`plain_complete_line`] deliberately avoids. Returns one line when there's
/// no note, two when there is.
///
/// `colors` exists only so this is unit-testable without a real terminal —
/// `force_styling` makes `console::style` emit ANSI codes regardless of tty
/// auto-detection, the same pattern `console`'s own test suite uses.
/// [`Reporter`] only ever calls this with `colors: true`, since it only takes
/// this path once it has already established stderr is a terminal.
fn colored_complete_lines(
    outcome: Outcome,
    branch: &str,
    direction: Direction,
    round_tripped: bool,
    note: Option<&str>,
    colors: bool,
) -> Vec<String> {
    let result = style(format!("{:<7}", outcome.label()))
        .fg(outcome.color())
        .force_styling(colors);
    let branch_styled = match branch_color(round_tripped) {
        Some(color) => style(branch.to_string())
            .fg(color)
            .force_styling(colors)
            .to_string(),
        None => style(branch.to_string()).force_styling(colors).to_string(),
    };

    let mut lines = vec![format!(
        "{result} {branch_styled} → {}",
        direction.arrow_target()
    )];
    if let Some(note) = note {
        lines.push(format!("    {note}"));
    }
    lines
}

/// One `sync` run's status reporter (decisions/0020): a pinned
/// `indicatif::MultiProgress` bar showing overall `[k/n] branches` progress
/// plus the branch/direction/step currently in flight, with every finished
/// branch-operation printed as a permanent line above it.
///
/// `total` — `config.branches.len()` plus source's local branch count — is
/// computed upfront by the caller (`sync::run`), before either sync phase
/// starts, so the bar's denominator never jumps mid-run (decisions/0020
/// point 3). Whether stderr is a terminal is checked once, at construction
/// (see the module doc comment for why completed lines don't just rely on
/// `indicatif`'s own non-tty behavior).
pub struct Reporter {
    multi: MultiProgress,
    bar: ProgressBar,
    tty: bool,
}

impl Reporter {
    pub fn new(total: usize) -> Self {
        let tty = user_attended_stderr();
        let multi = MultiProgress::new();
        let bar = multi.add(ProgressBar::new(total as u64));
        if let Ok(template) = ProgressStyle::with_template("[{pos}/{len}] branches {msg}") {
            bar.set_style(template);
        }
        Self { multi, bar, tty }
    }

    /// Updates the pinned bar's message with the branch/direction/step
    /// currently in flight, replacing today's "in progress" `eprintln!` call
    /// sites (e.g. "fetching dest ..."). When stderr isn't a terminal the bar
    /// never renders at all, so this prints the equivalent plain line
    /// directly instead of updating a message nobody would see.
    pub fn step(&self, branch: &str, direction: Direction, message: &str) {
        if self.tty {
            self.bar
                .set_message(tty_step_message(branch, direction, message));
        } else {
            eprintln!("{}", plain_step_line(branch, message));
        }
    }

    /// Marks one branch-operation finished: prints its permanent colored
    /// summary (plus any indented note) above the pinned bar and advances it
    /// by one, or the plain single-line equivalent when stderr isn't a
    /// terminal (decisions/0020 point 6).
    pub fn complete(
        &self,
        outcome: Outcome,
        branch: &str,
        direction: Direction,
        round_tripped: bool,
        note: Option<&str>,
    ) {
        if self.tty {
            for line in
                colored_complete_lines(outcome, branch, direction, round_tripped, note, true)
            {
                let _ = self.multi.println(line);
            }
        } else {
            eprintln!("{}", plain_complete_line(outcome, branch, direction, note));
        }
        self.bar.inc(1);
    }

    /// Ends this run's pinned bar, whether `sync::run` is about to return
    /// `Ok(())` or a hard-stop `anyhow::bail!` is about to unwind past it
    /// (decisions/0020's own Context cites clearing transient UI once a step
    /// is done, per the Evil Martians survey). Without this the bar is left
    /// frozen on whatever its last [`Reporter::step`] message happened to be
    /// — worst on a conflict hard-stop, where a bar frozen mid-fetch sitting
    /// right above the real error text reads as "the run hung," not "the run
    /// errored." `finish_and_clear` removes the bar entirely rather than
    /// leaving a stale completed-looking bar behind — the completed lines
    /// already printed above it (via [`Reporter::complete`]) are the
    /// permanent record of what happened, not the bar itself. A no-op, safe
    /// to call more than once, and harmless when the bar was never rendered
    /// (non-tty).
    pub fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_color_is_cyan_only_when_round_tripped() {
        assert_eq!(branch_color(true), Some(Color::Cyan));
        assert_eq!(branch_color(false), None);
    }

    #[test]
    fn plain_step_line_matches_todays_eprintln_wording() {
        assert_eq!(
            plain_step_line(
                "main",
                "fetching dest (finding resume point before merging from source)"
            ),
            "main: fetching dest (finding resume point before merging from source)"
        );
    }

    #[test]
    fn tty_step_message_names_the_direction_not_just_the_branch() {
        assert_eq!(
            tty_step_message(
                "main",
                Direction::SourceToDest,
                "fetching dest (finding resume point before merging from source)"
            ),
            "main → dest: fetching dest (finding resume point before merging from source)"
        );
        assert_eq!(
            tty_step_message(
                "main",
                Direction::DestToSource,
                "fetching dest (checking for independent content to reflect into source)"
            ),
            "main → source: fetching dest (checking for independent content to reflect into source)",
            "both directions' first step happens to start with the same words \
             (\"fetching dest\") — the direction must still distinguish them"
        );
    }

    #[test]
    fn plain_complete_line_stays_one_line_even_with_a_note() {
        let line = plain_complete_line(
            Outcome::Skipped,
            "feature/foo",
            Direction::SourceToDest,
            Some("already merged into \"main\" and cleaned up there"),
        );
        assert!(
            !line.contains('\n'),
            "non-tty fallback must stay one line (decisions/0020 point 6): {line:?}"
        );
        assert!(line.contains("feature/foo"));
        assert!(line.contains("skipped"));
        assert!(line.contains("already merged into"));
    }

    #[test]
    fn plain_complete_line_without_a_note_names_branch_outcome_and_direction() {
        assert_eq!(
            plain_complete_line(Outcome::Done, "main", Direction::DestToSource, None),
            "main: done (dest -> source)"
        );
    }

    #[test]
    fn colored_complete_lines_has_one_line_when_no_note_two_when_present() {
        assert_eq!(
            colored_complete_lines(
                Outcome::Done,
                "main",
                Direction::SourceToDest,
                true,
                None,
                true
            )
            .len(),
            1
        );
        assert_eq!(
            colored_complete_lines(
                Outcome::Done,
                "main",
                Direction::SourceToDest,
                true,
                Some("why"),
                true
            )
            .len(),
            2
        );
    }

    #[test]
    fn colored_complete_lines_colors_branch_cyan_only_when_round_tripped() {
        let round_tripped = colored_complete_lines(
            Outcome::Done,
            "main",
            Direction::SourceToDest,
            true,
            None,
            true,
        );
        let mirror_only = colored_complete_lines(
            Outcome::Done,
            "main",
            Direction::SourceToDest,
            false,
            None,
            true,
        );

        let cyan = style("main").cyan().force_styling(true).to_string();
        let plain = style("main").force_styling(true).to_string();

        assert!(
            round_tripped[0].contains(&cyan),
            "a round-tripped branch name should render cyan: {round_tripped:?}"
        );
        assert!(
            !mirror_only[0].contains(&cyan),
            "a mirror-only branch name must not render cyan (decisions/0020 rejects dimming it too): {mirror_only:?}"
        );
        assert!(mirror_only[0].contains(&plain));
    }

    #[test]
    fn finish_leaves_the_bar_in_a_finished_state() {
        let reporter = Reporter::new(2);
        assert!(
            !reporter.bar.is_finished(),
            "a freshly constructed reporter's bar must not already read as finished"
        );

        reporter.finish();

        assert!(
            reporter.bar.is_finished(),
            "finish() must leave the bar in a finished state, not frozen mid-step"
        );
    }

    #[test]
    fn colored_complete_lines_notes_line_is_indented_beneath_the_summary() {
        let lines = colored_complete_lines(
            Outcome::Skipped,
            "feature/foo",
            Direction::SourceToDest,
            false,
            Some("already merged into \"main\""),
            true,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("    "));
        assert!(lines[1].contains("already merged into"));
    }
}
