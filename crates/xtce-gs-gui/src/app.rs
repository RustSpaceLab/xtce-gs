//! The window: one session, five panels, and the two places a lock is taken.
//!
//! egui is immediate mode, so this type is not a widget tree — it is the state a frame is
//! drawn from, plus the buffers that state is copied into. The frame has two halves and the
//! [`eframe::App`] trait hands them over separately:
//!
//! * [`eframe::App::logic`] — called before every draw, and also when the window is hidden and
//!   something called `request_repaint`. This is where the store's read lock is taken, the
//!   panels' scratch buffers are refilled, and the lock is dropped.
//! * [`eframe::App::ui`] — called to draw. It touches no lock, no atomic and no session; it
//!   lays out the panels over the buffers `logic` filled and collects one [`Action`].
//!
//! The action is applied at the end of `ui`, which is the only place the *write* lock is
//! taken, at most once a frame. Everything about this arrangement follows from one fact: the
//! decode thread needs the write lock to file a packet, and a station that held the read lock
//! across a draw would block it for a frame every frame.
//!
//! # Repainting
//!
//! The session holds a waker filled with `egui::Context::request_repaint` and calls it after
//! each batch is ingested, so a loaded station repaints at the frame rate and an idle one does
//! not repaint at all — except once a second, which [`eframe::App::logic`] asks for through
//! `request_repaint_after` so that the ages and the live indicator keep moving. Comparing the
//! store's generation with the last one drawn is what makes an unchanged frame cheap.

use std::sync::{Arc, PoisonError};

use xtce_gs_core::{Event, ParameterStore, Utc};
use xtce_gs_engine::Session;
use xtce_gs_engine::session::log;
use xtce_model::{ParamId, XtceDb};

use crate::layout::Layout;
use crate::panels::events::Events;
use crate::panels::plots::{Plots, View};
use crate::panels::status::Status;
use crate::panels::table::Table;
use crate::panels::tree::Tree;
use crate::panels::{Action, events, plots, status, table, tree};

/// What the window is called.
pub const WINDOW_TITLE: &str = "xtce-gs";

/// Seconds between repaints when nothing is arriving.
///
/// One. The clock, the ages and the live indicator move on their own, and a station that only
/// repainted on telemetry would show an age frozen at "2 s" for a pass that ended.
pub const IDLE_REPAINT_SECONDS: f32 = 1.0;

/// Passes whose buffers are refilled after the window saw an input event.
///
/// Two, and the second is the one that matters. [`eframe::App::logic`] runs *before*
/// [`eframe::App::ui`] in the same pass, so a keystroke in the table's filter box reaches the
/// panel after that pass has already copied its rows out; the pass after it is the first one
/// that can show the result. One extra copy per event is nothing — a few hundred rows — and
/// missing it leaves the operator typing into a table that does not move until the next
/// packet, which on a station between passes is never.
pub const INPUT_REFRESH_PASSES: u8 = 2;

/// The shallowest history the operator can ask for.
///
/// One point. A ring of none drops every sample it is given, which is why
/// `xtce_gs_engine::SessionConfig::validate` refuses a depth of zero at start-up; a control
/// that could set it to zero while running would make the same store useless with nothing on
/// the log to say so.
pub const MIN_HISTORY_DEPTH: usize = 1;

/// The table's share of the central area when nothing has been dragged.
///
/// Under half, because the plots are what the operator watches and the table is what they
/// check. Both panels are resizable; this is only where they start.
pub const TABLE_SHARE: f32 = 0.45;

