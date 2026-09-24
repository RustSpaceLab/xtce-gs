//! The link bar: what is arriving, and what is being lost on the way.
//!
//! One row across the top, and it is the first thing an operator looks at when a plot stops
//! moving. The question it answers is always the same one — *is the problem the spacecraft,
//! the link, or the definition* — so the counters are laid out in that order: bytes, frames,
//! packets, parameters. A station that showed only "packets decoded" makes a desynchronised
//! receiver and a mistyped root container look identical.
//!
//! # Why there are rates and not only totals
//!
//! A total says what the pass has done; it says nothing about *now*. `bytes_in` climbing to
//! 4.2 GiB looks identical whether the last byte arrived a millisecond ago or four minutes
//! ago, and an operator watching a number that only ever grows cannot tell a healthy link
//! from one that stopped. So this panel keeps its own short window of samples — see
//! [`RateWindow`] — and divides. The window is the only state in the interface that cannot be
//! recovered from the session, which is why it lives in [`Status`] and is fed from the one
//! place that already takes a consistent copy of the counters.

use xtce_gs_core::{LinkStats, StatsSnapshot, Utc};

use crate::layout::{Layout, Theme};
use crate::panels::Action;

/// Seconds without a packet after which the live indicator goes out.
///
/// Three, not one: a mission that sends a housekeeping packet every second would blink at
/// exactly the wrong rate, and an indicator an operator learns to ignore is worse than none.
pub const LIVE_WINDOW_SECONDS: f64 = 3.0;

/// Seconds without a packet after which the link is called stopped rather than quiet.
///
/// Thirty. Between [`LIVE_WINDOW_SECONDS`] and this the indicator is amber, which is the
/// honest colour for a link that is idle between two housekeeping cycles; past it, something
/// an operator can act on has happened — the pass ended, the receiver lost lock, the sender
/// was stopped — and amber that never goes red is amber nobody looks at.
pub const QUIET_WINDOW_SECONDS: f64 = 30.0;

/// Frame loss above which the figure is drawn as a warning.
///
/// One percent. Below it a CCSDS link with Reed-Solomon is doing its job; above it the
/// antenna, the modem or the bit rate is wrong, and no amount of decoding will help.
pub const FRAME_LOSS_WARNING: f64 = 0.01;

/// Seconds of history the rates are averaged over.
///
/// Five. Short enough that a link that stopped shows zero while the operator is still
/// looking at it, long enough that a source sending one packet a second does not read as
/// zero every other frame.
pub const RATE_WINDOW_SECONDS: f64 = 5.0;

/// Minimum seconds between two samples of the rate window.
///
/// [`Status::refresh`] runs once per drawn frame, which is sixty times a second on a loaded
/// station and once a second on an idle one. Sampling on that cadence would make the window
/// span half a second on one machine and half a minute on another — the averaging period
/// would be a property of the monitor rather than of this module. Gating on elapsed time
/// instead fixes the span at [`RATE_WINDOW_SECONDS`] wherever it runs.
pub const RATE_SAMPLE_SECONDS: f64 = 0.5;

/// Samples the rate window holds.
///
/// Enough to cover [`RATE_WINDOW_SECONDS`] at [`RATE_SAMPLE_SECONDS`] with room to spare. The
/// window is a fixed array rather than a `Vec` so that [`Status`] stays `Copy` and nothing on
/// the drawing path allocates.
pub const RATE_SAMPLES: usize = 16;

/// The colour of a live link.
///
/// A fixed green and not one out of [`egui::Visuals`], which has a warning colour and an
/// error colour and no "all is well" colour. This one is dark enough to read on the light
/// theme's background and bright enough to read on the dark one's.
pub const LIVE_COLOR: egui::Color32 = egui::Color32::from_rgb(0x2e, 0xa0, 0x43);

/// Diameter of the live indicator, in points.
pub const DOT_DIAMETER: f32 = 9.0;

/// History depths the settings menu offers.
///
/// Powers of four rather than a text field: the number an operator wants is "more" or "less",
/// and a field that accepts 4097 invites a typed 40970 and a station that swaps.
pub const HISTORY_DEPTH_CHOICES: [usize; 4] = [1_024, 4_096, 16_384, 65_536];

