//! The plot grid: history against time, decimated to the pixels it will be drawn on.
//!
//! A watched parameter keeps up to a few hundred thousand points and a plot is a thousand
//! points wide. Handing all of them to `egui_plot` means tessellating a polyline with one
//! segment per sample, on the processor, sixty times a second — so
//! [`xtce_gs_core::decimate::min_max_lttb`] reduces what is in the visible x-range to about
//! the plot's width first. LTTB rather than averaging because a one-sample spike on a current
//! monitor is a latch-up and not noise; the reasoning is in that module and is not repeated
//! here.
//!
//! # Where the points live
//!
//! Each [`Series`] owns the `Vec<PlotPoint>` it was decimated into, and hands `egui_plot` a
//! [`egui_plot::PlotPoints::Borrowed`] view of it. **The points** are therefore neither copied
//! into the widget nor reallocated per frame: `Series::refill` clears and extends the buffer
//! it already has, and `App::refresh` calls [`Plots::refresh`] only when the store's
//! generation moved or the view changed under it. That is why [`show`] takes
//! `&Plots` and not `&mut Plots`: a line borrows its points for as long as the plot's closure
//! runs, and a mutable borrow of the same structure inside it would not compile. Everything
//! the operator changes comes back as an [`Action`] or lives in [`View`].
//!
//! What is *not* free — written down so that the next reader aims at the half that is left
//! and not at the half that is already paid for. Everything here is per series, per plot or
//! per grid mark; nothing on this list is per point, which is the property the decimation
//! exists to buy:
//!
//! * `egui_plot::Line::new` takes `impl Into<String>` (`egui_plot` 0.36, `items/series.rs`), so
//!   each series allocates its name every frame, even though the `.id()` below it is what
//!   actually identifies the line to the legend and the link group.
//! * Every `&str` given to egui — `ui.strong(&plot.title)`, each `on_hover_text`, each button
//!   label — goes through `impl From<&str> for WidgetText`, which is `text.to_owned()`
//!   (egui 0.35 `widget_text.rs`). A tooltip's text is allocated whether or not it is shown.
//! * [`time_tick`] returns a `String` per grid mark, and `PlotUi::add`/`PlotUi::hline` box
//!   each item.
//! * [`xtce_gs_core::decimate::min_max_lttb`] builds one intermediate `Vec` per call that
//!   actually decimates — it delegates to `lttb` and allocates nothing when the budget is
//!   already wider than the window — and reserves `threshold * ratio + 2` against a loop that
//!   pushes up to `2 * (threshold - 2) * ratio + 2`: 4002 against 7986 for a 1000-pixel plot
//!   at [`MIN_MAX_RATIO`]. So it grows once per series per frame on any data whose buckets
//!   mostly hold a distinct minimum and maximum, which is what telemetry with noise on it
//!   looks like.
//!
//! The last one is the only one worth a measurement, and it is not this crate's to fix: the
//! reserve is in `xtce-gs-core`, which is the spine, and a change there wants a line in
//! `PROGRESS.md` rather than a drive-by from a panel. See `Scratch`.
//!
//! # The x axis is shared
//!
//! Every plot is linked on x through `Plot::link_axis`, because the question a grid of plots
//! is there to answer is "what else happened at that moment". Linking y would be wrong for
//! the same reason: a current and a temperature share a time and nothing else.
//!
//! Linking alone is not enough to make that true while the grid is *following*, and this is
//! the one place where the widget's behaviour had to be read rather than assumed.
//! `egui_plot`'s `compute_bounds` reads the link group first and then, when `auto_bounds.x` is
//! set, throws the linked range away and re-fits x to that plot's own items — so a grid of
//! auto-fitting plots agrees on nothing and the link group ends up holding whatever the last
//! plot drawn happened to want. [`show`] therefore computes **one** follow window for the
//! whole grid (see `Plots::window`) and pins it on every plot with
//! `PlotUi::set_plot_bounds_x`. While paused nothing is pinned, `auto_bounds.x` stays off, and
//! the link group carries a drag on one plot to all of them, which is what it is for.
//!
//! For the same reason `Plot::auto_bounds` is not how pausing is implemented: it only seeds
//! `PlotMemory` on the first frame, so after that it is inert. The state is forced every frame
//! from inside the plot's closure instead.

use std::fmt::Write as _;

use xtce_gs_core::{Limit, LimitSet, ParameterStore, Point, RingBuffer, Utc};
use xtce_model::{ParamId, XtceDb};

use crate::layout::{Layout, PlotLayout};
use crate::panels::Action;

/// Decimated points per pixel of plot width.
///
/// One. Below one the line visibly loses shape when the operator zooms; above it the extra
/// points land on pixels that are already lit.
pub const POINTS_PER_PIXEL: f64 = 1.0;

/// How many buckets the min/max pre-pass keeps per final point.
///
/// Four, the ratio `MinMaxLTTB` was published with — see
/// [`xtce_gs_core::decimate::min_max_lttb`].
pub const MIN_MAX_RATIO: usize = 4;

/// Height of a new plot, in points.
pub const DEFAULT_HEIGHT: f32 = 160.0;

/// The link group every plot's x axis belongs to.
pub const LINK_GROUP: &str = "xtce-gs-x";

/// Decimated points asked for when the plot's width is not known yet.
///
/// [`Plots::refresh`] runs in `eframe::App::logic`, which is before any plot has reported a
/// width, so the first painted frame of every session is decimated against this rather than
/// against zero. A thousand points is more than a plot needs and cheap enough for one frame.
pub const DEFAULT_BUDGET: usize = 1024;

/// Fewest decimated points a plot is ever handed.
///
/// Below three [`xtce_gs_core::decimate::lttb`] keeps only the ends, so a narrower budget
/// turns every line into a chord between its first and last sample. Sixty-four and not three,
/// because the only way to reach the floor is a panel dragged nearly shut, and the frame after
/// the operator drags it open again is drawn from *this* budget — a plot that came back as a
/// straight line for one frame is a plot that looked like a dead parameter.
pub const MIN_BUDGET: usize = 64;

/// Most decimated points a plot is ever handed.
///
/// 16 384, which is four times the width of a 4K display. The cap is not about the display —
/// it is about a `width` that arrived from a layout or a window manager as something absurd,
/// and about keeping one frame's tessellation bounded whatever the caller passes.
pub const MAX_BUDGET: usize = 16_384;

/// Seconds of time axis shown by a grid that has no points at all.
///
/// A station whose first packet has not arrived still draws a time axis, and an axis one
/// second wide labelled to the microsecond is not one. Sixty seconds ending at the wall clock
/// is a window the first packet will land in.
pub const EMPTY_WINDOW_SECONDS: f64 = 60.0;

/// Distinct hues the parameter colours are drawn from — see [`color_for`].
const HUE_STEPS: u64 = 1024;

/// Seconds either side of the Unix epoch that a tick label is clamped to.
///
/// [`xtce_gs_core::Utc`] holds nanoseconds in an `i64`, which reaches 2262. The panel clamps
/// its tick formatting to well inside that: an axis past 2262-04-10 is not a view of any
/// telemetry, and a tick label is produced from whatever the operator drags the axis to.
///
/// This was originally a guard against a real arithmetic panic — `Utc::civil` multiplied its
/// own day count back out by 86 400 000 000 000, which overflows an `i64` within a day of
/// either end, on the drawing thread, from a drag. Core now floor-divides instead and cannot
/// overflow for any input, so this is no longer load-bearing; it stays because a tick label
/// reading "2262-04-11" is noise either way.
const CIVIL_LIMIT_SECONDS: f64 = 9_223_200_000.0;