/// The running interface.
///
/// Holds the [`Session`] — dropping this shuts it down, because [`Session`]'s own `Drop` asks
/// both tasks to stop — the saved [`Layout`], and one scratch buffer per panel.
pub struct App {
    session: Session,
    db: Arc<XtceDb>,
    layout: Layout,
    status: Status,
    tree: Tree,
    table: Table,
    events: Events,
    plots: Plots,
    view: View,
    scratch: String,
    generation: u64,
    layout_dirty: bool,
    /// The axis and width the plots were last decimated for.
    ///
    /// The store's generation says whether there is anything new to draw; this says whether
    /// what is already held would be drawn differently. An operator who pans a paused plot,
    /// or who widens the window, moves neither the generation nor a dirty flag — and the
    /// decimation budget is computed from exactly these two numbers.
    view_drawn: View,
    /// Passes still owed a refill after input arrived — see [`INPUT_REFRESH_PASSES`].
    input_passes: u8,
}

/// Whether two axis ranges are the same number.
///
/// Compared bitwise, and deliberately: the question is whether the range *changed*, not
/// whether the two are numerically equal. A plot with no points inside it hands back bounds
/// of `NaN`, and `NaN != NaN` would make every frame of an empty plot a frame that decimates
/// the whole ring again.
fn same_range(left: Option<[f64; 2]>, right: Option<[f64; 2]>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left[0].to_bits() == right[0].to_bits() && left[1].to_bits() == right[1].to_bits()
        }
        _ => false,
    }
}

/// Whether a frame drawn for `left` would decimate to the same points as one drawn for
/// `right`.
fn same_view(left: View, right: View) -> bool {
    left.paused == right.paused
        && left.width.to_bits() == right.width.to_bits()
        && same_range(left.x_range, right.x_range)
}

/// Adds every plotted parameter to the layout's watch list.
///
/// The layout's own invariant is that a plotted parameter is also a watched one, and this is
/// where a file that was edited by hand is brought back to it. A plot naming a parameter with
/// no ring draws an empty box for the rest of the pass, and the operator's reasonable reading
/// of an empty box is that the telemetry is missing.
fn watch_what_is_plotted(layout: &mut Layout) {
    let plotted: Vec<String> = layout
        .plots
        .iter()
        .flat_map(|plot| plot.parameters.iter().cloned())
        .collect();
    for name in plotted {
        if !layout.watched.contains(&name) {
            layout.watched.push(name);
        }
    }
}

impl App {
    /// Builds the interface around a started session.
    ///
    /// Takes the store's *write* lock once, at start-up, to set the depth the layout asks for
    /// and watch what it names — depth first, so the rings come out at the right size rather
    /// than being allocated and then resized one by one.
    ///
    /// How many of the saved names resolved goes on the event log. A definition that was
    /// edited between two runs is the case that tells the operator about, and this is the
    /// only chance to: every later frame is drawn from the store, which cannot tell a
    /// parameter that was never declared from one that has not arrived.
    ///
    /// `cc.storage` is `None` in this build — `eframe`'s persistence feature is off, so the
    /// layout comes from the file [`crate::Layout::load`] read before the window opened. The
    /// parameter is taken anyway because it is where the theme and the fonts belong.
    #[must_use]
    pub fn new(cc: &eframe::CreationContext<'_>, session: Session, mut layout: Layout) -> Self {
        let db = Arc::clone(session.db());
        cc.egui_ctx.set_theme(layout.theme.preference());

        watch_what_is_plotted(&mut layout);
        let named = layout.watched.len();
        let resolved = {
            let mut store = session
                .store()
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            store.set_depth(layout.history_depth.max(MIN_HISTORY_DEPTH));
            layout.restore(&db, &mut store)
        };
        if named > 0 {
            log(session.events(), restore_report(resolved, named));
        }

        // Built here and not on the first frame: `Plots::refresh` draws from `groups`, and a
        // grid that was empty until the first action would show a saved arrangement only
        // after the operator touched something.
        let mut plots = Plots::new();
        plots.sync(&layout, &db);

        Self {
            session,
            db,
            layout,
            status: Status::new(),
            // `Tree::new` starts dirty, which is what forces the first frame's copy: the
            // store's generation can legitimately still be 0 at this point.
            tree: Tree::new(),
            table: Table::new(),
            events: Events::new(),
            plots,
            view: View::default(),
            scratch: String::new(),
            generation: 0,
            layout_dirty: false,
            view_drawn: View::default(),
            input_passes: INPUT_REFRESH_PASSES,
        }
    }