/// What the indicator is saying.
///
/// Three states and not a boolean, because "no packet for four seconds" and "no packet for
/// four minutes" are different situations and a single lamp that covers both is a lamp that
/// gets taped over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Liveness {
    /// A packet arrived within [`LIVE_WINDOW_SECONDS`].
    Live,
    /// Nothing recently, but not long enough to call it over.
    Quiet,
    /// Nothing for [`QUIET_WINDOW_SECONDS`], or the source is no longer being read.
    Stopped,
}

impl Liveness {
    /// The colour the dot is drawn in, for the theme in use.
    #[must_use]
    pub fn color(self, visuals: &egui::Visuals) -> egui::Color32 {
        match self {
            Self::Live => LIVE_COLOR,
            Self::Quiet => visuals.warn_fg_color,
            Self::Stopped => visuals.error_fg_color,
        }
    }

    /// What the dot means, for the tooltip.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Live => "Receiving: a packet arrived in the last few seconds",
            Self::Quiet => "Quiet: nothing has arrived recently, the source is still open",
            Self::Stopped => "Stopped: the source ended, or nothing has arrived for a while",
        }
    }
}

/// Bytes and packets per second over the last [`RATE_WINDOW_SECONDS`].
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rates {
    /// Bytes read from the source, per second.
    pub bytes_per_second: f64,
    /// Packets handed to the decoder, per second.
    pub packets_per_second: f64,
}

/// One sample of the counters the rates are computed from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Tick {
    at: Utc,
    bytes: u64,
    packets: u64,
}

impl Tick {
    const ZERO: Self = Self {
        at: Utc::EPOCH,
        bytes: 0,
        packets: 0,
    };
}

/// A short sliding window of counter samples, oldest first.
///
/// A fixed array used as a queue: [`RATE_SAMPLES`] is small, a sample is three words, and
/// shifting sixteen of them twice a second costs less than the index arithmetic a ring would
/// need — and cannot get the arithmetic wrong.
#[derive(Clone, Copy, Debug)]
pub struct RateWindow {
    samples: [Tick; RATE_SAMPLES],
    len: usize,
}

impl Default for RateWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl RateWindow {
    /// An empty window.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            samples: [Tick::ZERO; RATE_SAMPLES],
            len: 0,
        }
    }

    /// How many samples are held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been sampled yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Forgets every sample.
    pub const fn clear(&mut self) {
        self.len = 0;
    }

    /// Seconds between the oldest and the newest sample held.
    #[must_use]
    pub fn span_seconds(&self) -> f64 {
        match (self.samples.first(), self.newest()) {
            (Some(oldest), Some(newest)) if self.len >= 2 => newest.at.secs_since(oldest.at),
            _ => 0.0,
        }
    }

    /// Offers a sample of the cumulative counters.
    ///
    /// Ignored when it comes less than [`RATE_SAMPLE_SECONDS`] after the last one, so the
    /// window spans the same wall-clock period whatever the frame rate is. A timestamp before
    /// the newest sample — a clock stepped backwards by NTP mid-pass — empties the window
    /// rather than producing a negative interval.
    pub fn push(&mut self, at: Utc, bytes: u64, packets: u64) {
        if let Some(newest) = self.newest() {
            let since = at.secs_since(newest.at);
            if since < 0.0 {
                self.len = 0;
            } else if since < RATE_SAMPLE_SECONDS {
                return;
            }
        }
        if self.len == RATE_SAMPLES {
            self.samples.copy_within(1..RATE_SAMPLES, 0);
            self.len -= 1;
        }
        if let Some(slot) = self.samples.get_mut(self.len) {
            *slot = Tick { at, bytes, packets };
            self.len += 1;
        }
        self.prune(at);
    }

    /// The rates over the samples held.
    ///
    /// Zero until there are two samples: one sample is a total, and a total divided by no
    /// interval is not a rate. Counters that went backwards — which they do not, but a
    /// display that divides must be total anyway — also read as zero rather than as a
    /// negative rate.
    #[must_use]
    pub fn rates(&self) -> Rates {
        if self.len < 2 {
            return Rates::default();
        }
        let (Some(oldest), Some(newest)) = (self.samples.first(), self.newest()) else {
            return Rates::default();
        };
        let interval = newest.at.secs_since(oldest.at);
        if interval <= 0.0 {
            return Rates::default();
        }
        Rates {
            bytes_per_second: newest.bytes.saturating_sub(oldest.bytes) as f64 / interval,
            packets_per_second: newest.packets.saturating_sub(oldest.packets) as f64 / interval,
        }
    }

    /// The most recent sample.
    fn newest(&self) -> Option<Tick> {
        self.len
            .checked_sub(1)
            .and_then(|last| self.samples.get(last))
            .copied()
    }

    /// Drops samples older than [`RATE_WINDOW_SECONDS`], never the last one.
    ///
    /// Keeping the newest is what makes a window that stalled — the station was minimised for
    /// a minute — report zero until it has two fresh samples, rather than averaging a minute
    /// of arrivals and calling it "now".
    fn prune(&mut self, now: Utc) {
        let mut drop = 0;
        while drop + 1 < self.len
            && self
                .samples
                .get(drop)
                .is_some_and(|sample| now.secs_since(sample.at) > RATE_WINDOW_SECONDS)
        {
            drop += 1;
        }
        if drop > 0 {
            self.samples.copy_within(drop..self.len, 0);
            self.len -= drop;
        }
    }
}

