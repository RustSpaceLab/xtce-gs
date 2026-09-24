//! The operator interface: a window onto one session.
//!
//! Everything here reads. The session decodes telemetry into an
//! [`xtce_gs_core::ParameterStore`] on its own threads; this crate takes the read lock once a
//! frame, copies out the few hundred values that are on screen, drops the lock and draws. The
//! only thing it writes back is what the operator decided — which parameters are watched,
//! which are plotted, how deep the history goes — and that goes through one function, once a
//! frame, under the write lock.
//!
//! ```text
//! run(config)
//!   ├── tokio::runtime::Builder — the reactor the session's tasks live on
//!   ├── Layout::load            — next to the definition, before the window exists
//!   └── eframe::run_native
//!         └── AppCreator: Session::start(config, runtime.handle(), waker)
//!                waker = Arc::new(move || ctx.request_repaint())
//! ```
//!
//! # What this crate refuses
//!
//! * **It does not decode.** Nothing here parses a packet or reads a bit. It draws what the
//!   engine filed, and a value it cannot render is a value it says it cannot render.
//! * **It does not hold a lock across a draw.** The two halves — `refresh` under the guard,
//!   `show` after it — are why no `show` signature in this crate names a store, a log or a
//!   set of counters. See [`panels`].
//! * **It does not repaint on a timer.** egui redraws when something asks it to, so the
//!   session's waker is `egui::Context::request_repaint` and an idle station repaints once a
//!   second for the clock rather than sixty times for nothing.
//! * **It does not decide what the telemetry means.** A limit state colours a cell; it does
//!   not pop up, sound, or acknowledge. A station that interrupted the operator would be a
//!   station they mute.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// Widget sizes are `f32`, counters are `u64`, and every number drawn here has to become one
// or the other. The casts are narrowing on paper and not in fact: a pixel count fits a
// `f32` with room to spare, and a row index that did not would be a row nobody could scroll to.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
// Tests are allowed to assert loudly; the no-panic rule is about a window that must stay up
// for the length of a pass, not about test setup.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )
)]

pub mod app;
pub mod error;
pub mod fmt;
pub mod layout;
pub mod panels;

pub use app::App;
pub use error::GuiError;
pub use layout::{Discarded, Layout, PlotLayout, Theme};

use std::sync::{Arc, OnceLock};

use xtce_gs_engine::{Session, SessionConfig, Waker};

/// Worker threads the reactor is built with.
///
/// Two. One session reads one socket and writes one recording; the decoder is a plain thread
/// of its own and not a task. A runtime sized to the machine would spawn a worker per core to
/// have them all park on the same socket.
pub const RUNTIME_WORKER_THREADS: usize = 2;

/// The application identifier the window is registered under.
///
/// What a Wayland compositor matches a window against its desktop entry with, and what
/// `eframe` falls back to for a title. The reverse-DNS form a desktop entry would want is
/// deliberately not used: this is not installed, and an id that claims a domain nothing
/// serves is an id that collides with the one that eventually is.
///
/// The same characters as [`app::WINDOW_TITLE`] today and deliberately not the same constant:
/// a title is what the operator reads and may be changed for them, while an id is what the
/// desktop matches on and changing it orphans every pinned launcher and saved window rule.
/// They are free to drift, and the day they do this is the one that must not move.
pub const APP_ID: &str = "xtce-gs";

/// Width of the window when nothing has been saved, in points.
pub const DEFAULT_WINDOW_SIZE: [f32; 2] = [1280.0, 800.0];

/// The window the station opens.
///
/// Nothing but the viewport is set: the renderer is whatever the `glow` feature gives, and
/// persistence is off in this build — see [`layout`] for what stands in for it.
#[must_use]
pub fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(app::WINDOW_TITLE)
            .with_inner_size(DEFAULT_WINDOW_SIZE)
            .with_app_id(APP_ID),
        ..eframe::NativeOptions::default()
    }
}

/// Where the engine's waker finds the window.
///
/// The engine is handed a [`Waker`] — `Arc<dyn Fn() + Send + Sync>` — before [`eframe`] has
/// built anything, because the session is started before the window so that a session which
/// will not start can be *shown* rather than only printed. But the thing the waker has to
/// call is [`egui::Context::request_repaint`], and the context does not exist until `eframe`
/// runs the creation closure. So the waker holds this instead, and reads it every time.
///
/// # Why it is not a race
///
/// Three separate reasons, and all three are needed:
///
/// * **The slot itself is safe.** `OnceLock` publishes the context with release ordering and
///   every `get` acquires it, so no thread can observe a half-built `egui::Context`.
///   Filling it twice is refused rather than racing, and the refusal is ignored — the first
///   window is the one there is.
/// * **A call that lands before the fill is dropped, and nothing is lost by it.** The window
///   has not drawn a frame yet; when it draws its first, [`App::refresh`] reads the store's
///   *current* generation, which already includes every batch that arrived during the gap.
///   A repaint request is a request to look again, not a delivery of anything.
/// * **A call that lands after the fill cannot arrive too late.** `request_repaint` is
///   itself thread-safe and coalescing: many calls between two frames produce one frame.
///
/// The one thing this cannot do is wake a window that was never opened, which is the same
/// thing as not needing to.
pub struct RepaintSlot {
    context: OnceLock<egui::Context>,
}