    /// The session being drawn.
    #[must_use]
    pub const fn session(&self) -> &Session {
        &self.session
    }

    /// The definition, shared with the decode thread.
    #[must_use]
    pub const fn db(&self) -> &Arc<XtceDb> {
        &self.db
    }

    /// What will be saved when the window closes.
    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The store generation the buffers were filled from.
    ///
    /// Compared with the store's own in [`App::refresh`]; a frame where they agree copies
    /// nothing.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the layout has changed since it was last written.
    #[must_use]
    pub const fn is_layout_dirty(&self) -> bool {
        self.layout_dirty
    }

    /// Copies out everything this frame will draw. The only place a read lock is taken.
    ///
    /// **The store's read lock is taken once, everything the panels need is copied out under
    /// it, and it is dropped before anything draws.** That is the whole shape of this crate:
    /// the decode thread needs the write lock to file a packet, and a station that held the
    /// read lock across a draw would block it for a frame, every frame. Nothing below the
    /// guard's scope touches the store, and nothing inside it draws.
    ///
    /// The link bar is refreshed *before* the guard is taken and not under it. Those counters
    /// are atomics no lock covers — see the comment on the call — so a snapshot taken inside
    /// the guard is no more consistent with the values than one taken outside it, and only
    /// keeps the writer waiting.
    ///
    /// The store half is skipped when the generation has not moved and nothing in the
    /// interface asked for it. That is what keeps an idle station off the processor; it is
    /// also why [`Tree`] carries its own dirty flag rather than relying on the generation,
    /// and why [`View`] is compared against the one the buffers were filled for.
    ///
    /// A poisoned lock is recovered through `PoisonError::into_inner` rather than dropping
    /// the frame: a panic on the decode thread must not leave the operator with a window that
    /// stopped updating and no explanation, when the explanation is a line on the event log
    /// this same frame would have drawn.
    pub fn refresh(&mut self) {
        let forced =
            self.input_passes > 0 || self.tree.is_dirty() || !same_view(self.view, self.view_drawn);
        self.input_passes = self.input_passes.saturating_sub(1);

        // Outside the guard, and deliberately: the link counters are `Relaxed` atomics that
        // no lock covers. Thirteen of the eighteen are written by the link task, which never
        // touches the store, and the decode task bumps the rest before it takes the write
        // lock — so holding the read lock across this buys no consistency and only makes the
        // writer wait through a `Utc::now()` and eighteen atomic loads. The row is a
        // best-effort sample: `frames_ok` can read one behind `frames_seen` on a clean pass,
        // because those are two `fetch_add`s and a snapshot can land between them. What the
        // bar does guarantee is that its own numbers come from one call to
        // `LinkStats::snapshot` and are aged against one `Status::taken`, which is the
        // consistency an operator can see. Nothing below reads `self.status`.
        self.status
            .refresh(self.session.stats(), self.session.is_running());

        {
            let store = self
                .session
                .store()
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if forced || store.generation() != self.generation {
                self.tree.refresh(&self.db, &store);
                self.table.refresh(&store, &self.db, self.session.limits());
                self.plots.refresh(&store, self.view);
                self.generation = store.generation();
                self.view_drawn = self.view;
            }
        }

        // A different mutex, and taken only after the store's guard is gone. Two locks taken
        // in two orders by two threads is the one deadlock this crate has a rule against,
        // and the decode thread takes the store's and then the log's.
        {
            let events = self
                .session
                .events()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.events.refresh(&events);
        }
    }