/// The counters as of the last refresh, and when that was.
///
/// A copy and not a borrow of [`LinkStats`]: the counters are eighteen atomics the link
/// writes several times per frame, and reading them one at a time *while drawing* would show
/// a row whose numbers come from eighteen different instants.
#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    snapshot: StatsSnapshot,
    taken: Utc,
    running: bool,
    rates: RateWindow,
    opened: Option<Utc>,
}

impl Status {
    /// A bar showing nothing has arrived.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one consistent copy of the counters.
    ///
    /// Called once per frame, outside every lock — [`LinkStats`] is atomics, so there is no
    /// guard to hold and nothing to block. It must be called on *every* frame and not only on
    /// the frames where the store's generation moved: the rate window and the age of the last
    /// packet are the two things on this bar that change when nothing arrives, and they are
    /// exactly the two an operator is watching when nothing arrives.
    pub fn refresh(&mut self, stats: &LinkStats, running: bool) {
        self.snapshot = stats.snapshot();
        self.taken = Utc::now();
        self.running = running;
        self.opened.get_or_insert(self.taken);
        self.rates
            .push(self.taken, self.snapshot.bytes_in, self.snapshot.packets_in);
    }

    /// The counters this bar is drawing.
    #[must_use]
    pub const fn snapshot(&self) -> &StatsSnapshot {
        &self.snapshot
    }

    /// When they were copied.
    ///
    /// The frame's idea of *now*: every age and every threshold on this bar is measured
    /// against it rather than against a fresh `Utc::now()`, so that the row is one instant
    /// and not a dozen.
    #[must_use]
    pub const fn taken(&self) -> Utc {
        self.taken
    }

    /// Whether the acquisition task is still reading.
    ///
    /// False for a file replay that ran to the end, which is not a failure — the session is
    /// over and everything it decoded is still on screen.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// Whether a packet arrived within [`LIVE_WINDOW_SECONDS`].
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.snapshot.is_live(self.taken, LIVE_WINDOW_SECONDS)
    }

    /// Seconds since the last packet, or since the bar was first refreshed when none has
    /// arrived at all.
    ///
    /// `None` before the first refresh, when the bar knows neither a packet nor a now.
    #[must_use]
    pub fn silence_seconds(&self) -> Option<f64> {
        let opened = self.opened?;
        Some(self.snapshot.last_packet.map_or_else(
            || self.taken.secs_since(opened),
            |last| self.taken.secs_since(last),
        ))
    }

    /// What the indicator says.
    ///
    /// A source that is no longer being read is [`Liveness::Stopped`] whatever the counters
    /// say, and so is a bar that has never been refreshed — a dot drawn from a default
    /// [`Utc`] would otherwise report the link as silent since 1970. A link that has never
    /// delivered a packet is never [`Liveness::Live`], however recently the window opened:
    /// green is what says telemetry is arriving, and a green dot over a station that has
    /// received nothing is the one lie this bar must not tell.
    #[must_use]
    pub fn liveness(&self) -> Liveness {
        let (Some(opened), true) = (self.opened, self.running) else {
            return Liveness::Stopped;
        };
        let Some(last) = self.snapshot.last_packet else {
            return if self.taken.secs_since(opened) <= QUIET_WINDOW_SECONDS {
                Liveness::Quiet
            } else {
                Liveness::Stopped
            };
        };
        let since = self.taken.secs_since(last);
        if since <= LIVE_WINDOW_SECONDS {
            Liveness::Live
        } else if since <= QUIET_WINDOW_SECONDS {
            Liveness::Quiet
        } else {
            Liveness::Stopped
        }
    }

    /// Bytes and packets per second over the last [`RATE_WINDOW_SECONDS`].
    #[must_use]
    pub fn rates(&self) -> Rates {
        self.rates.rates()
    }

    /// Symbols Reed-Solomon repaired per frame it repaired, over the whole pass.
    ///
    /// Zero when no frame has been repaired, which is not a depth of zero but the absence of
    /// one — `write_reed_solomon` draws nothing in that case rather than `0.0`.
    #[must_use]
    pub fn correction_depth(&self) -> f64 {
        correction_depth(&self.snapshot)
    }

    /// The samples the rates are computed from.
    #[must_use]
    pub const fn rate_window(&self) -> &RateWindow {
        &self.rates
    }
}

