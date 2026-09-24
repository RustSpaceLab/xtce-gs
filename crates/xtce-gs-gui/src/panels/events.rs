//! The event log: what the station said, in order, with repeats collapsed.
//!
//! [`xtce_gs_core::EventLog`] already collapses a line repeated back to back into one line and
//! a count — a link dropping every frame produces thousands of identical lines a second, and
//! without that the one line saying something else scrolls past before it can be read. This
//! panel shows that count rather than hiding it: `CRC failed ×4 812` is a diagnosis, and 4 812
//! identical rows are a scrollbar.
//!
//! # Why the lines are copied
//!
//! The log lives behind a `Mutex` shared with the acquisition and decode threads. Drawing
//! from it directly holds that mutex for a frame, on the path those threads use to report
//! what went wrong — so the lines are cloned out under the lock, and only when
//! [`EventLog::total`] has moved. On an idle station that is one comparison per frame and no
//! copying at all.
//!
//! # Why it follows the newest line, until it does not
//!
//! A log that jumps to the bottom while an operator is reading four lines up is a log they
//! close. A log that does not follow is one they have to scroll to be told anything. So the
//! scroll position *is* the setting: the view sticks to the newest line until the operator
//! scrolls away from it, and sticks again the moment they scroll back. There is no follow
//! switch beside it to disagree with — a checkbox that says "follow" while the view is
//! parked forty lines up is a control with two answers to one question — only a button to
//! go back to the newest line when the view has left it.

use xtce_gs_core::{Event, EventLog, Severity};

use crate::panels::Action;

/// Height of one line, in points. Fixed for the same reason the table's is.
pub const LINE_HEIGHT: f32 = 16.0;

/// Characters the source column is padded to.
///
/// Five, which is `decode`'s width less one and `link`'s plus one: the four sources the
/// station uses are `link`, `frame`, `decode` and `session`, and a column that fits the
/// longest of them wastes the width the message needs.
pub const SOURCE_WIDTH: usize = 6;

/// Points of slack allowed when deciding the view is on the newest line.
///
/// Scroll offsets are floats that come back from a smoothed animation, so an exact
/// comparison against the furthest offset reads as "scrolled up" on the frame after the
/// operator scrolled down. Half a line is far less than a deliberate scroll and far more
/// than the rounding.
pub const BOTTOM_SLACK: f32 = LINE_HEIGHT / 2.0;

/// One line of the log, copied out under the mutex.
#[derive(Clone, Debug)]
pub struct Line {
    /// The line itself, including when it was last seen.
    pub event: Event,
    /// How many times it repeated back to back.
    pub repeats: u32,
}

/// The log panel's state: the lines it drew, and what it is filtering to.
#[derive(Clone, Debug)]
pub struct Events {
    lines: Vec<Line>,
    total: u64,
    min_severity: Severity,
    /// A substring the message must contain, case-insensitively. Empty matches everything.
    ///
    /// It filters what is *drawn* and never what [`Events::refresh`] copies, for the same
    /// reason the severity filter does not: a pass spent searching for one APID must not lose
    /// every line that was hidden while the operator was typing.
    needle: String,
    follow: bool,
}

impl Default for Events {
    /// An empty log showing everything, scrolled to the newest line.
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            total: 0,
            min_severity: Severity::Info,
            needle: String::new(),
            follow: true,
        }
    }
}

impl Events {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The lines as of the last refresh, oldest first.
    #[must_use]
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// The lines the filter lets through, oldest first.
    ///
    /// The one predicate: the count the scroll area is sized from and the lines it draws both
    /// come through here, so a row can never be drawn that the count did not allow for. It is
    /// a lazy filter and not a second `Vec`, because the log is bounded and a comparison per
    /// line is cheaper than keeping two collections agreeing with each other.
    pub fn visible(&self) -> impl Iterator<Item = &Line> {
        let min_severity = self.min_severity;
        let needle = self.needle.as_str();
        self.lines.iter().filter(move |line| {
            line.event.severity >= min_severity
                && (needle.is_empty()
                    || crate::fmt::contains_ignore_ascii_case(&line.event.message, needle)
                    || crate::fmt::contains_ignore_ascii_case(line.event.source, needle))
        })
    }

    /// The substring being searched for.
    #[must_use]
    pub fn needle(&self) -> &str {
        &self.needle
    }