    /// Carries out what a panel asked for. The only place a write lock is taken.
    ///
    /// Called once, after the last panel of a frame has been laid out. Everything that
    /// changes the layout sets `layout_dirty`; everything that changes what is *kept* goes
    /// through [`App::edit_store`], which is the only writer of the store in this crate.
    pub fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            // The common case, sixty times a second.
            Action::None => {}
            // Changes what is shown and not what is kept, so it takes no lock at all.
            Action::Select(parameter) => self.tree.select(Some(parameter)),
            Action::Watch(parameter) => self.watch(parameter),
            Action::Unwatch(parameter) => self.unwatch(parameter),
            Action::AddToPlot { parameter, plot } => self.add_to_plot(parameter, plot),
            Action::RemoveFromPlot { parameter, plot } => self.remove_from_plot(parameter, plot),
            Action::NewPlot(parameter) => self.new_plot(parameter),
            Action::RemovePlot(plot) => {
                self.layout.remove_plot(plot);
                self.relayout();
            }
            Action::SetHistoryDepth(depth) => self.set_history_depth(depth),
            // Pausing is an axis and not a session: telemetry keeps arriving, the table keeps
            // moving, and nothing downstream of the store knows this happened.
            Action::SetPaused(paused) => self.view.paused = paused,
            Action::SetTheme(theme) => {
                ctx.set_theme(theme.preference());
                self.layout.theme = theme;
                self.layout_dirty = true;
            }
            Action::ClearEvents => self.clear_events(),
            Action::Quit => {
                // Saved here and not left to the close event: `ViewportCommand::Close` goes
                // out to the window system and comes back as a `close_requested` pass only if
                // nothing refuses it on the way, and the arrangement is what the operator
                // spent the pass making.
                if self.layout_dirty {
                    self.save_layout();
                }
                self.session.shutdown();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Runs one edit under the store's write lock.
    ///
    /// Every caller holds it for a single call — a watch, an unwatch, a depth — and none of
    /// them draws while it is held.
    fn edit_store(&self, edit: impl FnOnce(&mut ParameterStore)) {
        let mut store = self
            .session
            .store()
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        edit(&mut store);
    }

    /// A parameter's qualified name, owned.
    ///
    /// Owned because the borrow is of `self.db` and every caller goes on to write
    /// `self.layout`. One allocation per operator action; nothing here runs per frame.
    fn qualified(&self, parameter: ParamId) -> String {
        crate::fmt::qualified_name_of(&self.db, parameter).to_owned()
    }

    /// Keeps history for a parameter, and records that in the layout.
    fn watch(&mut self, parameter: ParamId) {
        let name = self.qualified(parameter);
        // Empty means the id is not from this definition, which the store is sized by. There
        // is nothing to watch and nothing that could be saved and resolved again.
        if name.is_empty() {
            return;
        }
        self.edit_store(|store| {
            store.watch(parameter);
        });
        if !self.layout.watched.contains(&name) {
            self.layout.watched.push(name);
        }
        self.layout_dirty = true;
    }

    /// Stops keeping history for a parameter, and stops drawing it.
    ///
    /// The plots lose it too. A plot cannot draw a parameter whose ring has been freed, and a
    /// line left at the points it had when the ring went is a line claiming the spacecraft
    /// stopped sending. [`Action::RemoveFromPlot`] is the action that keeps a parameter
    /// watched; this is its dual and takes everything.
    fn unwatch(&mut self, parameter: ParamId) {
        let name = self.qualified(parameter);
        self.edit_store(|store| store.unwatch(parameter));
        self.layout.watched.retain(|held| *held != name);
        for plot in 0..self.layout.plots.len() {
            self.layout.remove_from_plot(plot, &name);
        }
        self.relayout();
    }

    /// Draws a parameter in an existing plot, watching it if it was not.
    fn add_to_plot(&mut self, parameter: ParamId, plot: usize) {
        let name = self.qualified(parameter);
        if name.is_empty() {
            return;
        }
        self.layout.add_to_plot(plot, name);
        // A plotted parameter with no ring draws an empty box, and an operator who plotted
        // something and got one would reasonably conclude the telemetry is missing.
        self.watch(parameter);
        self.relayout();
    }

    /// Stops drawing a parameter in a plot. It stays watched.
    fn remove_from_plot(&mut self, parameter: ParamId, plot: usize) {
        let name = self.qualified(parameter);
        self.layout.remove_from_plot(plot, &name);
        self.relayout();
    }

    /// Adds a plot holding one parameter.
    fn new_plot(&mut self, parameter: ParamId) {
        let name = self.qualified(parameter);
        if name.is_empty() {
            return;
        }
        // The leaf name for the title, the qualified one for the parameter: a title reading
        // `/Mission/Payload/Thermal/TEMP_A` is all path and no name, and a saved qualified
        // name is what survives the definition being rebuilt.
        let title = crate::fmt::name_of(&self.db, parameter).to_owned();
        let _index = self.layout.new_plot(title, name);
        self.watch(parameter);
        self.relayout();
    }

    /// Changes how much history every watched parameter keeps.
    fn set_history_depth(&mut self, depth: usize) {
        // Clamped rather than refused, so that the control which asked shows what it got.
        let depth = depth.max(MIN_HISTORY_DEPTH);
        self.edit_store(|store| store.set_depth(depth));
        self.layout.history_depth = depth;
        self.layout_dirty = true;
    }

    /// Drops every line of the log, and this frame's copy of it.
    fn clear_events(&mut self) {
        {
            let mut events = self
                .session
                .events()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            events.clear();
        }
        // And the panel's copy, whose `total` is what decides whether the next refresh copies
        // anything at all. Leaving it would show the cleared lines until the next event.
        self.events.clear();
    }

    /// Rebuilds the series from the layout and marks the layout for saving.
    fn relayout(&mut self) {
        self.plots.sync(&self.layout, &self.db);
        self.layout_dirty = true;
    }

    /// Writes the layout next to the definition.
    ///
    /// Called when the window is closing, and when the layout changed and the operator has
    /// let go of the pointer — not on every change. A save per frame of a drag is a file
    /// written sixty times a second.
    ///
    /// A failure is one `Event::warning` on the log and nothing more. A definition directory
    /// that is read-only must not end a pass, and `layout_dirty` is cleared either way so
    /// that the same line is not written again on the next frame, and the next, and the next.
    pub fn save_layout(&mut self) {
        {
            let store = self
                .session
                .store()
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            self.layout.capture(&self.db, &store);
        }
        if let Err(error) = self.layout.save(&self.session.config().definition) {
            log(
                self.session.events(),
                Event::warning("session", format!("the layout was not saved: {error}")),
            );
        }
        self.layout_dirty = false;
    }
}

// TODO(gs-gui-app): nothing in this module is tested against a running session — `refresh`,
// `apply`, `ui` and `save_layout` are covered only by the free functions they lean on. They
// need a `Session`, and `xtce_gs_engine::Session` has no constructor that does not spawn
// tasks and read a definition off disk (its own `Session::idle` is `#[cfg(test)]` and so
// stops at that crate's edge). Deciding this means deciding what `App` holds: a public
// `Session::idle`-shaped constructor, or a trait over the four accessors `App` actually uses
// — `store`, `stats`, `events`, `limits` — with `Session` as its only implementation. The
// second is testable and costs a vtable on four calls per frame; the first is cheaper and
// widens the engine's API for the interface's benefit.
/// What the log is told about a layout that has just been restored.
///
/// Split out so that the counting is testable without a window: the case worth getting right
/// is the one where a definition was edited between two runs, and the operator has to be able
/// to tell "nothing was saved" from "what was saved is no longer declared".
fn restore_report(resolved: usize, named: usize) -> Event {
    if resolved == named {
        Event::info(
            "session",
            format!("layout: watching {resolved} of {named} saved parameters"),
        )
    } else {
        Event::warning(
            "session",
            format!(
                "layout: {resolved} of {named} saved parameters are in this definition; the \
                 other {} are kept in the layout in case it comes back",
                named - resolved
            ),
        )
    }
}

impl eframe::App for App {
    /// Copies the frame out of the session. Draws nothing — the trait forbids it, and the
    /// point of the split is that this half also runs when the window is hidden and the
    /// waker fired.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Any event at all — a keystroke in a filter box, a click on a column header, a drag
        // on a plot — changes what a panel will want out of the store, and `Table` carries no
        // dirty flag by design: its filter is applied while its rows are being copied.
        if ctx.input(|input| !input.events.is_empty()) {
            self.input_passes = INPUT_REFRESH_PASSES;
        }
        self.refresh();
        if self.input_passes > 0 {
            // The pass that *applies* an input is the one after it arrived. Ask for it now
            // rather than making the operator wait out the idle second below.
            ctx.request_repaint();
        }
        // The clock, the ages and the live indicator move on their own.
        ctx.request_repaint_after_secs(IDLE_REPAINT_SECONDS);
    }