// Every counter `LinkStats` keeps is now reachable from this bar. Five of them have no cell:
// `rs_symbols` is folded into the Reed-Solomon cell as symbols-per-corrected-frame, and
// `sync_found`, `sync_lost`, `idle_frames` and `idle_packets` are in the hover text of the
// cells whose numbers raise the question they answer. That was the third of the three options
// the row had — a second row, a collapsing section, or no width at all — and it is the only
// one that costs the plots nothing.
/// Draws the link bar. Returns what the operator asked for.
///
/// `source` is [`xtce_gs_engine::Session::describe_source`] — what was actually opened, which
/// is not always what was configured — and `framed` says whether the link carries transfer
/// frames, which is what decides whether the coding counters mean anything. `layout` is read
/// for the settings menu's current values and never written: see [`crate::panels`].
pub fn show(
    ui: &mut egui::Ui,
    status: &Status,
    source: &str,
    framed: bool,
    layout: &Layout,
    paused: bool,
    scratch: &mut String,
) -> Action {
    ui.horizontal(|ui| {
        indicator(ui, status);
        ui.label(source)
            .on_hover_text("The source that was opened, which is not always the one configured");
        ui.separator();
        arrival(ui, status, scratch);
        ui.separator();
        if framed {
            frames(ui, status, scratch);
            ui.separator();
            coding(ui, status, scratch);
            ui.separator();
        }
        packets(ui, status, scratch);

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            controls(ui, layout, paused)
        })
        .inner
    })
    .inner
}

/// The dot, coloured by [`Status::liveness`].
fn indicator(ui: &mut egui::Ui, status: &Status) {
    let liveness = status.liveness();
    let color = liveness.color(ui.visuals());
    let (rect, response) =
        ui.allocate_exact_size(egui::Vec2::splat(DOT_DIAMETER), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), DOT_DIAMETER / 2.0, color);
    let _ = response.on_hover_text(liveness.describe());
}

/// How much is arriving, and how long ago the last of it did.
fn arrival(ui: &mut egui::Ui, status: &Status, scratch: &mut String) {
    let rates = status.rates();

    scratch.clear();
    crate::fmt::bytes(scratch, rates.bytes_per_second as u64);
    scratch.push_str("/s");
    ui.label(scratch.as_str()).on_hover_text(
        "Bytes per second off the source, averaged over the last few seconds — not a total",
    );

    scratch.clear();
    crate::fmt::bytes(scratch, status.snapshot().bytes_in);
    ui.weak(scratch.as_str())
        .on_hover_text("Bytes read from the source since the session started");

    scratch.clear();
    write_rate(scratch, rates.packets_per_second);
    scratch.push_str(" pkt/s");
    ui.label(scratch.as_str())
        .on_hover_text("Packets per second, averaged over the last few seconds");

    scratch.clear();
    match status.snapshot().last_packet {
        Some(last) => {
            crate::fmt::age(scratch, status.taken(), last);
            scratch.push_str(" ago");
        }
        None => scratch.push_str("no packet yet"),
    }
    ui.weak(scratch.as_str())
        .on_hover_text("Time since the last packet reached the decoder");
}