impl Default for RepaintSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl RepaintSlot {
    /// An empty slot, for a window that does not exist yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            context: OnceLock::new(),
        }
    }

    /// Hands the slot the window's context. The first call wins.
    ///
    /// Called from `eframe`'s creation closure, on the interface thread, once. A second call
    /// is ignored rather than refused loudly: overwriting would point the waker at a context
    /// whose window may already be closing, and there is no correct repair for a second root
    /// window that this crate never asks for.
    pub fn fill(&self, context: egui::Context) {
        let _already_filled = self.context.set(context);
    }

    /// Whether the window's context has arrived.
    #[must_use]
    pub fn is_filled(&self) -> bool {
        self.context.get().is_some()
    }

    /// Asks the window to repaint, if there is one yet.
    ///
    /// Called from the decode thread after every batch. Cheap enough to call per batch: an
    /// atomic load, and `request_repaint` short-circuits once a repaint is already pending.
    pub fn wake(&self) {
        if let Some(context) = self.context.get() {
            context.request_repaint();
        }
    }
}

/// The window a session that would not start puts up.
///
/// A station launched from a desktop entry has no terminal, so a configuration error that
/// only went to `stderr` is a process that flashes and disappears. [`run`] still returns the
/// error — the command line prints it — and this is what the operator sees in the meantime.
struct Failure {
    message: String,
}