    /// Lays the panels out over the buffers [`App::refresh`] filled. Computes nothing.
    ///
    /// The order is the nesting: the first panel added is the outermost, and a central panel
    /// must be last. In egui 0.35 a panel is shown *inside* a [`egui::Ui`] rather than opened
    /// on the context — `show_inside` is the deprecated spelling of the same call.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Utc::now();
        // Frames are what the coding counters count. A packet link leaves them at zero for
        // the length of the pass, and a row of zeroes that can never move teaches the
        // operator to skip the row.
        let framed = self.session.config().pipeline.codeword_length().is_some();

        let mut action = egui::Panel::top("status")
            .show(ui, |ui| {
                status::show(
                    ui,
                    &self.status,
                    &self.session.describe_source(),
                    framed,
                    &self.layout,
                    self.view.paused,
                    &mut self.scratch,
                )
            })
            .inner;

        action = action.or(egui::Panel::left("tree")
            .default_size(self.layout.tree_width)
            .show(ui, |ui| {
                tree::show(ui, &mut self.tree, &self.db, now, self.layout.plots.len())
            })
            .inner);

        action = action.or(egui::Panel::bottom("events")
            .resizable(true)
            .default_size(self.layout.events_height)
            .show(ui, |ui| {
                events::show(ui, &mut self.events, &mut self.scratch)
            })
            .inner);