/// Frames seen, frames kept, and the fraction lost.
fn frames(ui: &mut egui::Ui, status: &Status, scratch: &mut String) {
    use std::fmt::Write as _;

    let snapshot = status.snapshot();
    scratch.clear();
    scratch.push_str("frames ");
    crate::fmt::count(scratch, snapshot.frames_ok);
    scratch.push('/');
    crate::fmt::count(scratch, snapshot.frames_seen);
    // The four counters that have no cell of their own live here, where they cost the row no
    // width. A hover is the right home for them: `sync_lost` moves when `frames_seen` stops,
    // and idle frames are padding rather than loss, so each answers a question only after one
    // of the visible numbers has raised it.
    let detail = {
        let mut detail = String::from(
            "Transfer frames that passed every check, over frames the synchroniser offered\n\n",
        );
        let _ = write!(
            detail,
            "sync markers found  {}\nlock lost           {}\nidle frames         {}",
            snapshot.sync_found, snapshot.sync_lost, snapshot.idle_frames
        );
        detail
    };
    ui.label(scratch.as_str()).on_hover_text(detail);

    let loss = snapshot.frame_loss();
    scratch.clear();
    scratch.push_str("loss ");
    crate::fmt::percent(scratch, loss);
    let text = egui::RichText::new(scratch.as_str());
    let text = if loss > FRAME_LOSS_WARNING {
        text.color(ui.visuals().warn_fg_color)
    } else {
        text
    };
    ui.label(text)
        .on_hover_text("Frames dropped as a fraction of frames seen; zero when none has been seen");
}

/// What the coding layer had to repair, and what it could not.
fn coding(ui: &mut egui::Ui, status: &Status, scratch: &mut String) {
    let snapshot = status.snapshot();

    scratch.clear();
    write_reed_solomon(scratch, snapshot);
    ui.label(scratch.as_str()).on_hover_text(
        "Frames Reed-Solomon repaired, over frames it could not — an uncorrectable frame is lost, \
         not guessed at. In brackets, symbols repaired per repaired frame: the closer that runs \
         to the code's capacity, the less margin is left before a repair is wrong rather than \
         refused.",
    );

    scratch.clear();
    scratch.push_str("crc ");
    crate::fmt::count(scratch, snapshot.crc_failures);
    ui.label(scratch.as_str())
        .on_hover_text("Frames whose trailing checksum did not match");
}

/// What reached the decoder and what came out of it.
fn packets(ui: &mut egui::Ui, status: &Status, scratch: &mut String) {
    use std::fmt::Write as _;

    let snapshot = status.snapshot();

    scratch.clear();
    scratch.push_str("pkt ");
    crate::fmt::count(scratch, snapshot.packets_in);
    let detail = {
        let mut detail =
            String::from("Packets pulled out of the frames, or read straight off the source\n\n");
        let _ = write!(detail, "idle packets discarded  {}", snapshot.idle_packets);
        detail
    };
    ui.label(scratch.as_str()).on_hover_text(detail);

    scratch.clear();
    scratch.push_str("dec ");
    crate::fmt::count(scratch, snapshot.packets_decoded);
    ui.label(scratch.as_str())
        .on_hover_text("Packets decoded against the definition");

    scratch.clear();
    scratch.push_str("rej ");
    crate::fmt::count(scratch, snapshot.packets_rejected);
    let text = egui::RichText::new(scratch.as_str());
    let text = if snapshot.packets_rejected > 0 {
        text.color(ui.visuals().warn_fg_color)
    } else {
        text
    };
    ui.label(text)
        .on_hover_text("Packets the definition refused or does not describe — a definition problem, not a link one");

    scratch.clear();
    scratch.push_str("lost ");
    crate::fmt::count(scratch, snapshot.packets_lost);
    ui.label(scratch.as_str())
        .on_hover_text("Partial packets abandoned at a frame gap");

    scratch.clear();
    scratch.push_str("gaps ");
    crate::fmt::count(scratch, snapshot.sequence_gaps);
    scratch.push_str(" / ");
    crate::fmt::count(scratch, snapshot.sequence_missing);
    ui.label(scratch.as_str()).on_hover_text(
        "Times an APID's sequence count broke continuity, and packets those breaks imply never \
         arrived",
    );
}

/// The pause toggle, the settings menu and the quit button, right to left.
fn controls(ui: &mut egui::Ui, layout: &Layout, paused: bool) -> Action {
    let mut action = Action::None;
    if ui.button("Quit").clicked() {
        action = Action::Quit;
    }
    action = action.or(settings_menu(ui, layout));
    let mut is_paused = paused;
    if ui
        .toggle_value(&mut is_paused, "Pause")
        .on_hover_text("Freeze the plots' x axis at what is on screen. The session keeps decoding.")
        .clicked()
    {
        action = action.or(Action::SetPaused(is_paused));
    }
    action
}