/// FNV-1a's 64-bit offset basis (Fowler–Noll–Vo, 1991).
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a's 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The scratch two-stage decimation needs, owned once for the whole grid.
///
/// Two buffers and not two per series: decimation runs one series at a time, so one pair is
/// refilled per series and freed by nobody. A `Vec` allocated inside the per-frame loop would
/// be one allocation per plotted parameter per frame.
///
/// This removes the *decimation's* per-frame allocations and not every allocation on the draw
/// path — the module doc lists what is left. The nearest one is inside the call this buffer
/// feeds: [`xtce_gs_core::decimate::min_max_lttb`] still builds one intermediate `Vec` of its
/// own per call, and under-reserves it. Hoisting that one into this struct means core growing
/// a scratch-taking entry point, which is the spine changing, and the spine changes with a
/// reason in `PROGRESS.md` and not from here.
#[derive(Clone, Debug, Default)]
pub struct Scratch {
    visible: Vec<Point>,
    reduced: Vec<Point>,
}

impl Scratch {
    /// Empty buffers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// How many decimated points a plot `width` pixels wide should be handed.
///
/// A width of zero — the first frame, or a panel dragged shut — is [`DEFAULT_BUDGET`] rather
/// than nothing, and a width that is negative, infinite or `NaN` is treated the same way: the
/// number arrives from a widget and a plain `as usize` on it would be a silent zero.
#[must_use]
pub fn pixel_budget(width: f32) -> usize {
    if !width.is_finite() || width <= 0.0 {
        return DEFAULT_BUDGET;
    }
    // The cast saturates rather than wrapping, and the clamp catches both ends of it.
    ((f64::from(width) * POINTS_PER_PIXEL) as usize).clamp(MIN_BUDGET, MAX_BUDGET)
}

/// The half-open logical index range of a ring that an x range selects.
///
/// Logical means "oldest first", the order [`RingBuffer::as_slices`] hands the two halves back
/// in and the order a line is drawn in. The ring is time-ordered, so each half is searched
/// with [`slice::partition_point`] rather than scanned: at a history depth of 100 000 points a
/// scan per series per frame is the cost this whole module exists to avoid.
///
/// The range is widened by one point at each end. Without it a zoomed-in plot draws its line
/// from the first sample *inside* the window instead of from the frame edge, which reads as a
/// gap in the telemetry that is not there.
///
/// `None` selects the whole ring. An inverted or `NaN` range selects nothing.
///
// TODO(gs-gui-plots-monotonic): the search assumes `Point::t` never goes backwards. It does
// not today — the store stamps history from `Batch::time`, which is the packet's — but a
// spacecraft clock that steps back mid-pass, or a `TimeSource` with the wrong epoch applied to
// only some containers, would make it. The failure is a truncated window and not a panic.
// Deciding what to do needs a policy first: drop the out-of-order point at ingest, or keep it
// and make this a scan.
#[must_use]
pub fn visible_range(
    history: &RingBuffer<Point>,
    range: Option<[f64; 2]>,
) -> std::ops::Range<usize> {
    let (old, new) = history.as_slices();
    let total = old.len() + new.len();
    let Some([low, high]) = range else {
        return 0..total;
    };
    if low.is_nan() || high.is_nan() || high < low {
        return 0..0;
    }

    // All of `old` precedes all of `new`, so a bound that is not inside `old` is inside `new`.
    let logical = |bound: usize, rest: usize| {
        if bound < old.len() {
            bound
        } else {
            old.len() + rest
        }
    };
    let start = logical(
        old.partition_point(|point| point.t < low),
        new.partition_point(|point| point.t < low),
    );
    let end = logical(
        old.partition_point(|point| point.t <= high),
        new.partition_point(|point| point.t <= high),
    );

    let start = start.saturating_sub(1);
    let end = (end + 1).min(total);
    // The `max` cannot fire: `low <= high` makes `{t < low}` a subset of `{t <= high}`, so the
    // two partition points are ordered in whichever half they land in, and widening only moves
    // them apart. It is kept so that a `Range` leaving here can never be backwards whatever a
    // later edit does to the arithmetic above — a backwards one panics at its first `len()`.
    start..end.max(start)
}

/// The colour a parameter is always drawn in.
///
/// Hashed from the qualified name, so it is the same colour in the next session as in this
/// one, the same colour in two plots that both hold the parameter, and unchanged when the
/// series beside it is removed. `egui_plot`'s own `auto_color` assigns by insertion index and
/// gives none of that: deleting the first line recolours every line after it.
///
/// The name and not [`xtce_model::ParamId`] because an arena index belongs to one build of one
/// definition — add a parameter to the XML and every index after it moves — and this colour is
/// meant to outlive an edit to the definition, for the same reason the saved layout stores
/// names. FNV-1a, chosen because it is eight lines and this is not a security decision.
///
/// Two names can land close on the hue wheel. That is why the saturation and value vary as
/// well, and why the legend carries the name: a colour here is a memory aid, not an identifier.
/// The three brightness steps stay in the band `egui_plot`'s own `auto_color` uses — value 0.5
/// and below — because that band is what is legible against the light theme's near-white plot
/// background *and* the dark theme's near-black one, and this crate ships both.
#[must_use]
pub fn color_for(name: &str) -> egui::Color32 {
    let hash = fnv1a(name.as_bytes());
    let hue = (hash % HUE_STEPS) as f32 / HUE_STEPS as f32;
    let (saturation, value) = match (hash / HUE_STEPS) % 3 {
        0 => (0.85, 0.50),
        1 => (0.65, 0.42),
        _ => (1.00, 0.34),
    };
    egui::ecolor::Hsva::new(hue, saturation, value, 1.0).into()
}

/// FNV-1a over a byte string.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Renders one tick of the time axis.
///
/// `value` is seconds since the Unix epoch — what [`xtce_gs_core::Point`] carries — and `step`
/// is the distance to the next tick of the same thickness, which is what
/// [`egui_plot::GridMark`] offers to decide a resolution by. A grid at one tick a minute wants
/// `14:03` and one at a tick a millisecond wants `14:03:07.412`; neither wants `1.757e9`, which
/// is what the default formatter draws and is the reason this exists.
///
/// The date appears only at day-scale spacing. It is on the status bar once, and repeating it
/// on every tick costs the characters the time needs.
///
/// An axis dragged somewhere absurd renders a clamped date rather than panicking on the
/// drawing thread — see `CIVIL_LIMIT_SECONDS`. A `NaN` tick renders as the epoch, because
/// Rust's float-to-integer `as` cast maps it to zero.
#[must_use]
pub fn time_tick(value: f64, step: f64) -> String {
    // `clamp` passes a `NaN` through unchanged and the cast then maps it to zero.
    let nanos = (value.clamp(-CIVIL_LIMIT_SECONDS, CIVIL_LIMIT_SECONDS) * 1e9) as i64;
    let (year, month, day, hour, minute, second, nano) = Utc::from_unix_nanos(nanos).civil();
    let step = step.abs();
    if step >= 86_400.0 {
        format!("{year:04}-{month:02}-{day:02}")
    } else if step >= 60.0 {
        format!("{hour:02}:{minute:02}")
    } else if step >= 1.0 {
        format!("{hour:02}:{minute:02}:{second:02}")
    } else if step >= 0.001 {
        format!("{hour:02}:{minute:02}:{second:02}.{:03}", nano / 1_000_000)
    } else {
        format!("{hour:02}:{minute:02}:{second:02}.{:06}", nano / 1_000)
    }
}

/// One parameter's line in one plot.
#[derive(Clone, Debug)]
pub struct Series {
    parameter: ParamId,
    points: Vec<egui_plot::PlotPoint>,
    considered: usize,
}

impl Series {
    /// An empty line for a parameter.
    #[must_use]
    pub fn new(parameter: ParamId) -> Self {
        Self {
            parameter,
            points: Vec::new(),
            considered: 0,
        }
    }

