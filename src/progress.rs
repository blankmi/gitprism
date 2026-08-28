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

use console::{Color, Term, style, user_attended_stderr};
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
/// Actions all already agree on — plus decisions/0024's `Warning`, magenta so
/// it never reads as `Skipped`'s yellow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Skipped,
    /// decisions/0024: a branch gitprism refuses to touch and will keep
    /// re-reporting every run — distinct from `Skipped`'s genuinely benign,
    /// one-time no-op, rendered in its own color (magenta) so the two never
    /// read as the same thing on a fast scan.
    Warning,
    Error,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Skipped => "skipped",
            Outcome::Warning => "warning",
            Outcome::Error => "error",
        }
    }

    fn color(self) -> Color {
        match self {
            Outcome::Done => Color::Green,
            Outcome::Skipped => Color::Yellow,
            Outcome::Warning => Color::Magenta,
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
///
/// `branch` is repository-controlled (any name `git check-ref-format`
/// accepts) and escaped before it ever reaches this format string —
/// `git check-ref-format` allows C1 controls and Unicode line/paragraph
/// separators that would otherwise forge terminal output or split a log
/// line (F-09).
fn plain_step_line(branch: &str, message: &str) -> String {
    format!("{}: {message}", escape_branch(branch))
}

/// The single point every [`Reporter`]-printed line escapes a
/// repository-controlled branch name through before formatting — reuses
/// [`crate::git::escape_bytes`] rather than reimplementing its control/
/// format-character handling.
fn escape_branch(branch: &str) -> String {
    crate::git::escape_bytes(branch.as_bytes())
}

/// The default terminal width `Reporter::step` assumes when the real width
/// can't be determined — `console::Term::size`'s own fallback
/// (`console::DEFAULT_WIDTH`, currently 80), kept as a local literal since
/// `console` doesn't expose that constant publicly.
const FALLBACK_TERM_WIDTH: u16 = 80;

/// How long a branch name is allowed to run in the pinned bar's one-line step
/// message before [`truncate_branch`] shortens it, given the terminal's
/// current column count. We *can* ask the terminal its width
/// (`console::Term::stderr().size()`), so this scales with it rather than
/// guessing a single fixed number: a quarter of the line for the branch name
/// leaves room for the fixed "Sync … -> dest - " decoration plus at least
/// some of `message`, floored at 15 so a narrow terminal still shows enough
/// of the name to recognize it, and capped at 40 so one very wide terminal
/// doesn't let a single absurd branch name dominate the whole line.
fn max_branch_len(term_width: u16) -> usize {
    ((term_width as usize) / 4).clamp(15, 40)
}

/// Shortens `branch` to at most `max_len` characters, replacing the tail with
/// a single `…` when it doesn't fit — used only for the pinned bar's one-line
/// step message, which (unlike the completed lines above it) can't wrap
/// without visually corrupting the bar. Escapes `branch` first (F-09) — both
/// of this function's callers ([`tty_step_message`] directly,
/// [`colored_complete_lines`] via [`pad_branch`]) print the result straight
/// to the terminal via `Display`.
fn truncate_branch(branch: &str, max_len: usize) -> String {
    let branch = escape_branch(branch);
    if branch.chars().count() <= max_len {
        return branch;
    }
    let head: String = branch.chars().take(max_len.saturating_sub(1)).collect();
    format!("{head}…")
}

/// The longest of [`Direction::arrow_target`]'s two words ("dest"/"source") —
/// the completed lines pad the arrow's target out to this width so their
/// trailing `- {status}` column lines up regardless of which direction a
/// given line is. The step line deliberately does *not* use this: it's
/// redrawn on every step, so there's no column to keep stable across lines
/// the way there is for the permanent completed-line log above it.
const TARGET_WORD_WIDTH: usize = 6; // "source".len()

/// Truncates `branch` to `width` (see [`truncate_branch`]) and then pads it
/// out to exactly `width` characters with trailing spaces — used by the
/// completed lines so every one of their `- {status}` columns starts at the
/// same position regardless of how long any one branch name happens to be.
/// Padding a *truncated* name, rather than the raw one, keeps this always
/// exactly `width` characters even for a name that had to be shortened. The
/// step line uses [`truncate_branch`] directly instead, without padding — see
/// [`tty_step_message`].
fn pad_branch(branch: &str, width: usize) -> String {
    format!("{:<width$}", truncate_branch(branch, width))
}

/// The single branch-name column width [`Reporter`] uses for every line this
/// run prints — the longest name actually in play this run (so short-named
/// runs don't get padded out to some unrelated worst case), capped at `cap`
/// (see [`max_branch_len`]) so one absurdly long branch name doesn't blow the
/// column out for every other line. Computed once, upfront, from the same
/// branch lists `sync::run` already gathered to size the bar's total — not
/// re-derived per line — so the column stays put even if the terminal is
/// resized mid-run.
fn branch_column_width<'a>(branches: impl Iterator<Item = &'a str>, cap: usize) -> usize {
    branches
        .map(|b| b.chars().count())
        .max()
        .unwrap_or(0)
        .min(cap)
}

/// The fixed status word the step line's `Sync {branch} -> {target} -
/// {status}` shows while a step is in flight — matching
/// [`colored_complete_lines`]'s completed-line shape, but `working` rather
/// than an [`Outcome`] label, since there's no outcome yet. Unlike the actual
/// step detail (`message` — e.g. "fetching dest (finding resume point before
/// merging from source)"), this word never changes length, so it's the piece
/// that shape's status column actually stays useful for on a redrawn line.
const WORKING_LABEL: &str = "working";

/// The pinned bar's message for one in-flight step (e.g. two lines: `Sync
/// main -> dest - working` followed by an indented `fetching dest (finding
/// resume point before merging from source)`). Direction is included
/// deliberately, not just decorative: `sync_pair_to_dest`'s round-tripped
/// fetch and `sync_pair_from_dest`'s fetch both start with the literal words
/// "fetching dest", so without a direction indicator someone watching the
/// live bar can't tell which phase is in flight for a given branch from the
/// first line alone. `message` — the actual step detail — is always longer
/// and more variable in length than a fixed status word, so it's placed on
/// its own indented line beneath rather than packed onto the first line next
/// to the arrow; it keeps the same (unstyled) color as the line above it
/// rather than being dimmed like [`colored_complete_lines`]'s notes are —
/// this is live in-progress detail, not a demoted explanation. `branch` is
/// truncated ([`truncate_branch`]) but, unlike [`colored_complete_lines`]'s
/// completed lines, deliberately *not* padded: this line is replaced wholesale
/// on every step (there's no run of these to keep a column aligned across),
/// so the extra trailing spaces would only be visual noise between the
/// branch name and its arrow.
fn tty_step_message(
    branch: &str,
    direction: Direction,
    message: &str,
    max_branch_len: usize,
) -> String {
    format!(
        "Sync {} -> {} - {WORKING_LABEL}\n    {message}",
        truncate_branch(branch, max_branch_len),
        direction.arrow_target()
    )
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
    let branch = escape_branch(branch);
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
/// summary line, with any `note` demoted to its own indented line beneath —
/// Cargo's verb/noun-plus-note convention (decisions/0020 point 5), kept
/// separate from the two-line non-tty output [`plain_complete_line`]
/// deliberately avoids. Returns one line when there's no note, two when
/// there is. Shares the step line's `Sync {branch} -> {target} - {status}`
/// phrasing (here `status` is the final outcome rather than an in-flight
/// message) and its fixed-width branch/target padding ([`pad_branch`],
/// [`TARGET_WORD_WIDTH`]), so every completed line's `- {status}` column
/// lines up in the same place, whether or not the branch is still in
/// progress, round-tripped, short, or truncated.
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
    branch_column_width: usize,
) -> Vec<String> {
    let result = style(outcome.label())
        .fg(outcome.color())
        .force_styling(colors);
    let padded_branch = pad_branch(branch, branch_column_width);
    let branch_styled = match branch_color(round_tripped) {
        Some(color) => style(padded_branch)
            .fg(color)
            .force_styling(colors)
            .to_string(),
        None => style(padded_branch).force_styling(colors).to_string(),
    };

    let mut lines = vec![format!(
        "Sync {branch_styled} -> {:<width$} - {result}",
        direction.arrow_target(),
        width = TARGET_WORD_WIDTH
    )];
    if let Some(note) = note {
        lines.push(format!("    {}", style(note).dim().force_styling(colors)));
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
    /// Fixed for the whole run (see [`branch_column_width`]) rather than
    /// recomputed per line. The completed-line log above the bar pads every
    /// branch name to this width so its `- {status}` column stays aligned
    /// line to line; the step line below the bar reuses the same number only
    /// as a truncation cap ([`tty_step_message`]), not for padding, since it's
    /// redrawn in place rather than accumulated as a log.
    branch_column_width: usize,
}

impl Reporter {
    /// `branch_names` is every branch this run will ever print a line for —
    /// `sync::run` passes `config.branches` chained with the already-listed
    /// source branches, the same two lists it combines to compute `total`.
    pub fn new<'a>(total: usize, branch_names: impl IntoIterator<Item = &'a str>) -> Self {
        let tty = user_attended_stderr();
        let multi = MultiProgress::new();
        let bar = multi.add(ProgressBar::new(total as u64));
        let term_width = Term::stderr()
            .size_checked()
            .map_or(FALLBACK_TERM_WIDTH, |(_, cols)| cols);
        let branch_column_width =
            branch_column_width(branch_names.into_iter(), max_branch_len(term_width));
        // A real filled/unfilled bar glyph (`{bar}`), not just an `[k/n]`
        // count — Gradle's own rich console (the prior art decisions/0020
        // named) pairs exactly this with a percentage and elapsed time, and
        // `indicatif` renders all three natively; the exact template string
        // was left as unsettled implementation detail by that decision.
        //
        // `{msg}` sits on its own line, after a literal `\n` — decisions/0020
        // point 2's own shape is "progress bar, current step below it," not
        // the current step trailing the bar on the same line. `indicatif`
        // renders a template's embedded newlines as genuinely separate lines
        // (confirmed in `indicatif` 0.18.6's own source:
        // `style.rs::expand_template` splits on `'\n'`, and `MultiProgress`'s
        // own `visual_line_count` already accounts for a multi-line member
        // when clearing/redrawing) — no second `ProgressBar` needed just to
        // get a second line.
        if let Ok(template) = ProgressStyle::with_template(
            "{bar:30.cyan/blue} [{pos}/{len}] branches [{elapsed}]\n{msg}",
        ) {
            bar.set_style(template);
        }
        Self {
            multi,
            bar,
            tty,
            branch_column_width,
        }
    }

    /// Updates the pinned bar's message with the branch/direction/step
    /// currently in flight, replacing today's "in progress" `eprintln!` call
    /// sites (e.g. "fetching dest ..."). When stderr isn't a terminal the bar
    /// never renders at all, so this prints the equivalent plain line
    /// directly instead of updating a message nobody would see.
    pub fn step(&self, branch: &str, direction: Direction, message: &str) {
        if self.tty {
            self.bar.set_message(tty_step_message(
                branch,
                direction,
                message,
                self.branch_column_width,
            ));
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
            for line in colored_complete_lines(
                outcome,
                branch,
                direction,
                round_tripped,
                note,
                true,
                self.branch_column_width,
            ) {
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
    fn plain_step_line_escapes_a_c1_control_in_the_branch_name() {
        let line = plain_step_line("feature/\u{9b}pwn", "fetching dest");
        assert!(!line.contains('\u{9b}'), "escaped: {line:?}");
        assert!(line.contains("\\x9B"));
    }

    #[test]
    fn tty_step_message_names_the_direction_not_just_the_branch() {
        assert_eq!(
            tty_step_message(
                "main",
                Direction::SourceToDest,
                "fetching dest (finding resume point before merging from source)",
                30
            ),
            "Sync main -> dest - working\n    fetching dest (finding resume point before merging from source)"
        );
        assert_eq!(
            tty_step_message(
                "main",
                Direction::DestToSource,
                "fetching dest (checking for independent content to reflect into source)",
                30
            ),
            "Sync main -> source - working\n    fetching dest (checking for independent content to reflect into source)",
            "both directions' first step happens to start with the same words \
             (\"fetching dest\") — the direction must still distinguish them"
        );
    }

    #[test]
    fn tty_step_message_does_not_pad_branch_or_target() {
        assert_eq!(
            tty_step_message("main", Direction::SourceToDest, "fetching dest", 30),
            "Sync main -> dest - working\n    fetching dest",
            "the first line is redrawn on every step, not accumulated as a log, so there's no \
             column to keep aligned across lines — unlike colored_complete_lines it should not \
             pad the branch name or the arrow's target word"
        );
    }

    #[test]
    fn tty_step_message_puts_the_variable_length_detail_on_its_own_indented_line() {
        let message = tty_step_message(
            "main",
            Direction::SourceToDest,
            "fetching dest (finding resume point before merging from source)",
            30,
        );
        let mut lines = message.lines();
        assert_eq!(
            lines.next(),
            Some("Sync main -> dest - working"),
            "the first line names branch, direction, and a fixed-length status word, matching \
             colored_complete_lines' shape — not the variable-length detail message"
        );
        assert_eq!(
            lines.next(),
            Some("    fetching dest (finding resume point before merging from source)"),
            "the detail message — always longer and more variable than a status word — goes on \
             its own indented line beneath"
        );
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn tty_step_message_truncates_an_overly_long_branch_name() {
        let long_branch = "feature/this-is-a-very-long-branch-name-that-goes-on-and-on-and-on";
        let message = tty_step_message(long_branch, Direction::SourceToDest, "fetching dest", 20);
        assert!(
            !message.contains(long_branch),
            "the full branch name must not appear once truncated: {message:?}"
        );
        assert!(message.starts_with("Sync feature/"));
        assert!(message.contains('…'));
    }

    #[test]
    fn truncate_branch_leaves_short_names_untouched() {
        assert_eq!(truncate_branch("main", 30), "main");
    }

    #[test]
    fn truncate_branch_escapes_a_c1_control_character() {
        let branch = "feature/\u{9b}pwn";
        let truncated = truncate_branch(branch, 30);
        assert!(
            !truncated.contains('\u{9b}'),
            "a raw CSI byte must never reach the terminal: {truncated:?}"
        );
        assert!(truncated.contains("\\x9B"));
    }

    #[test]
    fn truncate_branch_escapes_a_line_separator_character() {
        let branch = "feature/\u{2028}pwn";
        let truncated = truncate_branch(branch, 30);
        assert!(
            !truncated.contains('\u{2028}'),
            "a raw U+2028 line separator must never reach the terminal: {truncated:?}"
        );
        assert!(truncated.contains("\\u{2028}"));
    }

    #[test]
    fn truncate_branch_shortens_long_names_with_an_ellipsis() {
        let long = "feature/this-is-a-very-long-branch-name-that-goes-on-and-on";
        let truncated = truncate_branch(long, 20);
        assert_eq!(truncated.chars().count(), 20);
        assert!(truncated.ends_with('…'));
        let head_len = truncated.chars().count() - 1;
        assert_eq!(
            long.chars().take(head_len).collect::<String>(),
            truncated.chars().take(head_len).collect::<String>()
        );
    }

    #[test]
    fn max_branch_len_scales_with_terminal_width_within_sensible_bounds() {
        assert_eq!(
            max_branch_len(40),
            15,
            "a narrow terminal should floor at a still-readable length"
        );
        assert_eq!(
            max_branch_len(100),
            25,
            "a typical terminal should get roughly a quarter of the line"
        );
        assert_eq!(
            max_branch_len(400),
            40,
            "a very wide terminal shouldn't let one branch name dominate the whole line"
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
    fn plain_complete_line_escapes_a_line_separator_in_the_branch_name() {
        let line = plain_complete_line(
            Outcome::Done,
            "feature/\u{2028}pwn",
            Direction::SourceToDest,
            None,
        );
        assert!(!line.contains('\u{2028}'), "escaped: {line:?}");
        assert!(line.contains("\\u{2028}"));
    }

    #[test]
    fn plain_complete_line_names_warning_as_its_own_label_not_skipped() {
        assert_eq!(
            plain_complete_line(Outcome::Warning, "ai-setup", Direction::SourceToDest, None),
            "ai-setup: warning (source -> dest)",
            "decisions/0024: Warning must read as its own word, not reuse Skipped's label"
        );
    }

    #[test]
    fn colored_complete_lines_matches_the_sync_x_arrow_y_dash_status_shape() {
        assert_eq!(
            colored_complete_lines(
                Outcome::Done,
                "main",
                Direction::SourceToDest,
                true,
                None,
                false,
                30
            ),
            vec![format!("Sync {:<30} -> {:<6} - done", "main", "dest")],
            "the completed line should read the same shape as the in-flight step line \
             (Sync {{branch}} -> {{target}} - {{status}}), just with the final outcome as status, \
             padded the same way so the status column lines up"
        );
    }

    #[test]
    fn colored_complete_lines_truncates_an_overly_long_branch_name() {
        let long_branch = "feature/this-is-a-very-long-branch-name-that-goes-on-and-on-and-on";
        let lines = colored_complete_lines(
            Outcome::Skipped,
            long_branch,
            Direction::SourceToDest,
            false,
            None,
            false,
            20,
        );
        assert!(
            !lines[0].contains(long_branch),
            "the full branch name must not appear once truncated: {lines:?}"
        );
        assert!(lines[0].contains('…'));
    }

    #[test]
    fn colored_complete_lines_escapes_a_c1_control_and_a_line_separator_in_the_branch_name() {
        let lines = colored_complete_lines(
            Outcome::Done,
            "feature/\u{9b}pwn",
            Direction::SourceToDest,
            false,
            None,
            false,
            30,
        );
        assert!(!lines[0].contains('\u{9b}'), "escaped: {lines:?}");
        assert!(lines[0].contains("\\x9B"));

        let lines = colored_complete_lines(
            Outcome::Done,
            "feature/\u{2028}pwn",
            Direction::SourceToDest,
            false,
            None,
            false,
            30,
        );
        assert!(!lines[0].contains('\u{2028}'), "escaped: {lines:?}");
        assert!(lines[0].contains("\\u{2028}"));
    }

    #[test]
    fn colored_complete_lines_leaves_a_plain_ascii_branch_name_unchanged() {
        let lines = colored_complete_lines(
            Outcome::Done,
            "feature/plain-name",
            Direction::SourceToDest,
            false,
            None,
            false,
            30,
        );
        assert!(lines[0].contains("feature/plain-name"));
    }

    #[test]
    fn colored_complete_lines_aligns_the_status_column_across_branch_names_and_directions() {
        let short = colored_complete_lines(
            Outcome::Done,
            "main",
            Direction::SourceToDest,
            true,
            None,
            false,
            30,
        );
        let long = colored_complete_lines(
            Outcome::Skipped,
            "feature/a-much-longer-branch-name",
            Direction::DestToSource,
            false,
            None,
            false,
            30,
        );

        assert_eq!(
            status_column(&short[0]),
            status_column(&long[0]),
            "the status column should start at the same position regardless of branch length \
             or direction: {short:?} vs {long:?}"
        );
    }

    /// The *character* position of the ` - ` immediately before a line's
    /// status word — not `str::rfind`'s byte offset, which disagrees with the
    /// visual column as soon as one line's branch name was truncated (the `…`
    /// [`truncate_branch`] appends is 3 bytes but a single character/column).
    fn status_column(line: &str) -> usize {
        let chars: Vec<char> = line.chars().collect();
        chars
            .windows(3)
            .rposition(|w| w == [' ', '-', ' '])
            .expect("a status line always has a \" - \" before its status")
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
                true,
                30
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
                true,
                30
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
            30,
        );
        let mirror_only = colored_complete_lines(
            Outcome::Done,
            "main",
            Direction::SourceToDest,
            false,
            None,
            true,
            30,
        );

        let cyan = style(format!("{:<30}", "main"))
            .cyan()
            .force_styling(true)
            .to_string();
        let plain = style(format!("{:<30}", "main"))
            .force_styling(true)
            .to_string();

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
    fn colored_complete_lines_renders_warning_magenta_distinct_from_skipped_yellow() {
        let warning = colored_complete_lines(
            Outcome::Warning,
            "ai-setup",
            Direction::SourceToDest,
            false,
            None,
            true,
            30,
        );
        let expected_magenta_warning = style("warning")
            .fg(Color::Magenta)
            .force_styling(true)
            .to_string();
        assert!(
            warning[0].contains(&expected_magenta_warning),
            "decisions/0024: Warning renders magenta, not Skipped's yellow: {warning:?}"
        );
        assert!(
            !warning[0].contains("skipped"),
            "Warning must not reuse Skipped's label: {warning:?}"
        );
    }

    #[test]
    fn finish_leaves_the_bar_in_a_finished_state() {
        let reporter = Reporter::new(2, std::iter::empty());
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
            30,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("    "));
        assert!(lines[1].contains("already merged into"));
    }

    #[test]
    fn colored_complete_lines_note_is_dimmed_not_default_styled() {
        let lines = colored_complete_lines(
            Outcome::Skipped,
            "feature/foo",
            Direction::SourceToDest,
            false,
            Some("already merged into \"main\""),
            true,
            30,
        );

        let dimmed = style("already merged into \"main\"")
            .dim()
            .force_styling(true)
            .to_string();
        assert!(
            lines[1].contains(&dimmed),
            "the note line should be dimmed, not rendered in the terminal's default style: {:?}",
            lines[1]
        );
    }
}