/// The theme and the history depth, behind one button.
///
/// On the status bar because it is the one panel that is always visible: a setting behind a
/// plot the operator closed is a setting they cannot get back to.
fn settings_menu(ui: &mut egui::Ui, layout: &Layout) -> Action {
    let mut action = Action::None;
    ui.menu_button("Settings", |ui| {
        ui.label("Theme");
        for (theme, name) in [
            (Theme::System, "System"),
            (Theme::Dark, "Dark"),
            (Theme::Light, "Light"),
        ] {
            if ui.selectable_label(layout.theme == theme, name).clicked() {
                action = Action::SetTheme(theme);
            }
        }
        ui.separator();
        ui.label("History per parameter");
        for depth in HISTORY_DEPTH_CHOICES {
            let mut text = String::new();
            crate::fmt::count(&mut text, depth as u64);
            if ui
                .selectable_label(layout.history_depth == depth, text)
                .clicked()
            {
                action = Action::SetHistoryDepth(depth);
            }
        }
    });
    action
}

/// Appends the Reed-Solomon cell: frames repaired, frames lost, and how deep the repairs went.
///
/// The depth is `rs_symbols / rs_corrected` — symbols repaired per frame that needed
/// repairing — and it is the number `xtce_gs_link::rs` argues an operator has to be able to
/// watch: a decoder beyond its capacity does not fail, it lands on a different valid codeword
/// and hands back a frame that passes every check. The frame counts beside it say a repair
/// happened; only the depth says how much margin was left.
///
/// It is appended to the same cell rather than given one of its own, and only while something
/// has actually been repaired, so a clean pass leaves the row exactly the width it has today —
/// which is what `gs-gui-status-row2` was blocked on.
///
/// Not coloured against a threshold. The capacity is `E` symbols per codeword times the
/// interleaving depth (CCSDS 131.0-B-5 §4.4.1), and neither number reaches this panel:
/// [`xtce_gs_core::LinkStats`] carries counters and not the coding configuration, so a
/// threshold here would be a guess at the code in use.
fn write_reed_solomon(out: &mut String, snapshot: &StatsSnapshot) {
    use std::fmt::Write as _;
    out.push_str("rs ");
    crate::fmt::count(out, snapshot.rs_corrected);
    out.push('/');
    crate::fmt::count(out, snapshot.rs_uncorrectable);
    if snapshot.rs_corrected > 0 {
        let _ = write!(out, " ({:.1} sym/frame)", correction_depth(snapshot));
    }
}

/// Symbols repaired per frame repaired, or zero when nothing has been repaired.
///
/// One formula, called by both [`Status::correction_depth`] and the cell that draws it: a
/// second copy of it inline in `write_reed_solomon` would be the half an operator reads and
/// the accessor's copy the half a test covers.
fn correction_depth(snapshot: &StatsSnapshot) -> f64 {
    if snapshot.rs_corrected == 0 {
        return 0.0;
    }
    snapshot.rs_symbols as f64 / snapshot.rs_corrected as f64
}