    /// Which parameter this draws.
    #[must_use]
    pub const fn parameter(&self) -> ParamId {
        self.parameter
    }

    /// The decimated points, borrowed rather than copied.
    #[must_use]
    pub fn plot_points(&self) -> egui_plot::PlotPoints<'_> {
        egui_plot::PlotPoints::Borrowed(&self.points)
    }

    /// How many points will be drawn.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether there is nothing to draw.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// How many raw points those stand for.
    ///
    /// Shown next to the line's name, because an operator looking at a decimated plot has to
    /// be able to tell that it is one: a flat line of 200 points out of 40 000 and a flat line
    /// of 200 samples are different claims about the spacecraft.
    #[must_use]
    pub const fn considered(&self) -> usize {
        self.considered
    }

    /// The span of what will be drawn, as `[oldest, newest]` seconds since the Unix epoch.
    #[must_use]
    pub fn span(&self) -> Option<[f64; 2]> {
        let first = self.points.first()?;
        let last = self.points.last()?;
        Some([first.x, last.x])
    }

    /// Drops every point, keeping the capacity.
    ///
    /// What a plotted parameter that is no longer watched gets: its ring was freed, so there
    /// is nothing to redraw from, and leaving last frame's points on screen would show a line
    /// that has stopped moving as a line that is still true.
    pub fn clear(&mut self) {
        self.points.clear();
        self.considered = 0;
    }

    /// Refills this line from a parameter's history, decimated to `budget` points.
    ///
    /// `range` of `None` is the whole ring, which is what an unpaused plot following live
    /// telemetry wants; a paused one passes the window it was frozen at.
    ///
    /// Runs under the store's read lock: the caller holds it, hands over the ring, and drops it
    /// before anything draws.
    pub fn refill(
        &mut self,
        history: &RingBuffer<Point>,
        range: Option<[f64; 2]>,
        budget: usize,
        scratch: &mut Scratch,
    ) {
        let window = visible_range(history, range);
        let (old, new) = history.as_slices();
        let split = old.len();

        let head = old
            .get(window.start.min(split)..window.end.min(split))
            .unwrap_or_default();
        let tail = new
            .get(window.start.saturating_sub(split)..window.end.saturating_sub(split))
            .unwrap_or_default();

        // The count before decimation, which is the denominator of the plot's subtitle. Setting
        // it afterwards would report the decimated count twice and tell the operator nothing.
        self.considered = head.len() + tail.len();

        let Scratch { visible, reduced } = scratch;
        // `min_max_lttb` takes one slice, so the halves are joined only when the window
        // straddles the ring's wrap. A window inside one half — every window on a ring that has
        // not wrapped, and most windows on one that has — is decimated in place.
        let source: &[Point] = if tail.is_empty() {
            head
        } else if head.is_empty() {
            tail
        } else {
            visible.clear();
            visible.reserve(head.len() + tail.len());
            visible.extend_from_slice(head);
            visible.extend_from_slice(tail);
            visible
        };

        xtce_gs_core::decimate::min_max_lttb(source, budget, MIN_MAX_RATIO, reduced);

        self.points.clear();
        self.points
            .extend(reduced.iter().map(|point| egui_plot::PlotPoint {
                x: point.t,
                y: point.v,
            }));
    }
}

/// What the operator did to the axes, and whether the plots are following.
///
/// Separate from [`Plots`] so that [`show`] can take the points immutably and this mutably in
/// the same call. Public fields because it is three values with no invariant between them.
#[derive(Clone, Copy, Debug, Default)]
pub struct View {
    /// The x range every plot is showing, as `[min, max]` in seconds since the Unix epoch.
    ///
    /// `None` until the first frame has been drawn: the range comes *out* of `egui_plot`
    /// through `PlotResponse::transform`, and it is what the next frame's decimation is
    /// computed against.
    pub x_range: Option<[f64; 2]>,
    /// Whether the x range stays where the operator left it.
    ///
    /// Pausing does not pause the session — telemetry keeps arriving and the table keeps
    /// moving. It pauses the *axis*, which is the difference between reading a transient and
    /// watching it scroll off the left edge.
    pub paused: bool,
    /// Width of the plot area in physical pixels, from the last frame, for the decimation
    /// budget.
    ///
    /// Pixels and not points, because the budget is a number of line segments per lit pixel
    /// and a 2× display draws two of those per point — see [`POINTS_PER_PIXEL`]. Written from
    /// `PlotTransform::frame`, which is the plot itself, with the axis labels already taken
    /// off.
    pub width: f32,
}

/// Every plot on screen, and the points they draw.
///
/// Grouped exactly as [`crate::Layout::plots`] is: `groups[i]` holds the series for
/// `layout.plots[i]`. Two structures rather than one because the layout is what is saved and
/// this is what is redrawn, and a `Vec<PlotPoint>` has no business in a JSON file.
#[derive(Clone, Debug, Default)]
pub struct Plots {
    groups: Vec<Vec<Series>>,
    scratch: Scratch,
}

impl Plots {
    /// No plots.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The series, one `Vec` per plot, in layout order.
    #[must_use]
    pub fn groups(&self) -> &[Vec<Series>] {
        &self.groups
    }

    /// How many raw points the last refresh looked at across every series.
    #[must_use]
    pub fn considered(&self) -> usize {
        self.groups.iter().flatten().map(Series::considered).sum()
    }

    /// How many points will be drawn across every series.
    #[must_use]
    pub fn drawn(&self) -> usize {
        self.groups.iter().flatten().map(Series::len).sum()
    }

    /// Reconciles the series with the saved layout. Cheap when nothing changed.
    ///
    /// A name the definition does not declare is skipped silently — [`crate::Layout::restore`]
    /// is what counts and reports those at start-up, and reporting them again once per frame
    /// would fill the event log.
    pub fn sync(&mut self, layout: &Layout, db: &XtceDb) {
        self.sync_with(layout, |name| db.find_parameter(name));
    }

    /// [`Plots::sync`] with the name lookup handed in.
    ///
    /// Split out because an [`xtce_model::XtceDb`] can only be had by parsing an XTCE document,
    /// and the reconciliation — which series survive a layout change, in which order — is worth
    /// testing without one.
    fn sync_with(&mut self, layout: &Layout, resolve: impl Fn(&str) -> Option<ParamId>) {
        if self.matches(layout, &resolve) {
            return;
        }

        // Moved out of the old groups rather than rebuilt: a `Series` carries the points that
        // are on screen, and rebuilding it blanks every plot for one frame on any layout
        // change at all — including adding an unrelated plot.
        let mut previous = std::mem::take(&mut self.groups);
        self.groups.reserve(layout.plots.len());
        for (index, plot) in layout.plots.iter().enumerate() {
            let mut group = Vec::with_capacity(plot.parameters.len());
            for name in &plot.parameters {
                let Some(parameter) = resolve(name) else {
                    continue;
                };
                let kept = previous.get_mut(index).and_then(|held| {
                    held.iter()
                        .position(|series| series.parameter == parameter)
                        // `swap_remove` reorders what is left, which is fine: it is only ever
                        // searched by parameter after this, never by position.
                        .map(|at| held.swap_remove(at))
                });
                group.push(kept.unwrap_or_else(|| Series::new(parameter)));
            }
            self.groups.push(group);
        }
    }