impl eframe::App for Failure {
    /// One panel, one message, and no session behind it.
    ///
    /// No [`eframe::App::logic`] and no repaint on a timer: there is nothing here that moves
    /// on its own. Input still draws — `egui_winit` answers a pointer event with
    /// `EventResponse { repaint: true, .. }`, which `eframe` turns into the next pass — and
    /// the window's own close button does not go through egui at all, so a station cannot
    /// get stuck on its own error dialog.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("The session did not start");
            ui.add_space(8.0);
            // Selectable, because the next thing that happens to this text is that it is
            // pasted into a message to whoever wrote the definition.
            ui.add(
                egui::Label::new(egui::RichText::new(self.message.as_str()).monospace())
                    .wrap()
                    .selectable(true),
            );
            ui.add_space(8.0);
            ui.label("Fix it and start the station again.");
            ui.add_space(8.0);
            if ui.button("Close").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

/// Opens the window and runs until it closes.
///
/// In this order, and the order is the contract:
///
///  1. [`Layout::load`], before anything is opened. A layout that will not parse is
///     something the operator edited, and they must be told about it with a terminal still
///     in front of them.
///  2. The reactor, as a *local* and not a field of [`App`]. `Runtime::drop` waits for what
///     is on it, and a runtime dropped as a field beside the session would wait on tasks the
///     session has not been told to stop yet. As a local it outlives `run_native`, which
///     returns only after the window is closed and `App` — and with it the [`Session`],
///     whose `Drop` asks both tasks to stop — is gone.
///  3. [`Session::start`], *before* `run_native` and not inside its creation closure. The
///     waker it takes cannot name a context that does not exist yet, so it reads a
///     [`RepaintSlot`] the creation closure fills; see that type for why the gap is not a
///     race. Starting here rather than in the closure is what makes a session that will not
///     start something the operator can be *shown*: inside the closure the only thing a
///     failure can do is abort the window.
///  4. `eframe::run_native`, with either the station or — when the session refused to start
///     — a window carrying the reason.
///
/// A definition that takes two seconds to parse shows nothing for two seconds either way.
/// The alternative is a window that opens and then closes.
///
/// # Errors
///
/// [`GuiError::Layout`] for a saved layout that will not parse, [`GuiError::Io`] for a
/// reactor that will not build, [`GuiError::Engine`] for a session that will not start — and
/// that one is returned *after* the operator has closed the window that said so — and
/// [`GuiError::Window`] for a platform that will not give the process a window.
///
/// The two cannot both be reported: when the session fails to start *and* the window that
/// would have said so cannot be opened, the session error is what comes back. It is the one
/// the operator has to fix, and a machine with no display is a machine where this is being
/// read off a terminal anyway.
pub fn run(config: SessionConfig) -> Result<(), GuiError> {
    let Discarded { layout, note } = Layout::load(&config.definition)?;
    if let Some(note) = note {
        // Before the window, deliberately: this is the one moment the operator still has a
        // terminal in front of them, and the message is about a file they did not write.
        eprintln!("{note}");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKER_THREADS)
        .thread_name(APP_ID)
        .enable_all()
        .build()?;

    let slot = Arc::new(RepaintSlot::new());
    let waker: Waker = {
        let slot = Arc::clone(&slot);
        Arc::new(move || slot.wake())
    };

    let session = match Session::start(config, runtime.handle(), Some(waker)) {
        Ok(session) => session,
        Err(error) => {
            let message = error.to_string();
            // A window failure on top of a session failure is deliberately dropped: the
            // session is what the operator has to fix, and a platform that will not open a
            // window is one where this error lands on a terminal anyway.
            let _no_window = eframe::run_native(
                app::WINDOW_TITLE,
                native_options(),
                Box::new(|_cc| Ok(Box::new(Failure { message }))),
            );
            return Err(GuiError::Engine(error));
        }
    };

    let window = eframe::run_native(
        app::WINDOW_TITLE,
        native_options(),
        Box::new(move |cc| {
            // The waker has been live since `Session::start`; every call it made before this
            // line did nothing, and nothing was lost by that — see [`RepaintSlot`].
            slot.fill(cc.egui_ctx.clone());
            Ok(Box::new(App::new(cc, session, layout)))
        }),
    );

    // Explicitly, and here: `run_native` has returned, so `App` — and with it the `Session`,
    // whose `Drop` asks both tasks to stop — is gone, and `Runtime::drop` has nothing left to
    // wait for. Dropping it any earlier would wait on tasks nothing had asked to stop.
    drop(runtime);
    window?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context that has drawn, and so is not asking for anything.
    ///
    /// A fresh [`egui::Context`] starts with an outstanding repaint of its own — egui runs a
    /// couple of passes at the start on purpose — so asking a brand new one whether a
    /// repaint has been requested answers yes before anybody has requested one. Running
    /// passes until it settles is what makes `has_requested_repaint` mean what these tests
    /// read it as.
    fn settled_context() -> egui::Context {
        let context = egui::Context::default();
        for _ in 0..8 {
            if !context.has_requested_repaint() {
                return context;
            }
            let _output = context.run_ui(egui::RawInput::default(), |_ui| {});
        }
        context
    }

    /// The waker crosses to the decode thread, so the slot behind it has to be `Sync` and
    /// the context inside it `Send`. Stated here rather than discovered as the error message
    /// that `Arc<dyn Fn() + Send + Sync>` produces at the `Session::start` call.
    const fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn the_slot_can_be_shared_with_the_decode_thread() {
        assert_send_sync::<RepaintSlot>();
        assert_send_sync::<egui::Context>();
    }

    #[test]
    fn a_wake_before_the_window_exists_does_nothing() {
        let slot = RepaintSlot::new();
        assert!(!slot.is_filled());
        // The engine calls this after every batch from the moment `Session::start` returns,
        // which is before `eframe` has built anything. It must be a no-op, not a panic.
        slot.wake();
        slot.wake();
        assert!(!slot.is_filled());
    }

    #[test]
    fn a_wake_after_the_window_exists_asks_for_a_repaint() {
        let context = settled_context();
        assert!(
            !context.has_requested_repaint(),
            "the fixture has to start quiet"
        );

        let slot = RepaintSlot::new();
        slot.fill(context.clone());
        assert!(slot.is_filled());
        slot.wake();
        assert!(context.has_requested_repaint());
    }

    #[test]
    fn filling_the_slot_twice_keeps_the_first_window() {
        let first = settled_context();
        let second = settled_context();
        let slot = RepaintSlot::new();
        slot.fill(first.clone());
        slot.fill(second.clone());
        slot.wake();

        assert!(first.has_requested_repaint());
        assert!(
            !second.has_requested_repaint(),
            "a second fill must not move the waker to another window"
        );
    }

    #[test]
    fn the_waker_the_engine_is_given_reads_the_slot_and_not_a_context() {
        let slot = Arc::new(RepaintSlot::new());
        let waker: Waker = {
            let slot = Arc::clone(&slot);
            Arc::new(move || slot.wake())
        };
        // Built and called before there is a window, exactly as `run` hands it over.
        waker();

        let context = settled_context();
        slot.fill(context.clone());
        waker();
        assert!(context.has_requested_repaint());
    }

    #[test]
    fn the_window_is_named_and_sized_before_anything_is_saved() {
        let options = native_options();
        assert_eq!(
            options.viewport.title.as_deref(),
            Some(app::WINDOW_TITLE),
            "a Wayland compositor with no title falls back to the app id"
        );
        assert_eq!(options.viewport.app_id.as_deref(), Some(APP_ID));
        assert_eq!(
            options.viewport.inner_size,
            Some(egui::Vec2::from(DEFAULT_WINDOW_SIZE))
        );
    }
}