    /// The substring being searched for, to be edited by a text field.
    pub fn needle_mut(&mut self) -> &mut String {
        &mut self.needle
    }

    /// How many lines the log had pushed when these were copied, repeats included.
    ///
    /// The interface's copy of [`EventLog::total`], and what tells the next refresh whether
    /// anything changed.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// The lowest severity being shown.
    #[must_use]
    pub const fn min_severity(&self) -> Severity {
        self.min_severity
    }

    /// Shows only lines at least this severe.
    ///
    /// Filtering happens while drawing rather than while copying: a pass spent at "errors
    /// only" must still have the informational lines to show when the operator widens it,
    /// and re-copying would only have the ones the log still holds.
    pub fn set_min_severity(&mut self, severity: Severity) {
        self.min_severity = severity;
    }

    /// Whether the view sticks to the newest line.
    #[must_use]
    pub const fn follows(&self) -> bool {
        self.follow
    }

    /// Sticks to the newest line, or stays where the operator scrolled.
    pub fn set_follow(&mut self, follow: bool) {
        self.follow = follow;
    }

    /// Copies the log out, when it has changed.
    ///
    /// Runs under the log's mutex and is the only method here that names [`EventLog`]. The
    /// comparison against [`EventLog::total`] is the whole point: on an idle station this is
    /// one integer load per frame and no allocation, and `total` counts repeats, so a line
    /// that is only collapsing still moves it and still gets copied.
    pub fn refresh(&mut self, log: &EventLog) {
        if log.total() == self.total {
            return;
        }
        self.lines.clear();
        self.lines.extend(log.iter().map(|(event, repeats)| Line {
            event: event.clone(),
            repeats,
        }));
        self.total = log.total();
    }

    /// Forgets the copied lines.
    ///
    /// Only this panel's copy: the session's log is cleared by [`crate::App`] applying
    /// [`Action::ClearEvents`], which holds the mutex. Resetting `total` to zero is what makes
    /// the next refresh refill from whatever the log still holds.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.total = 0;
    }
}

/// Draws the event log. Returns what the operator asked for.
pub fn show(ui: &mut egui::Ui, events: &mut Events, scratch: &mut String) -> Action {
    let (action, jump) = ui.horizontal(|ui| toolbar(ui, events, scratch)).inner;

    let rows = events.visible().count();
    if rows == 0 {
        ui.weak("Nothing logged.");
        return action;
    }

    let mut area = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(events.follow);
    if jump {
        // Far enough down to reach the last line whatever the panel's height is; the scroll
        // area clamps it to the end of the content. Sticking alone would not do it — egui
        // stops sticking once the operator has scrolled away, and only an explicit offset
        // brings the view back.
        area = area.vertical_scroll_offset(LINE_HEIGHT * rows as f32);
        events.follow = true;
    }

    let output = area.show_rows(ui, LINE_HEIGHT, rows, |ui, range| {
        let width = ui.available_width();
        for line in events.visible().skip(range.start).take(range.len()) {
            // Exactly one `LINE_HEIGHT` per line, whatever the font measures: `show_rows`
            // decides which lines to build from that height, and a row that drew shorter
            // would leave the ones after it above where it said they would be.
            let _ = ui.allocate_ui_with_layout(
                egui::vec2(width, LINE_HEIGHT),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_height(LINE_HEIGHT);
                    draw_line(ui, line, width, scratch);
                },
            );
        }
    });

    // Read the position back, so that the wheel is what decides. Not on the frame the
    // operator asked to jump: the view has not moved yet and the answer would be stale.
    if !jump {
        let overflow = output.content_size.y - output.inner_rect.height();
        events.follow = overflow <= 0.0 || output.state.offset.y >= overflow - BOTTOM_SLACK;
    }

    action
}