    /// Whether the series already are what the layout asks for.
    fn matches(&self, layout: &Layout, resolve: &impl Fn(&str) -> Option<ParamId>) -> bool {
        self.groups.len() == layout.plots.len()
            && self
                .groups
                .iter()
                .zip(layout.plots.iter())
                .all(|(group, plot)| {
                    plot.parameters
                        .iter()
                        .filter_map(|name| resolve(name))
                        .eq(group.iter().map(Series::parameter))
                })
    }

    /// Refills every line from the store, for the range and width the last frame showed.
    ///
    /// A parameter that is in a plot and not watched has no ring; it is cleared rather than
    /// watched here, because this runs under the *read* lock and watching needs the write lock.
    /// [`crate::App`] is what notices and watches it, once.
    ///
    /// Runs under the read lock, and is the only method here that names the store.
    pub fn refresh(&mut self, store: &ParameterStore, view: View) {
        let budget = pixel_budget(view.width);
        // Following selects the whole ring: the window is computed *from* the points in
        // [`show`], so selecting by last frame's window as well would be a loop that can only
        // shrink. A paused grid selects the window it was frozen at, which is the whole point
        // of the pause and the reason decimation is worth anything at a hundred thousand points.
        let range = if view.paused { view.x_range } else { None };
        for series in self.groups.iter_mut().flatten() {
            match store.history(series.parameter) {
                Some(history) => series.refill(history, range, budget, &mut self.scratch),
                None => series.clear(),
            }
        }
    }

    /// The x window a following grid pins every plot to.
    ///
    /// One window for the whole grid — see the module header for why linking cannot do this on
    /// its own. `now` is only reached for when there is nothing to draw at all: a station
    /// waiting for its first packet still shows a time axis, and `egui_plot`'s fallback for
    /// empty bounds is ±1, which renders as 1970.
    fn window(&self, now: Utc) -> [f64; 2] {
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for [first, last] in self.groups.iter().flatten().filter_map(Series::span) {
            low = low.min(first);
            high = high.max(last);
        }
        if !high.is_finite() {
            let end = now.unix_secs_f64();
            return [end - EMPTY_WINDOW_SECONDS, end];
        }
        if !(high - low).is_finite() || high - low <= 0.0 {
            // One sample, or several that share an instant: a zero-width axis would be
            // sanitised by `egui_plot` into ±1 second around it, which labels to the tick.
            return [high - EMPTY_WINDOW_SECONDS, high];
        }
        [low, high]
    }
}

/// What every plot in the grid is drawn against.
///
/// Gathered once so that the per-plot call is six arguments and not ten, and so that the
/// follow window and the theme colours are resolved once per frame rather than once per plot.
struct Grid<'a> {
    db: &'a XtceDb,
    limits: &'a LimitSet,
    /// The x range to pin every plot to, or `None` while paused.
    follow: Option<[f64; 2]>,
    /// Whether the operator is driving the axes rather than following.
    paused: bool,
    /// The theme's warning colour, for the inner limit lines.
    warning: egui::Color32,
    /// The theme's alarm colour, for the outer limit lines.
    alarm: egui::Color32,
    /// The theme's weak text colour, for a plot with nothing in it.
    weak: egui::Color32,
    /// The font a plot with nothing in it says so in.
    font: egui::FontId,
}

/// Draws the plot grid. Returns what the operator asked for.
///
/// `limits` is [`xtce_gs_engine::Session::limits`]: a plot without the warning and alarm bounds
/// on it is a picture of a number, and what makes it a ground station display is being able to
/// see how far the number is from the one that matters.
pub fn show(
    ui: &mut egui::Ui,
    plots: &Plots,
    view: &mut View,
    layout: &Layout,
    db: &XtceDb,
    limits: &LimitSet,
    scratch: &mut String,
) -> Action {
    let grid = Grid {
        db,
        limits,
        follow: if view.paused {
            None
        } else {
            Some(plots.window(Utc::now()))
        },
        paused: view.paused,
        warning: ui.visuals().warn_fg_color,
        alarm: ui.visuals().error_fg_color,
        weak: ui.visuals().weak_text_color(),
        font: egui::TextStyle::Body.resolve(ui.style()),
    };
    let pixels_per_point = ui.pixels_per_point();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if layout.plots.is_empty() {
                ui.weak("No plots. Right-click a parameter in the list to open one.");
                return Action::None;
            }

            let mut action = Action::None;
            // Zipped and not indexed: for the frame between a plot being removed from the
            // layout and `Plots::sync` running, the two are different lengths, and indexing one
            // by the other's index is how a plot index goes out of range for exactly one frame.
            for (index, (group, plot)) in plots.groups().iter().zip(layout.plots.iter()).enumerate()
            {
                let (asked, transform) = show_plot(ui, &grid, index, group, plot, scratch);
                action = action.or(asked);

                // The first plot only: they are linked, so the rest agree, and reading all of
                // them means the last one drawn wins by accident.
                if index == 0 {
                    let bounds = transform.bounds();
                    if bounds.is_finite_x() {
                        view.x_range = Some([bounds.min()[0], bounds.max()[0]]);
                    }
                    view.width = transform.frame().width() * pixels_per_point;
                }
                ui.separator();
            }
            action
        })
        .inner
}