/// Appends a rate with one decimal below ten and none above.
///
/// Ten packets a second is `10`, and one every three seconds is `0.3`: the decimal matters
/// exactly where the integer part has run out of resolution.
fn write_rate(out: &mut String, rate: f64) {
    use std::fmt::Write as _;
    if rate < 10.0 {
        let _ = write!(out, "{rate:.1}");
    } else {
        let _ = write!(out, "{:.0}", rate.round());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instant, in whole milliseconds since the epoch.
    fn at(millis: i64) -> Utc {
        Utc::from_unix_nanos(millis * 1_000_000)
    }

    /// A bar whose last packet was at `last`, refreshed at `now`.
    fn bar(opened: Utc, last: Option<Utc>, now: Utc, running: bool) -> Status {
        Status {
            snapshot: StatsSnapshot {
                last_packet: last,
                ..StatsSnapshot::default()
            },
            taken: now,
            running,
            rates: RateWindow::new(),
            opened: Some(opened),
        }
    }

    #[test]
    fn the_first_sample_is_a_total_and_not_a_rate() {
        let mut window = RateWindow::new();
        window.push(at(0), 10_000, 100);
        assert_eq!(window.len(), 1);
        assert_eq!(window.rates(), Rates::default());
    }

    #[test]
    fn a_rate_is_the_difference_over_the_interval() {
        let mut window = RateWindow::new();
        window.push(at(0), 1_000, 10);
        window.push(at(1_000), 3_000, 30);
        let rates = window.rates();
        assert_eq!(rates.bytes_per_second, 2_000.0);
        assert_eq!(rates.packets_per_second, 20.0);
    }

    #[test]
    fn samples_closer_than_the_sample_interval_are_ignored() {
        let mut window = RateWindow::new();
        for frame in 0..20 {
            window.push(at(frame * 10), frame as u64 * 100, frame as u64);
        }
        assert_eq!(window.len(), 1, "twenty frames in 200 ms are one sample");
        assert_eq!(window.rates(), Rates::default());
    }

    #[test]
    fn the_window_never_spans_more_than_it_promises() {
        let mut window = RateWindow::new();
        for step in 0..200 {
            window.push(at(step * 600), step as u64 * 1_000, step as u64);
        }
        assert!(window.len() <= RATE_SAMPLES);
        assert!(
            window.span_seconds() <= RATE_WINDOW_SECONDS + RATE_SAMPLE_SECONDS,
            "spanned {} s",
            window.span_seconds()
        );
        // 1 000 bytes every 600 ms is 1 666.67 B/s whatever part of the window is kept.
        assert!((window.rates().bytes_per_second - 1_000.0 / 0.6).abs() < 1e-6);
    }

    #[test]
    fn a_gap_longer_than_the_window_forgets_what_came_before_it() {
        let mut window = RateWindow::new();
        window.push(at(0), 0, 0);
        // The station was minimised for a minute. Averaging over the gap would report a
        // minute of arrivals as if they were happening now.
        window.push(at(60_000), 60_000, 600);
        assert_eq!(window.len(), 1);
        assert_eq!(window.rates(), Rates::default());
        window.push(at(61_000), 61_000, 610);
        assert_eq!(window.rates().bytes_per_second, 1_000.0);
        assert_eq!(window.rates().packets_per_second, 10.0);
    }

    #[test]
    fn a_clock_that_stepped_backwards_restarts_the_window() {
        let mut window = RateWindow::new();
        window.push(at(10_000), 1_000, 10);
        window.push(at(11_000), 2_000, 20);
        window.push(at(9_000), 3_000, 30);
        assert_eq!(window.len(), 1);
        assert_eq!(window.rates(), Rates::default());
    }

    #[test]
    fn counters_that_went_backwards_do_not_give_a_negative_rate() {
        let mut window = RateWindow::new();
        window.push(at(0), 9_000, 90);
        window.push(at(1_000), 1_000, 10);
        assert_eq!(window.rates(), Rates::default());
    }

    #[test]
    fn a_packet_inside_the_live_window_is_live() {
        let status = bar(at(0), Some(at(9_000)), at(11_000), true);
        assert_eq!(status.liveness(), Liveness::Live);
        assert!(status.is_live());
    }

    #[test]
    fn silence_is_quiet_before_it_is_stopped() {
        let opened = at(0);
        let last = at(1_000);
        let quiet = at(1_000 + (LIVE_WINDOW_SECONDS as i64 + 1) * 1_000);
        assert_eq!(
            bar(opened, Some(last), quiet, true).liveness(),
            Liveness::Quiet
        );
        let stopped = at(1_000 + (QUIET_WINDOW_SECONDS as i64 + 1) * 1_000);
        assert_eq!(
            bar(opened, Some(last), stopped, true).liveness(),
            Liveness::Stopped
        );
    }

    #[test]
    fn the_live_threshold_is_inclusive_at_its_edge() {
        let last = at(1_000);
        let edge = at(1_000 + (LIVE_WINDOW_SECONDS * 1_000.0) as i64);
        assert_eq!(
            bar(at(0), Some(last), edge, true).liveness(),
            Liveness::Live
        );
    }

    #[test]
    fn a_source_that_is_no_longer_read_is_stopped_however_recent_the_last_packet() {
        let status = bar(at(0), Some(at(10_900)), at(11_000), false);
        assert_eq!(status.liveness(), Liveness::Stopped);
    }

    #[test]
    fn a_link_that_has_never_delivered_a_packet_ages_from_when_it_opened() {
        let opened = at(0);
        assert_eq!(
            bar(opened, None, at(2_000), true).liveness(),
            Liveness::Quiet
        );
        assert_eq!(
            bar(opened, None, at(60_000), true).liveness(),
            Liveness::Stopped
        );
    }

    #[test]
    fn a_bar_that_never_refreshed_is_not_live() {
        let status = Status::new();
        assert!(status.silence_seconds().is_none());
        assert_eq!(status.liveness(), Liveness::Stopped);
        assert!(!status.is_live());
        assert_eq!(status.rates(), Rates::default());
    }

    #[test]
    fn refreshing_samples_the_counters() {
        let counters = LinkStats::new();
        counters.add(&counters.bytes_in, 4_096);
        counters.add(&counters.packets_in, 8);
        let mut status = Status::new();
        status.refresh(&counters, true);
        assert_eq!(status.snapshot().bytes_in, 4_096);
        assert_eq!(status.rate_window().len(), 1);
        // One sample, so no rate yet — and two refreshes in the same frame are still one.
        status.refresh(&counters, true);
        assert_eq!(status.rate_window().len(), 1);
        assert_eq!(status.rates(), Rates::default());
    }

    #[test]
    fn the_correction_depth_is_symbols_over_the_frames_they_were_repaired_in() {
        let counters = LinkStats::new();
        counters.add(&counters.rs_corrected, 4);
        counters.add(&counters.rs_symbols, 30);
        let mut status = Status::new();
        status.refresh(&counters, true);
        assert!((status.correction_depth() - 7.5).abs() < 1e-9);
    }

    #[test]
    fn a_link_that_has_repaired_no_frame_has_no_correction_depth() {
        let counters = LinkStats::new();
        counters.add(&counters.rs_symbols, 0);
        let mut status = Status::new();
        status.refresh(&counters, true);
        assert_eq!(status.correction_depth(), 0.0);
    }

    #[test]
    fn the_correction_depth_is_on_the_bar_beside_the_frames_it_repaired() {
        let mut out = String::new();
        write_reed_solomon(
            &mut out,
            &StatsSnapshot {
                rs_corrected: 4,
                rs_symbols: 30,
                rs_uncorrectable: 1,
                ..StatsSnapshot::default()
            },
        );
        assert_eq!(out, "rs 4/1 (7.5 sym/frame)");

        // Nothing repaired, nothing to say about how deep the repairs went: the bar keeps the
        // width it has on a clean pass.
        out.clear();
        write_reed_solomon(&mut out, &StatsSnapshot::default());
        assert_eq!(out, "rs 0/0");
    }

    #[test]
    fn a_rate_is_rendered_with_a_decimal_only_where_it_helps() {
        let mut out = String::new();
        write_rate(&mut out, 0.333);
        assert_eq!(out, "0.3");
        out.clear();
        write_rate(&mut out, 1_234.6);
        assert_eq!(out, "1235");
    }

    #[test]
    fn the_bar_draws_over_a_framed_link_without_taking_the_window_down() {
        let counters = LinkStats::new();
        counters.add(&counters.bytes_in, 1_048_576);
        counters.add(&counters.frames_seen, 1_000);
        counters.add(&counters.frames_ok, 990);
        counters.add(&counters.frames_dropped, 10);
        counters.add(&counters.rs_corrected, 7);
        counters.add(&counters.rs_symbols, 52);
        counters.add(&counters.packets_in, 4_000);
        counters.add(&counters.packets_rejected, 3);
        counters.touch(Utc::now().unix_nanos());
        let mut status = Status::new();
        status.refresh(&counters, true);

        let layout = Layout::default();
        let mut scratch = String::new();
        egui::__run_test_ui(|ui| {
            let action = show(
                ui,
                &status,
                "udp://0.0.0.0:10015",
                true,
                &layout,
                false,
                &mut scratch,
            );
            assert!(action.is_none(), "nothing was clicked");
        });
    }

    #[test]
    fn the_bar_draws_over_a_link_that_has_received_nothing() {
        // The first frame of every session: no packet, no rate, and a default `Utc` that a
        // careless age would render as fifty-six years.
        let counters = LinkStats::new();
        let mut status = Status::new();
        status.refresh(&counters, true);
        let layout = Layout::default();
        let mut scratch = String::new();
        egui::__run_test_ui(|ui| {
            assert!(
                show(
                    ui,
                    &status,
                    "file:///stream.dat",
                    false,
                    &layout,
                    true,
                    &mut scratch
                )
                .is_none()
            );
        });
    }
}