/// The filter, the jump-to-newest button and the clear button.
///
/// Returns what was asked for, and whether the operator asked to go back to the newest line.
fn toolbar(ui: &mut egui::Ui, events: &mut Events, scratch: &mut String) -> (Action, bool) {
    let mut action = Action::None;

    ui.label("Show");
    for (severity, name) in [
        (Severity::Info, "all"),
        (Severity::Warning, "warnings"),
        (Severity::Error, "errors"),
    ] {
        if ui
            .selectable_label(events.min_severity == severity, name)
            .on_hover_text("Lines below this are kept, not dropped — widening shows them again")
            .clicked()
        {
            events.min_severity = severity;
        }
    }

    ui.separator();

    // Beside the severity buttons, because the two are one filter from the operator's side:
    // "errors, about this APID" is the question a bad pass ends in.
    ui.add(
        egui::TextEdit::singleline(&mut events.needle)
            .desired_width(140.0)
            .hint_text("find"),
    )
    .on_hover_text(
        "Substring of the message or the source, case-insensitive. Hidden lines are kept.",
    );

    ui.separator();

    let jump = if events.follow {
        ui.weak("following").on_hover_text(
            "The newest line is on screen. Scroll up and it stays where you left it.",
        );
        false
    } else {
        ui.button("Newest")
            .on_hover_text("Back to the newest line, and follow it again")
            .clicked()
    };

    if ui.button("Clear").clicked() {
        action = Action::ClearEvents;
    }

    scratch.clear();
    let shown = events.visible().count();
    crate::fmt::count(scratch, shown as u64);
    scratch.push_str(" of ");
    crate::fmt::count(scratch, events.lines.len() as u64);
    ui.weak(scratch.as_str())
        .on_hover_text("Lines the filter shows, of lines the log still holds");

    (action, jump)
}

/// One line: time, severity letter, source, message, repeat count.
///
/// Drawn as a single label over a [`egui::text::LayoutJob`] rather than as four labels in a
/// row, for three reasons: the columns stay aligned because one galley is laid out once, the
/// whole line is one selection, and the copy is one line rather than whichever of the four
/// pieces the pointer was over.
fn draw_line(ui: &mut egui::Ui, line: &Line, width: f32, scratch: &mut String) {
    let job = layout_line(ui, line, width, scratch);
    let response = ui.add(
        egui::Label::new(job)
            .selectable(true)
            .halign(egui::Align::LEFT),
    );
    let _ = response.context_menu(|ui| {
        if ui.button("Copy line").clicked() {
            let mut text = String::new();
            compose(&mut text, line);
            ui.ctx().copy_text(text);
            ui.close();
        }
    });
}

/// The line as one galley, coloured by severity.
fn layout_line(
    ui: &egui::Ui,
    line: &Line,
    width: f32,
    scratch: &mut String,
) -> egui::text::LayoutJob {
    let visuals = ui.visuals();
    let font = monospace(ui);
    let mut job = egui::text::LayoutJob {
        break_on_newline: false,
        ..egui::text::LayoutJob::default()
    };
    // One row, elided at the panel's edge. `Label` shows the full text on hover when it
    // elided, which is the tooltip an operator wants on a long decode error anyway.
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;

    scratch.clear();
    crate::fmt::clock(scratch, line.event.time);
    scratch.push(' ');
    job.append(
        scratch.as_str(),
        0.0,
        egui::TextFormat::simple(font.clone(), visuals.weak_text_color()),
    );

    scratch.clear();
    scratch.push(line.event.severity.letter());
    scratch.push(' ');
    job.append(
        scratch.as_str(),
        0.0,
        egui::TextFormat::simple(
            font.clone(),
            crate::fmt::severity_color(line.event.severity, visuals),
        ),
    );

    scratch.clear();
    pad(scratch, line.event.source, SOURCE_WIDTH);
    scratch.push(' ');
    job.append(
        scratch.as_str(),
        0.0,
        egui::TextFormat::simple(font.clone(), visuals.weak_text_color()),
    );

    scratch.clear();
    message(scratch, line);
    job.append(
        scratch.as_str(),
        0.0,
        egui::TextFormat::simple(font, visuals.text_color()),
    );

    job
}

/// The whole line as plain text, for the clipboard.
///
/// The full ISO-8601 instant and not the time of day: a line pasted into a ticket is read by
/// someone who does not know which day the pass was.
fn compose(out: &mut String, line: &Line) {
    use std::fmt::Write as _;
    let _ = write!(out, "{} ", line.event.time);
    out.push(line.event.severity.letter());
    out.push(' ');
    pad(out, line.event.source, SOURCE_WIDTH);
    out.push(' ');
    message(out, line);
}

/// The message, with the repeat count when there is one.
fn message(out: &mut String, line: &Line) {
    use std::fmt::Write as _;
    out.push_str(&line.event.message);
    if line.repeats > 1 {
        let _ = write!(out, " ×{}", line.repeats);
    }
}