/// Draws one plot and returns what it was asked for and the transform it drew with.
///
/// The transform goes back to the caller rather than being written into [`View`] here, so that
/// only the first plot's is kept and this function needs no mutable state at all.
fn show_plot(
    ui: &mut egui::Ui,
    grid: &Grid<'_>,
    index: usize,
    group: &[Series],
    plot: &PlotLayout,
    scratch: &mut String,
) -> (Action, egui_plot::PlotTransform) {
    let drawn: usize = group.iter().map(Series::len).sum();
    let mut action = plot_header(ui, grid, index, group, plot, scratch);

    // The id is the index and not the title: two plots can be called the same thing, and
    // `egui_plot` paints an id clash over the plot rather than beside it.
    let response = egui_plot::Plot::new(("xtce-gs-plot", index))
        .height(plot.height)
        .legend(
            egui_plot::Legend::default()
                // By id and in insertion order: two parameters can share a leaf name, and
                // grouping those into one entry would hide both when either is clicked.
                .grouping(egui_plot::LegendGrouping::ById)
                .follow_insertion_order(true),
        )
        .link_axis(LINK_GROUP, [true, false])
        .link_cursor(LINK_GROUP, [true, false])
        // Driving the axes is what pausing is for: while the grid is following, every plot's x
        // range is pinned below, so a drag would be a rubber band.
        .allow_drag(grid.paused)
        .allow_zoom(grid.paused)
        .allow_boxed_zoom(grid.paused)
        // Never: the plots are stacked in a vertical scroll area, and a plot that eats the
        // wheel is a panel the operator cannot scroll.
        .allow_scroll(false)
        // A double click would reset `auto_bounds`, which this module sets every frame — a
        // gesture that silently does nothing is worse than one that is not offered.
        .allow_double_click_reset(false)
        .x_axis_label("time (UTC)")
        .x_axis_formatter(|mark, _| time_tick(mark.value, mark.step_size))
        .show(ui, |plot_ui| {
            // Every frame, because `Plot::auto_bounds` only seeds `PlotMemory` on the first
            // one. x is never automatic: it is pinned while following and left alone while
            // paused, so that the link group is what shares it.
            plot_ui.set_auto_bounds([false, plot.autoscale]);
            if let Some([low, high]) = grid.follow {
                plot_ui.set_plot_bounds_x(low..=high);
            }
            if !plot.autoscale
                && let Some([low, high]) = plot.y_range
            {
                plot_ui.set_plot_bounds_y(low..=high);
            }

            for series in group {
                let name = crate::fmt::name_of(grid.db, series.parameter());
                let qualified = crate::fmt::qualified_name_of(grid.db, series.parameter());
                // `PlotUi::add` and not `PlotUi::line`: the latter drops a line with no points,
                // and a parameter that is plotted but has not arrived has to keep its name and
                // its colour in the legend. That is the difference between an empty plot and a
                // plot that has forgotten what it is for.
                plot_ui.add(
                    egui_plot::Line::new(name, series.plot_points())
                        .id(egui::Id::new(qualified))
                        .color(color_for(qualified))
                        .width(1.2),
                );
                if let Some(limit) = grid.limits.get(qualified) {
                    limit_lines(plot_ui, limit, series.parameter(), grid);
                }
            }
        });

    if drawn == 0 {
        // Painted over the plot rather than added as a `Text` item: an item is positioned in
        // plot coordinates, and the only bounds reachable inside the closure are the previous
        // frame's, which puts the label off screen on the frame it is most needed.
        scratch.clear();
        if plot.title.is_empty() {
            scratch.push_str("no data");
        } else {
            let _ = write!(scratch, "{} — no data", plot.title);
        }
        ui.painter().text(
            response.transform.frame().center(),
            egui::Align2::CENTER_CENTER,
            &*scratch,
            grid.font.clone(),
            grid.weak,
        );
    }

    action = plot_menu(&response.response, grid, index, group).or(action);
    (action, response.transform)
}

/// The row above a plot: its name, how much of its history is on screen, and a way to close it.
fn plot_header(
    ui: &mut egui::Ui,
    grid: &Grid<'_>,
    index: usize,
    group: &[Series],
    plot: &PlotLayout,
    scratch: &mut String,
) -> Action {
    let drawn: usize = group.iter().map(Series::len).sum();
    let considered: usize = group.iter().map(Series::considered).sum();
    let mut action = Action::None;

    ui.horizontal(|ui| {
        ui.strong(&plot.title);
        scratch.clear();
        // `write!` into a `String` cannot fail; the `Result` is dropped rather than unwrapped,
        // because nothing in a draw is allowed to panic.
        //
        // The claim the subtitle makes: a flat line of 200 points out of 40 000 and a flat line
        // of 200 samples are different claims about the spacecraft.
        let _ = write!(scratch, "{drawn} of {considered} points");
        ui.weak(&*scratch)
            .on_hover_text("Points drawn, of the points in the visible time range");
        if grid.paused {
            ui.weak("paused");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("✕")
                .on_hover_text("Remove this plot. Its parameters stay watched.")
                .clicked()
            {
                action = Action::RemovePlot(index);
            }
        });
    });
    action
}

/// The plot's context menu.
///
/// The legend is drawn by `egui_plot` and its entries are not widgets this module can hang a
/// menu on, so removing one series lives here rather than on the legend entry it belongs to.
fn plot_menu(response: &egui::Response, grid: &Grid<'_>, index: usize, group: &[Series]) -> Action {
    let mut action = Action::None;
    response.context_menu(|ui| {
        for series in group {
            let name = crate::fmt::name_of(grid.db, series.parameter());
            if ui.button(format!("Remove {name}")).clicked() {
                action = Action::RemoveFromPlot {
                    parameter: series.parameter(),
                    plot: index,
                };
                ui.close();
            }
        }
        if !group.is_empty() {
            ui.separator();
        }
        // The same [`Action::SetPaused`] the status bar's toggle returns; the plots carry it
        // too because the pause is about *these* axes and this is where an operator reaches
        // for it.
        if ui
            .button(if grid.paused { "Resume" } else { "Pause" })
            .clicked()
        {
            action = Action::SetPaused(!grid.paused);
            ui.close();
        }
        ui.separator();
        if ui.button("Remove this plot").clicked() {
            action = Action::RemovePlot(index);
            ui.close();
        }
    });
    action
}

/// How much of the warning colour the band between the bounds keeps.
///
/// A twelfth: enough to read as a region at a glance, little enough that a line drawn through
/// it is still the thing the eye lands on.
const BAND_OPACITY: f32 = 1.0 / 12.0;

/// Draws a parameter's warning and alarm bounds across the plot.
///
/// Dashed and thin so they read as annotation rather than as telemetry, and unnamed so that
/// `egui_plot` keeps them out of the legend — `LegendWidget::try_new` skips an item whose name
/// is empty, which is the only way to add four lines per series without burying the names.
///
/// The lines are inside the y auto-bounds, so a plot of a parameter with limits shows the
/// headroom rather than the noise. When that is the wrong trade — an alarm bound decades away
/// from the value — the escape is the plot's own `autoscale: false` and `y_range`, which are
/// saved in the layout.
///
/// The two ranges are also shaded, between the warning bound and the alarm bound on each side
/// — that is the band an operator reads as "getting close" without having to compare a value
/// against two numbers. Both decisions the shading needs are made from the theme rather than
/// from a constant: the fill is the same colour as the line at a twelfth of its opacity, so
/// it is as dark or as light as the rest of the window, and it is drawn *before* the series,
/// so telemetry is never behind it.
fn limit_lines(
    plot_ui: &mut egui_plot::PlotUi<'_>,
    limit: &Limit,
    parameter: ParamId,
    grid: &Grid<'_>,
) {
    let bounds = [
        ("warn-low", limit.warning.low, grid.warning),
        ("warn-high", limit.warning.high, grid.warning),
        ("alarm-low", limit.alarm.low, grid.alarm),
        ("alarm-high", limit.alarm.high, grid.alarm),
    ];
    // Before the lines, and therefore before the series `show` draws after this: a filled
    // shape over a polyline hides the polyline.
    let shaded = [
        ("band-low", limit.alarm.low, limit.warning.low),
        ("band-high", limit.warning.high, limit.alarm.high),
    ];
    for (tag, from, to) in shaded {
        let _ = tag;
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        if !from.is_finite() || !to.is_finite() || from >= to {
            continue;
        }
        plot_ui.span(
            // No `id` and no `allow_hover`: `Span` has neither in egui_plot 0.36, and needs
            // neither — nothing here is interactive, and a zero-width border is what keeps it
            // from drawing an edge the operator would read as a limit of its own.
            egui_plot::Span::new("", from..=to)
                .axis(egui_plot::Axis::Y)
                .fill(grid.warning.gamma_multiply(BAND_OPACITY))
                .border_width(0.0),
        );
    }

    for (tag, at, color) in bounds {
        let Some(at) = at else { continue };
        if !at.is_finite() {
            continue;
        }
        plot_ui.hline(
            egui_plot::HLine::new("", at)
                .id(egui::Id::new((parameter.raw(), tag)))
                .color(color)
                .width(1.0)
                .style(egui_plot::LineStyle::dashed_loose())
                // A limit line is not a sample; offering it to the hover readout would put a
                // number on the crosshair that no packet ever carried.
                .allow_hover(false),
        );
    }
}