        action = action.or(egui::CentralPanel::default()
            .show(ui, |ui| {
                let split = ui.available_height() * TABLE_SHARE;
                let rows = egui::Panel::top("table")
                    .resizable(true)
                    .default_size(split)
                    .show(ui, |ui| {
                        table::show(
                            ui,
                            &mut self.table,
                            &self.db,
                            now,
                            self.layout.show_raw,
                            self.layout.plots.len(),
                            &mut self.scratch,
                        )
                    })
                    .inner;
                rows.or(plots::show(
                    ui,
                    &self.plots,
                    &mut self.view,
                    &self.layout,
                    &self.db,
                    self.session.limits(),
                    &mut self.scratch,
                ))
            })
            .inner);

        // Once, after the last panel. Applying inside one would take the write lock in the
        // middle of a draw, and changing the layout while a later panel still holds an index
        // into it is how a plot index goes out of range for exactly one frame.
        self.apply(ui.ctx(), action);

        // This build has no `eframe` persistence, so `App::save` is never called and a
        // closing window offers no other hook. Written when the operator has let go — a save
        // per frame of a drag is a file written sixty times a second — and without waiting
        // for that on the way out, where there is no next frame to be settled in.
        let (closing, settled) = ui.ctx().input(|input| {
            (
                input.viewport().close_requested(),
                !input.pointer.any_down(),
            )
        });
        if self.layout_dirty && (closing || settled) {
            self.save_layout();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use xtce_gs_core::Severity;

    fn view(x_range: Option<[f64; 2]>, width: f32, paused: bool) -> View {
        View {
            x_range,
            paused,
            width,
        }
    }

    #[test]
    fn an_axis_that_has_not_moved_is_not_a_reason_to_decimate_again() {
        let drawn = view(Some([100.0, 200.0]), 800.0, false);
        assert!(same_view(drawn, drawn));
        assert!(same_range(None, None));
    }

    #[test]
    fn a_panned_axis_is_a_reason_to_decimate_again() {
        let drawn = view(Some([100.0, 200.0]), 800.0, false);
        assert!(!same_view(view(Some([101.0, 201.0]), 800.0, false), drawn));
        // The first frame: no range yet, then one.
        assert!(!same_view(view(None, 800.0, false), drawn));
    }

    #[test]
    fn a_resized_window_is_a_reason_to_decimate_again() {
        // The budget is the plot's width in points; a window dragged wider moves neither the
        // store's generation nor any dirty flag.
        let drawn = view(Some([100.0, 200.0]), 800.0, false);
        assert!(!same_view(view(Some([100.0, 200.0]), 1200.0, false), drawn));
    }

    #[test]
    fn pausing_is_a_reason_to_redraw_even_with_the_same_axis() {
        let drawn = view(Some([100.0, 200.0]), 800.0, false);
        assert!(!same_view(view(Some([100.0, 200.0]), 800.0, true), drawn));
    }

    #[test]
    fn an_empty_plots_nan_bounds_do_not_decimate_forever() {
        // `egui_plot` hands back NaN bounds for a plot with nothing in it, and `NaN != NaN`
        // would make every frame of an empty station a frame that walks every ring.
        let empty = view(Some([f64::NAN, f64::NAN]), 800.0, false);
        assert!(same_view(empty, empty));
        assert!(!same_view(view(Some([0.0, 1.0]), 800.0, false), empty));
    }

    #[test]
    fn zero_and_negative_zero_are_different_axes() {
        // The point of comparing bits: this is the one place where it differs from `==`, and
        // it costs a single extra decimation on a plot that crossed zero.
        assert!(!same_range(Some([0.0, 1.0]), Some([-0.0, 1.0])));
    }

    #[test]
    fn a_plotted_parameter_that_was_not_watched_becomes_watched() {
        let mut layout = Layout::default();
        let plot = layout.new_plot("VOLTS", "/Sat/VOLTS");
        layout.add_to_plot(plot, "/Sat/AMPS");
        assert!(layout.watched.is_empty(), "the fixture starts hand-edited");

        watch_what_is_plotted(&mut layout);
        assert_eq!(layout.watched, ["/Sat/VOLTS", "/Sat/AMPS"]);
    }

    #[test]
    fn a_parameter_already_watched_is_not_watched_twice() {
        let mut layout = Layout::default();
        layout.watched.push("/Sat/VOLTS".to_owned());
        layout.new_plot("VOLTS", "/Sat/VOLTS");
        watch_what_is_plotted(&mut layout);
        assert_eq!(layout.watched, ["/Sat/VOLTS"]);
    }

    #[test]
    fn a_layout_with_no_plots_is_left_alone() {
        let mut layout = Layout::default();
        layout.watched.push("/Sat/VOLTS".to_owned());
        watch_what_is_plotted(&mut layout);
        assert_eq!(layout.watched, ["/Sat/VOLTS"]);
    }

    #[test]
    fn a_layout_whose_names_all_resolve_is_reported_as_information() {
        let event = restore_report(12, 12);
        assert_eq!(event.severity, Severity::Info);
        assert!(event.message.contains("12"), "{}", event.message);
    }

    #[test]
    fn a_definition_that_lost_a_parameter_is_reported_as_a_warning() {
        // The case this exists for: the XML was edited between two runs, and the operator has
        // to be able to tell "nothing was saved" from "what was saved is no longer declared".
        let event = restore_report(9, 12);
        assert_eq!(event.severity, Severity::Warning);
        assert!(event.message.contains('3'), "{}", event.message);
    }

    #[test]
    fn a_layout_that_resolved_nothing_is_still_a_warning_and_not_silence() {
        let event = restore_report(0, 4);
        assert_eq!(event.severity, Severity::Warning);
        assert!(event.message.contains('4'), "{}", event.message);
    }
}
