//! What the interface can refuse to do.
//!
//! Four variants, and three of them happen before the window exists: a session that will not
//! start, a runtime that will not build, a window the platform will not open. Everything that
//! goes wrong *while* the window is up — a packet the definition does not describe, a source
//! that ended, a layout that cannot be written — is a line in the event log, because the one
//! thing an operator cannot use is a ground station that exits mid-pass.

/// The interface could not be started, or could not save what it was told to save.
#[derive(Debug, thiserror::Error)]
pub enum GuiError {
    /// The session would not start: the definition, the configuration, or a limits file.
    #[error("{0}")]
    Engine(#[from] xtce_gs_engine::EngineError),

    /// The window could not be opened, or the event loop failed.
    ///
    /// This carries `eframe`'s own error rather than a rendering of it, because the causes —
    /// no display, no GL context, a compositor that refused — are what the operator has to
    /// act on, and a flattened string loses the chain that names them.
    #[error("{0}")]
    Window(#[from] eframe::Error),

    /// A file the interface owns said no: the saved layout, or the runtime it builds.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// The saved layout could not be parsed or rendered.
    ///
    /// `serde_json::Error` is deliberately not a variant of its own: a layout that will not
    /// parse is a file someone edited, and the message carries the path, the line and the
    /// column for exactly that reason.
    #[error("{0}")]
    Layout(String),
}

impl GuiError {
    /// A layout failure, naming the file and what was wrong with it.
    ///
    /// Every caller writes the same `GuiError::Layout(format!(...))` without it.
    #[must_use]
    pub fn layout(message: impl Into<String>) -> Self {
        Self::Layout(message.into())
    }
}