#[cfg(test)]
mod tests {
    use xtce_gs_core::{Limit, Range};

    use super::*;
    use crate::layout::PlotLayout;

    // Drawing is not unit-testable here and no test below pretends to be one: every `show` path
    // needs an `egui::Context`, a font atlas and a `PlotMemory` that only exists after a frame
    // has been painted, and `egui_plot` reports the bounds a plot settled on through a value
    // that cannot be built without one. What is tested is everything `show` decides *before* it
    // draws — which points are in range, how many of them survive, what colour they get and
    // what the axis says — because those are the parts that are wrong silently.

    fn ring(times: &[f64]) -> RingBuffer<Point> {
        let mut ring = RingBuffer::with_capacity(times.len().max(1));
        for (index, t) in times.iter().enumerate() {
            ring.push(Point {
                t: *t,
                v: index as f64,
            });
        }
        ring
    }

    /// A ring of `capacity` that has been pushed `pushes` points, so it has wrapped.
    fn wrapped(capacity: usize, pushes: usize) -> RingBuffer<Point> {
        let mut ring = RingBuffer::with_capacity(capacity);
        for index in 0..pushes {
            ring.push(Point {
                t: 1_757_000_000.0 + index as f64,
                v: index as f64,
            });
        }
        ring
    }

    #[test]
    fn no_range_selects_the_whole_ring() {
        let ring = ring(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(visible_range(&ring, None), 0..4);
    }

    #[test]
    fn a_range_selects_one_point_beyond_each_edge() {
        // Times 1..=5 at indices 0..=4. Asking for [2.5, 3.5] holds only index 2, and the
        // anchors on either side are what draw the line out to the frame.
        let ring = ring(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(visible_range(&ring, Some([2.5, 3.5])), 1..4);
    }

    #[test]
    fn the_range_edges_are_inside() {
        let ring = ring(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        // [2.0, 4.0] holds indices 1..=3; widened by one each way that is 0..5.
        assert_eq!(visible_range(&ring, Some([2.0, 4.0])), 0..5);
    }

    #[test]
    fn a_window_that_straddles_the_wrap_is_found_in_both_halves() {
        // Capacity 4, six pushes: the ring holds times ..002 ..003 ..004 ..005 with the oldest
        // physically at index 2, so `as_slices` gives two halves of two.
        let ring = wrapped(4, 6);
        let (old, new) = ring.as_slices();
        assert_eq!((old.len(), new.len()), (2, 2), "the ring did not wrap");

        // Logical indices 1..=2 span the halves; widened, 0..4.
        let window = visible_range(&ring, Some([1_757_000_003.0, 1_757_000_004.0]));
        assert_eq!(window, 0..4);

        // And a window wholly inside the second half stays there — the `old.len()` offset is
        // the thing an off-by-one here would hide.
        let window = visible_range(&ring, Some([1_757_000_005.0, 1_757_000_009.0]));
        assert_eq!(window, 2..4);
    }

    #[test]
    fn an_inverted_or_nan_range_selects_nothing() {
        let ring = ring(&[1.0, 2.0, 3.0]);
        assert_eq!(visible_range(&ring, Some([3.0, 1.0])), 0..0);
        assert_eq!(visible_range(&ring, Some([f64::NAN, 1.0])), 0..0);
        assert_eq!(visible_range(&ring, Some([1.0, f64::NAN])), 0..0);
    }

    #[test]
    fn a_range_outside_the_ring_selects_nothing_or_one_anchor() {
        let ring = ring(&[1.0, 2.0, 3.0]);
        // Entirely after the newest point: only the anchor before it.
        assert_eq!(visible_range(&ring, Some([10.0, 20.0])), 2..3);
        // Entirely before the oldest: only the anchor after it.
        assert_eq!(visible_range(&ring, Some([-20.0, -10.0])), 0..1);
    }

    #[test]
    fn an_empty_ring_selects_nothing_whatever_the_range() {
        let ring: RingBuffer<Point> = RingBuffer::with_capacity(16);
        assert_eq!(visible_range(&ring, None), 0..0);
        assert_eq!(visible_range(&ring, Some([1.0, 2.0])), 0..0);
    }

    #[test]
    fn a_zero_capacity_ring_selects_nothing() {
        let ring: RingBuffer<Point> = RingBuffer::with_capacity(0);
        assert_eq!(visible_range(&ring, None), 0..0);
        assert_eq!(visible_range(&ring, Some([1.0, 2.0])), 0..0);
    }

    #[test]
    fn the_pixel_budget_is_the_width_and_is_bounded_at_both_ends() {
        assert_eq!(pixel_budget(800.0), 800);
        assert_eq!(pixel_budget(1.0), MIN_BUDGET);
        assert_eq!(pixel_budget(1e9), MAX_BUDGET);
    }

    #[test]
    fn a_width_no_frame_has_reported_yet_gets_the_default_budget() {
        // `refresh` runs before any plot has a width, so zero is the first frame of every
        // session and must not decimate to nothing.
        assert_eq!(pixel_budget(0.0), DEFAULT_BUDGET);
        assert_eq!(pixel_budget(-100.0), DEFAULT_BUDGET);
        assert_eq!(pixel_budget(f32::NAN), DEFAULT_BUDGET);
        assert_eq!(pixel_budget(f32::INFINITY), DEFAULT_BUDGET);
        assert_eq!(pixel_budget(f32::NEG_INFINITY), DEFAULT_BUDGET);
    }

    #[test]
    fn a_colour_is_the_same_one_every_session() {
        // Pinned to a literal, because "the same name twice gives the same colour" is true of
        // any function at all. This is the assertion that fails if the palette is changed, and
        // a changed palette is exactly what "stable between sessions" forbids.
        assert_eq!(
            color_for("/Mission/Payload/Thermal/TEMP_A"),
            egui::Color32::from_rgb(0x1A, 0x9E, 0x00)
        );
        assert_eq!(
            color_for("/Mission/Power/BATT_V"),
            egui::Color32::from_rgb(0xA4, 0x6B, 0xAD)
        );
    }

    #[test]
    fn two_parameters_do_not_share_a_colour() {
        let names = [
            "/Sat/TEMP_A",
            "/Sat/TEMP_B",
            "/Sat/BATT_V",
            "/Sat/BATT_I",
            "/Sat/MODE",
        ];
        let mut seen: Vec<egui::Color32> = Vec::new();
        for name in names {
            let color = color_for(name);
            assert!(!seen.contains(&color), "{name} collided with another line");
            seen.push(color);
        }
    }

    #[test]
    fn an_empty_name_still_has_a_colour() {
        // `fmt::qualified_name_of` returns "" for an id this definition does not contain, and a
        // draw that indexed a palette by hash would divide by zero on it.
        let _ = color_for("");
    }

    #[test]
    fn a_time_tick_is_a_time_and_not_a_number() {
        // 2025-09-04T16:33:20.123456Z
        let t = 1_757_003_600.123_456;
        assert_eq!(time_tick(t, 86_400.0), "2025-09-04");
        assert_eq!(time_tick(t, 300.0), "16:33");
        assert_eq!(time_tick(t, 1.0), "16:33:20");
        assert_eq!(time_tick(t, 0.01), "16:33:20.123");
        assert_eq!(time_tick(t, 1e-5), "16:33:20.123456");
    }

    #[test]
    fn a_tick_before_the_epoch_or_off_the_end_of_the_clock_still_renders() {
        // A wrong CCSDS epoch puts the first packet in 1958, and an axis zoomed somewhere
        // absurd puts a tick past what `Utc::civil` can break down without overflowing an
        // `i64` — which is an arithmetic panic in a debug build. Neither may reach the
        // drawing thread.
        assert_eq!(time_tick(-378_691_200.0, 86_400.0), "1958-01-01");
        assert_eq!(time_tick(1e30, 86_400.0), "2262-04-10");
        assert_eq!(time_tick(-1e30, 86_400.0), "1677-09-23");
        assert_eq!(time_tick(f64::INFINITY, 86_400.0), "2262-04-10");
        assert_eq!(time_tick(f64::NEG_INFINITY, 86_400.0), "1677-09-23");
        // A `NaN` tick is the epoch rather than a panic; `egui_plot` can produce one from a
        // degenerate grid step.
        assert_eq!(time_tick(f64::NAN, f64::NAN), "00:00:00.000000");
    }

    #[test]
    fn refill_decimates_and_reports_what_it_looked_at() {
        let times: Vec<f64> = (0..5_000)
            .map(|i| 1_757_000_000.0 + f64::from(i) * 0.1)
            .collect();
        let ring = ring(&times);
        let mut series = Series::new(ParamId::new(0));
        let mut scratch = Scratch::new();

        series.refill(&ring, None, 200, &mut scratch);
        assert!(series.len() <= 200, "decimated to {} points", series.len());
        assert_eq!(
            series.considered(),
            5_000,
            "the subtitle would report the decimated count twice"
        );
        assert!(series.considered() > series.len());
    }

    #[test]
    fn refill_replaces_rather_than_appends() {
        let ring = ring(&[1.0, 2.0, 3.0, 4.0]);
        let mut series = Series::new(ParamId::new(0));
        let mut scratch = Scratch::new();
        series.refill(&ring, None, 1024, &mut scratch);
        let once = series.len();
        series.refill(&ring, None, 1024, &mut scratch);
        assert_eq!(series.len(), once);
        assert_eq!(series.considered(), 4);
    }

    #[test]
    fn refill_from_a_window_inside_the_second_half_of_a_wrapped_ring() {
        // The branch that decimates `tail` in place, with `visible` left holding whatever the
        // last series put there.
        let ring = wrapped(4, 6);
        let mut series = Series::new(ParamId::new(1));
        let mut scratch = Scratch::new();
        scratch.visible.push(Point { t: 0.0, v: 99.0 });

        series.refill(
            &ring,
            Some([1_757_000_005.0, 1_757_000_009.0]),
            1024,
            &mut scratch,
        );
        assert_eq!(series.considered(), 2);
        assert_eq!(
            series.span(),
            Some([1_757_000_004.0, 1_757_000_005.0]),
            "the anchor before the window is missing"
        );
    }

    #[test]
    fn refill_from_a_window_inside_the_first_half_of_a_wrapped_ring() {
        // The third source branch: both halves are populated, but the window lands wholly in
        // the older one, so `head` is decimated in place and `tail` is empty. A wrapped ring
        // reaches every branch and only this window reaches this one.
        let ring = wrapped(4, 6);
        let mut series = Series::new(ParamId::new(1));
        let mut scratch = Scratch::new();

        series.refill(
            &ring,
            Some([1_757_000_001.0, 1_757_000_002.5]),
            1024,
            &mut scratch,
        );
        assert_eq!(series.considered(), 2);
        assert_eq!(series.span(), Some([1_757_000_002.0, 1_757_000_003.0]));
    }

    #[test]
    fn refill_from_a_window_that_straddles_the_wrap() {
        let ring = wrapped(4, 6);
        let mut series = Series::new(ParamId::new(1));
        let mut scratch = Scratch::new();

        series.refill(
            &ring,
            Some([1_757_000_003.0, 1_757_000_004.0]),
            1024,
            &mut scratch,
        );
        assert_eq!(series.considered(), 4);
        assert_eq!(series.span(), Some([1_757_000_002.0, 1_757_000_005.0]));
        // The joined halves have to stay in time order or the line folds back on itself.
        let points: Vec<f64> = (0..series.len())
            .filter_map(|i| series.points.get(i).map(|p| p.x))
            .collect();
        assert!(points.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn refill_from_an_empty_ring_draws_nothing_and_does_not_panic() {
        let ring: RingBuffer<Point> = RingBuffer::with_capacity(8);
        let mut series = Series::new(ParamId::new(0));
        let mut scratch = Scratch::new();
        series.refill(&ring, None, 1024, &mut scratch);
        assert!(series.is_empty());
        assert_eq!(series.considered(), 0);
        assert_eq!(series.span(), None);
    }

    #[test]
    fn refill_survives_a_budget_of_zero() {
        // Not reachable through `pixel_budget`, which floors at `MIN_BUDGET`, but `refill` is
        // public and a budget is a `usize` the caller chose.
        let ring = ring(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let mut series = Series::new(ParamId::new(0));
        let mut scratch = Scratch::new();
        series.refill(&ring, None, 0, &mut scratch);
        assert!(
            series.len() <= 2,
            "a budget of zero kept {} points",
            series.len()
        );
    }

    #[test]
    fn clearing_a_series_forgets_the_points_and_the_count() {
        let ring = ring(&[1.0, 2.0, 3.0]);
        let mut series = Series::new(ParamId::new(0));
        let mut scratch = Scratch::new();
        series.refill(&ring, None, 1024, &mut scratch);
        series.clear();
        assert!(series.is_empty());
        assert_eq!(series.considered(), 0);
    }

    fn layout_with(plots: &[&[&str]]) -> Layout {
        Layout {
            plots: plots
                .iter()
                .map(|names| PlotLayout {
                    parameters: names.iter().map(|name| (*name).to_owned()).collect(),
                    ..PlotLayout::default()
                })
                .collect(),
            ..Layout::default()
        }
    }

    /// `/Sat/A` is parameter 0, `/Sat/B` is 1, `/Sat/C` is 2, anything else is undeclared.
    fn resolve(name: &str) -> Option<ParamId> {
        match name {
            "/Sat/A" => Some(ParamId::new(0)),
            "/Sat/B" => Some(ParamId::new(1)),
            "/Sat/C" => Some(ParamId::new(2)),
            _ => None,
        }
    }

    #[test]
    fn sync_builds_one_group_per_plot_in_layout_order() {
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A", "/Sat/B"], &["/Sat/C"]]), resolve);
        let shape: Vec<Vec<ParamId>> = plots
            .groups()
            .iter()
            .map(|group| group.iter().map(Series::parameter).collect())
            .collect();
        assert_eq!(
            shape,
            vec![
                vec![ParamId::new(0), ParamId::new(1)],
                vec![ParamId::new(2)]
            ]
        );
    }

    #[test]
    fn a_name_the_definition_does_not_declare_is_skipped_silently() {
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A", "/Sat/GONE", "/Sat/B"]]), resolve);
        assert_eq!(plots.groups().len(), 1);
        assert_eq!(
            plots.groups().first().map(Vec::len),
            Some(2),
            "an undeclared name became a series"
        );
    }

    #[test]
    fn sync_keeps_the_points_of_a_series_that_is_still_there() {
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A"]]), resolve);

        let ring = ring(&[1.0, 2.0, 3.0]);
        let mut scratch = Scratch::new();
        if let Some(series) = plots.groups.first_mut().and_then(|group| group.first_mut()) {
            series.refill(&ring, None, 1024, &mut scratch);
        }
        assert_eq!(plots.drawn(), 3);

        // Adding a second plot must not blank the first: rebuilding the series would.
        plots.sync_with(&layout_with(&[&["/Sat/A"], &["/Sat/B"]]), resolve);
        assert_eq!(plots.groups().len(), 2);
        assert_eq!(
            plots.drawn(),
            3,
            "an unrelated plot blanked an existing one"
        );
        assert_eq!(plots.considered(), 3);
    }

    #[test]
    fn sync_drops_a_series_the_layout_no_longer_holds() {
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A", "/Sat/B"]]), resolve);
        plots.sync_with(&layout_with(&[&["/Sat/B"]]), resolve);
        assert_eq!(
            plots
                .groups()
                .first()
                .map(|group| group.iter().map(Series::parameter).collect::<Vec<_>>()),
            Some(vec![ParamId::new(1)])
        );
    }

    #[test]
    fn syncing_to_an_empty_layout_removes_every_plot() {
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A"]]), resolve);
        plots.sync_with(&Layout::default(), resolve);
        assert!(plots.groups().is_empty());
    }

    #[test]
    fn sync_is_a_no_op_when_nothing_changed() {
        let mut plots = Plots::new();
        let layout = layout_with(&[&["/Sat/A"]]);
        plots.sync_with(&layout, resolve);

        let ring = ring(&[1.0, 2.0, 3.0]);
        let mut scratch = Scratch::new();
        if let Some(series) = plots.groups.first_mut().and_then(|group| group.first_mut()) {
            series.refill(&ring, None, 1024, &mut scratch);
        }
        plots.sync_with(&layout, resolve);
        assert_eq!(plots.drawn(), 3);
    }

    #[test]
    fn the_follow_window_spans_every_plot_and_not_each_one() {
        // The whole reason `window` exists: two plots whose parameters cover different stretches
        // of the pass share one x axis, and it is the union.
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A"], &["/Sat/B"]]), resolve);
        let mut scratch = Scratch::new();
        let mut fill = |plot: usize, times: &[f64]| {
            let ring = ring(times);
            if let Some(series) = plots
                .groups
                .get_mut(plot)
                .and_then(|group| group.first_mut())
            {
                series.refill(&ring, None, 1024, &mut scratch);
            }
        };
        fill(0, &[100.0, 110.0, 120.0]);
        fill(1, &[90.0, 95.0]);

        assert_eq!(plots.window(Utc::from_unix_secs(0)), [90.0, 120.0]);
    }

    #[test]
    fn a_plot_with_nothing_in_it_does_not_drag_the_window_to_the_epoch() {
        // A parameter that has not arrived contributes no span at all. If it contributed
        // `[0, 0]` instead, the shared axis would run from 1970 to the pass and every plot in
        // the grid would be a flat line at the right-hand edge.
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A"], &["/Sat/B"]]), resolve);
        let mut scratch = Scratch::new();
        let ring = ring(&[1_757_000_100.0, 1_757_000_200.0]);
        if let Some(series) = plots.groups.get_mut(1).and_then(|group| group.first_mut()) {
            series.refill(&ring, None, 1024, &mut scratch);
        }
        assert_eq!(
            plots
                .groups
                .first()
                .and_then(|group| group.first())
                .map(Series::len),
            Some(0)
        );
        assert_eq!(
            plots.window(Utc::from_unix_secs(0)),
            [1_757_000_100.0, 1_757_000_200.0]
        );
    }

    #[test]
    fn a_grid_with_nothing_in_it_still_gets_a_window_ending_now() {
        let plots = Plots::new();
        let now = Utc::from_unix_secs(1_757_000_000);
        assert_eq!(
            plots.window(now),
            [1_757_000_000.0 - EMPTY_WINDOW_SECONDS, 1_757_000_000.0]
        );
    }

    #[test]
    fn a_single_sample_gets_a_window_and_not_a_zero_width_axis() {
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A"]]), resolve);
        let mut scratch = Scratch::new();
        let ring = ring(&[1_757_000_000.0]);
        if let Some(series) = plots.groups.first_mut().and_then(|group| group.first_mut()) {
            series.refill(&ring, None, 1024, &mut scratch);
        }
        let window = plots.window(Utc::from_unix_secs(0));
        assert!(window[1] > window[0]);
        assert_eq!(
            window[1], 1_757_000_000.0,
            "the newest point is not at the edge"
        );
    }

    #[test]
    fn refresh_clears_a_plotted_parameter_that_is_not_watched() {
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A", "/Sat/B"]]), resolve);

        let mut store = ParameterStore::new(4, 64);
        store.watch(ParamId::new(0));
        for index in 0..10 {
            store.push(xtce_gs_core::Sample {
                parameter: ParamId::new(0),
                time: Utc::from_unix_secs(1_757_000_000 + index),
                raw: xtce_gs_core::Value::Unsigned(index as u64),
                eng: xtce_gs_core::Value::Float(index as f64),
            });
        }

        plots.refresh(&store, View::default());
        let group = plots
            .groups()
            .first()
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert_eq!(group.first().map(Series::len), Some(10));
        // `/Sat/B` is plotted and not watched: no ring, so nothing to draw, and no write lock
        // taken to fix it.
        assert_eq!(group.get(1).map(Series::len), Some(0));
        assert_eq!(group.get(1).map(Series::considered), Some(0));
    }

    #[test]
    fn a_paused_refresh_decimates_only_the_frozen_window() {
        let mut plots = Plots::new();
        plots.sync_with(&layout_with(&[&["/Sat/A"]]), resolve);

        let mut store = ParameterStore::new(4, 1024);
        store.watch(ParamId::new(0));
        for index in 0..100 {
            store.push(xtce_gs_core::Sample {
                parameter: ParamId::new(0),
                time: Utc::from_unix_secs(1_757_000_000 + index),
                raw: xtce_gs_core::Value::Unsigned(index as u64),
                eng: xtce_gs_core::Value::Float(index as f64),
            });
        }

        plots.refresh(
            &store,
            View {
                x_range: Some([1_757_000_010.0, 1_757_000_019.0]),
                paused: true,
                width: 1000.0,
            },
        );
        // Ten points in the window plus the anchor either side of it.
        assert_eq!(plots.considered(), 12);

        // Unpaused with the *same* range still on the view: it is the pause flag that decides,
        // not whether a range has ever been recorded. A following grid derives its window from
        // the points, so narrowing to last frame's window here could only ever shrink it.
        plots.refresh(
            &store,
            View {
                x_range: Some([1_757_000_010.0, 1_757_000_019.0]),
                paused: false,
                width: 1000.0,
            },
        );
        assert_eq!(plots.considered(), 100);
    }

    #[test]
    fn a_limit_with_one_end_set_is_still_a_limit() {
        // Not a drawing test: what `limit_lines` iterates is four `Option`s, and the case that
        // bites is a file that sets only a high bound.
        let limit = Limit {
            warning: Range {
                low: None,
                high: Some(50.0),
            },
            alarm: Range::default(),
        };
        let bounds = [
            limit.warning.low,
            limit.warning.high,
            limit.alarm.low,
            limit.alarm.high,
        ];
        assert_eq!(bounds.iter().filter(|end| end.is_some()).count(), 1);
    }

    #[test]
    fn a_limit_set_with_no_entry_for_a_parameter_draws_no_lines() {
        let mut limits = LimitSet::new();
        limits.insert("/Sat/A", Limit::default());
        assert!(limits.get("/Sat/B").is_none());
    }
}