/// Appends `text`, padded with spaces to `width` characters.
///
/// Characters and not bytes: the sources are ASCII today, and a column that moved because a
/// message arrived in another script would be a column nobody could scan down.
fn pad(out: &mut String, text: &str, width: usize) {
    out.push_str(text);
    for _ in text.chars().count()..width {
        out.push(' ');
    }
}

/// The monospace font of the theme in use.
///
/// The log is read as a column of times and sources, and a proportional font makes that
/// column ragged. A style with no monospace entry falls back rather than failing: this is a
/// display, and there is nothing to report to.
fn monospace(ui: &egui::Ui) -> egui::FontId {
    ui.style()
        .text_styles
        .get(&egui::TextStyle::Monospace)
        .cloned()
        .unwrap_or_else(|| egui::FontId::monospace(LINE_HEIGHT - 4.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xtce_gs_core::Utc;

    fn line(severity: Severity, message: &str) -> Event {
        Event::at(Utc::now(), severity, "link", message)
    }

    #[test]
    fn an_unchanged_log_is_not_copied_again() {
        let mut log = EventLog::with_capacity(8);
        log.push(line(Severity::Info, "locked"));
        let mut events = Events::new();
        events.refresh(&log);
        assert_eq!(events.lines().len(), 1);

        // Empty the copy behind the panel's back. A refresh that did any work would refill
        // it; one that compared totals first leaves it alone.
        events.lines.clear();
        events.refresh(&log);
        assert!(events.lines().is_empty());

        log.push(line(Severity::Error, "desynchronised"));
        events.refresh(&log);
        assert_eq!(events.lines().len(), 2);
    }

    #[test]
    fn a_repeated_line_is_one_line_with_a_count() {
        let mut log = EventLog::with_capacity(8);
        for _ in 0..3 {
            log.push(line(Severity::Warning, "CRC failed"));
        }
        let mut events = Events::new();
        events.refresh(&log);
        assert_eq!(events.lines().len(), 1);
        assert_eq!(events.lines()[0].repeats, 3);

        // A line that is only collapsing still moves `total`, so the count on screen keeps up.
        log.push(line(Severity::Warning, "CRC failed"));
        events.refresh(&log);
        assert_eq!(events.lines()[0].repeats, 4);
    }

    #[test]
    fn the_filter_hides_everything_below_the_minimum() {
        let mut log = EventLog::with_capacity(8);
        log.push(line(Severity::Info, "locked"));
        log.push(line(Severity::Warning, "CRC failed"));
        log.push(line(Severity::Error, "desynchronised"));
        let mut events = Events::new();
        events.refresh(&log);

        assert_eq!(events.visible().count(), 3);
        events.set_min_severity(Severity::Warning);
        assert_eq!(events.visible().count(), 2);
        events.set_min_severity(Severity::Error);
        let shown: Vec<&str> = events
            .visible()
            .map(|shown| shown.event.message.as_str())
            .collect();
        assert_eq!(shown, ["desynchronised"]);
    }

    #[test]
    fn widening_the_filter_shows_the_lines_again() {
        let mut log = EventLog::with_capacity(8);
        log.push(line(Severity::Info, "locked"));
        log.push(line(Severity::Error, "desynchronised"));
        let mut events = Events::new();
        events.refresh(&log);

        events.set_min_severity(Severity::Error);
        assert_eq!(events.visible().count(), 1);
        // Nothing was copied again, and the informational line was never thrown away.
        events.set_min_severity(Severity::Info);
        assert_eq!(events.visible().count(), 2);
        assert_eq!(events.lines().len(), 2);
    }

    #[test]
    fn an_empty_log_shows_nothing_and_does_not_reach_for_a_line() {
        let events = Events::new();
        assert_eq!(events.visible().count(), 0);
        assert_eq!(events.total(), 0);
        assert!(events.follows());
    }

    #[test]
    fn clearing_the_copy_makes_the_next_refresh_refill_it() {
        let mut log = EventLog::with_capacity(8);
        log.push(line(Severity::Info, "locked"));
        let mut events = Events::new();
        events.refresh(&log);
        events.clear();
        assert_eq!(events.total(), 0);
        // The session's log still holds the line, and the panel picks it up again.
        events.refresh(&log);
        assert_eq!(events.lines().len(), 1);
    }

    #[test]
    fn a_bounded_log_that_dropped_its_oldest_lines_is_still_copied_whole() {
        let mut log = EventLog::with_capacity(2);
        for index in 0..5 {
            log.push(line(Severity::Info, &format!("line {index}")));
        }
        let mut events = Events::new();
        events.refresh(&log);
        assert_eq!(events.lines().len(), 2);
        assert_eq!(
            events.total(),
            5,
            "the count of what was said, not of what is kept"
        );
        assert_eq!(events.lines()[1].event.message, "line 4");
    }

    #[test]
    fn a_line_composes_time_severity_source_and_count() {
        let mut out = String::new();
        compose(
            &mut out,
            &Line {
                event: Event::at(Utc::EPOCH, Severity::Warning, "frame", "CRC failed"),
                repeats: 4_812,
            },
        );
        assert_eq!(out, "1970-01-01T00:00:00.000Z ! frame  CRC failed ×4812");
    }

    #[test]
    fn a_line_that_happened_once_says_nothing_about_repeats() {
        let mut out = String::new();
        message(
            &mut out,
            &Line {
                event: Event::at(Utc::EPOCH, Severity::Info, "link", "locked"),
                repeats: 1,
            },
        );
        assert_eq!(out, "locked");
    }

    #[test]
    fn a_source_longer_than_the_column_is_not_truncated() {
        let mut out = String::new();
        pad(&mut out, "session", SOURCE_WIDTH);
        assert_eq!(
            out, "session",
            "a ragged column beats a source nobody can identify"
        );
        out.clear();
        pad(&mut out, "link", SOURCE_WIDTH);
        assert_eq!(out, "link  ");
    }

    #[test]
    fn a_log_draws_and_drawing_changes_nothing_but_where_it_is_scrolled() {
        let mut log = EventLog::with_capacity(8);
        log.push(line(Severity::Info, "locked"));
        for _ in 0..3 {
            log.push(line(Severity::Warning, "CRC failed"));
        }
        log.push(line(Severity::Error, "desynchronised"));
        let mut events = Events::new();
        events.refresh(&log);

        let mut scratch = String::new();
        egui::__run_test_ui(|ui| {
            let action = show(ui, &mut events, &mut scratch);
            assert!(action.is_none(), "nothing was clicked");
        });
        assert_eq!(events.lines().len(), 3);
        assert_eq!(events.lines()[1].repeats, 3);
        assert!(
            events.follows(),
            "the first frame of a full log is at the newest line"
        );
    }

    #[test]
    fn an_empty_log_draws_without_taking_the_window_down() {
        // The first frame of every session, and the frame after the operator clears the log.
        let mut events = Events::new();
        let mut scratch = String::new();
        egui::__run_test_ui(|ui| {
            assert!(show(ui, &mut events, &mut scratch).is_none());
        });
    }

    #[test]
    fn the_search_filters_what_is_drawn_and_never_what_is_kept() {
        let mut log = EventLog::with_capacity(16);
        log.push(Event::warning("frame", "CRC failed"));
        log.push(Event::info("link", "APID 11 is quiet"));
        log.push(Event::error("decode", "APID 11 refused"));

        let mut events = Events::new();
        events.refresh(&log);
        assert_eq!(events.visible().count(), 3);

        events.needle_mut().push_str("apid 11");
        assert_eq!(events.visible().count(), 2, "case-insensitive substring");
        assert_eq!(
            events.lines().len(),
            3,
            "a hidden line must still be held: widening the search has to show it again"
        );

        // The two filters compose, and both are on the drawing side.
        events.set_min_severity(Severity::Error);
        assert_eq!(events.visible().count(), 1);
        events.needle_mut().clear();
        assert_eq!(events.visible().count(), 1);
        events.set_min_severity(Severity::Info);
        assert_eq!(events.visible().count(), 3);

        // The source matches too: "which part of the station said this" is the other half of
        // the question an operator asks a log.
        events.needle_mut().push_str("frame");
        assert_eq!(events.visible().count(), 1);
    }

    #[test]
    fn a_filter_that_hides_every_line_draws_the_empty_state_and_not_an_empty_scroll_area() {
        let mut log = EventLog::with_capacity(8);
        log.push(line(Severity::Info, "locked"));
        let mut events = Events::new();
        events.refresh(&log);
        events.set_min_severity(Severity::Error);
        assert_eq!(events.visible().count(), 0);

        let mut scratch = String::new();
        egui::__run_test_ui(|ui| {
            assert!(show(ui, &mut events, &mut scratch).is_none());
        });
        assert_eq!(events.lines().len(), 1, "the hidden line is still held");
    }
}
