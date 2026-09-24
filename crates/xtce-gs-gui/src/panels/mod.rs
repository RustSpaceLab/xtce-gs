//! The five panels, and the one thing they are allowed to change.
//!
//! Every panel is drawn by a free `show` function that takes a [`egui::Ui`], its own state,
//! and read-only data — and returns an [`Action`]. A panel never touches the session, never
//! takes a lock, and never mutates the store. That is not tidiness: the store sits behind one
//! `RwLock` shared with the decode thread, and a guard held across a draw is a decode thread
//! blocked for a frame. So the shape is fixed in two halves:
//!
//! * `refresh` — takes the guard, copies out the little that will be drawn, drops the guard.
//!   These are the *only* signatures in this crate that name [`xtce_gs_core::ParameterStore`],
//!   [`xtce_gs_core::EventLog`] or [`xtce_gs_core::LinkStats`].
//! * `show` — draws from the copy and returns what the operator asked for.
//!
//! [`crate::App`] then applies the action once, after the frame, in the one place that takes
//! the write lock. A panel that mutated the store directly would also be a panel that decides
//! when the write lock is taken, from inside a draw, several times a frame.

pub mod events;
pub mod plots;
pub mod status;
pub mod table;
pub mod tree;

use xtce_model::ParamId;

use crate::layout::Theme;

/// What a panel asks the application to do once the frame is drawn.
///
/// One enum for every panel rather than one per panel: the actions overlap — the tree, the
/// table and a plot legend can all ask for the same unwatch — and three enums that had to be
/// merged in [`crate::App`] anyway would only move the match.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Action {
    /// Nothing happened this frame. The common case, sixty times a second.
    #[default]
    None,
    /// Show this parameter's detail, without changing what is kept.
    Select(ParamId),
    /// Start keeping history for a parameter.
    Watch(ParamId),
    /// Stop keeping history for a parameter and free its ring.
    Unwatch(ParamId),
    /// Draw a parameter in an existing plot.
    AddToPlot {
        /// The parameter to draw.
        parameter: ParamId,
        /// Index into [`crate::Layout::plots`].
        plot: usize,
    },
    /// Stop drawing a parameter in a plot. It stays watched — see [`crate::Layout::remove_plot`].
    RemoveFromPlot {
        /// The parameter to stop drawing.
        parameter: ParamId,
        /// Index into [`crate::Layout::plots`].
        plot: usize,
    },
    /// Add a plot holding one parameter.
    NewPlot(ParamId),
    /// Remove a plot. Its parameters stay watched.
    RemovePlot(usize),
    /// Change how many points each watched parameter keeps.
    SetHistoryDepth(usize),
    /// Freeze the plots' x axis at what is on screen, or let it follow again.
    SetPaused(bool),
    /// Change the colours.
    SetTheme(Theme),
    /// Drop every line of the event log.
    ClearEvents,
    /// Shut the session down and close the window.
    Quit,
}

impl Action {
    /// Whether nothing was asked for.
    #[must_use]
    pub const fn is_none(self) -> bool {
        matches!(self, Self::None)
    }

    /// This action if there is one, otherwise the other.
    ///
    /// A panel draws dozens of widgets and at most one of them is clicked in a frame, so it
    /// folds their results with this rather than carrying a `mut` accumulator through every
    /// closure. First one wins: two clicks in one frame is not a thing a pointer does.
    #[must_use]
    pub const fn or(self, other: Self) -> Self {
        if self.is_none() { other } else { self }
    }
}
