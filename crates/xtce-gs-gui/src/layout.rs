//! What survives a restart.
//!
//! An operator who spent a pass arranging six plots and a watch list should not arrange them
//! again after a crash or a definition reload. What is saved is deliberately small: which
//! parameters are watched, which plots exist and what is in them, how deep the history goes,
//! and the theme. Not the window size, not the scroll position, not the last value of
//! anything — a layout is what the operator decided, and the rest is what the session did.
//!
//! # Why a file and not the eframe storage
//!
//! `eframe`'s storage — `App::save`, `eframe::get_value` — is behind its `persistence` and
//! `ron` features, and this workspace builds `eframe` with `default-features = false`. So
//! `App::save` is never called here and those helpers do not exist. The layout is therefore a
//! JSON file this crate writes itself, next to the definition, which has the better property
//! anyway: a layout belongs to a *mission*, not to a machine, and two operators sharing a
//! definition directory share the plots that go with it.
//!
//! # Why names and not indices
//!
//! Every parameter is stored as its qualified name. A [`xtce_model::ParamId`] is an index into
//! one build of one definition; add a parameter to the XML and every index after it moves. A
//! layout that referred to indices would silently plot the wrong parameter after an edit,
//! which is the failure nobody checks for because the plot still draws.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use xtce_gs_core::ParameterStore;
use xtce_model::XtceDb;

use crate::error::GuiError;

/// The version this crate writes, and the only one it reads.
///
/// Bumped when a field changes meaning rather than when one is added: `serde(default)` takes
/// care of a field that is merely new, and refusing an old file for that would throw away a
/// layout over a field the operator never set.
pub const LAYOUT_VERSION: u32 = 1;

/// What the layout file is called, appended to the definition's file name.
///
/// Appended rather than substituted for the extension, so that `mission.xml` and
/// `mission.xtce` in one directory do not share — and overwrite — one layout.
pub const LAYOUT_SUFFIX: &str = ".gs-layout.json";

/// Points of history a new layout asks for.
///
/// Matches [`xtce_gs_engine::config::DEFAULT_HISTORY_DEPTH`]; stated again here because this
/// value is written into a file that outlives the constant.
pub const DEFAULT_HISTORY_DEPTH: usize = 4096;

/// How the interface is coloured.
///
/// Three states and not a boolean: an operator in a dark control room and one at a desk by a
/// window want opposite things, and `System` is what a laptop that switches at sunset needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the desktop.
    #[default]
    System,
    /// Always dark.
    Dark,
    /// Always light.
    Light,
}

impl Theme {
    /// The egui preference this maps to.
    #[must_use]
    pub const fn preference(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Dark => egui::ThemePreference::Dark,
            Self::Light => egui::ThemePreference::Light,
        }
    }
}

/// One plot: a title, the parameters drawn in it, and how tall it is.
///
/// A plot holds several parameters because that is how a relationship is read — a current
/// against a voltage, three axes of a magnetometer — and the alternative, one parameter per
/// plot, puts the operator's eye on the wrong axis to see it.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlotLayout {
    /// What the plot is called, shown above it.
    pub title: String,
    /// Qualified parameter names, in the order they were added.
    pub parameters: Vec<String>,
    /// Height in points. The width is whatever the panel gives it.
    pub height: f32,
    /// Whether the y axis follows the data.
    pub autoscale: bool,
    /// The y range when it does not, as `[min, max]`.
    pub y_range: Option<[f64; 2]>,
}

impl Default for PlotLayout {
    /// An empty autoscaled plot of the default height.
    fn default() -> Self {
        Self {
            title: String::new(),
            parameters: Vec::new(),
            height: crate::panels::plots::DEFAULT_HEIGHT,
            autoscale: true,
            y_range: None,
        }
    }
}

impl PlotLayout {
    /// A named, empty plot.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// Whether a qualified name is already drawn in this plot.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.parameters.iter().any(|held| held == name)
    }
}

/// Everything the interface remembers between runs.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    /// The format version — see [`LAYOUT_VERSION`].
    pub version: u32,
    /// Qualified names of the parameters kept in history.
    ///
    /// Every plotted parameter is also in here. A parameter can be watched without being
    /// plotted — that is a table row with a history behind it, ready to plot without waiting
    /// for a pass to refill.
    pub watched: Vec<String>,
    /// The plots, top to bottom.
    pub plots: Vec<PlotLayout>,
    /// Points of history per watched parameter.
    pub history_depth: usize,
    /// How the interface is coloured.
    pub theme: Theme,
    /// Whether the table shows the raw value beside the engineering one.
    pub show_raw: bool,
    /// Width of the parameter list, in points.
    pub tree_width: f32,
    /// Height of the event log, in points.
    pub events_height: f32,
}

// TODO(gs-gui-layout): `tree_width` and `events_height` are written into
// `egui::Panel::default_size` and never read back, so a panel the operator drags wider is a
// panel that is narrow again next time. Capturing them means reading egui's own
// `PanelState` — `egui::Panel` stores the dragged size in `Memory` under the panel's `Id`,
// and there is no public accessor in 0.35 — or keeping the size the interface asked for and
// giving up on the drag. Decide which, because a field that looks saved and is not is worse
// than one that is missing.

impl Default for Layout {
    /// A station with nothing watched, nothing plotted, and the system theme.
    ///
    /// Deliberately empty rather than seeded with the first few parameters: a definition's
    /// arena order is XML order, which is not an order anybody chose to look at.
    fn default() -> Self {
        Self {
            version: LAYOUT_VERSION,
            watched: Vec::new(),
            plots: Vec::new(),
            history_depth: DEFAULT_HISTORY_DEPTH,
            theme: Theme::System,
            show_raw: false,
            tree_width: 260.0,
            events_height: 140.0,
        }
    }
}

/// What [`Layout::load`] read, and what it had to throw away to get it.
///
/// A note is not an error: the station starts either way. It exists because discarding an
/// operator's arrangement without saying so is the kind of silence that gets blamed on the
/// wrong thing three sessions later.
#[derive(Clone, Debug, PartialEq)]
pub struct Discarded {
    /// The layout to use.
    pub layout: Layout,
    /// What was discarded, if anything, ready to print.
    pub note: Option<String>,
}

impl Layout {
    /// Where the layout for a definition is kept.
    ///
    /// The definition's own path with [`LAYOUT_SUFFIX`] appended. A definition in a read-only
    /// directory therefore fails to *save* and not to load, which is the right way round: the
    /// session runs, and the operator is told once that the arrangement will not persist.
    #[must_use]
    pub fn path_for(definition: &Path) -> PathBuf {
        // Appended to the whole path, extension included: `mission.xml` and `mission.xtce`
        // side by side are two definitions, and a suffix that replaced the extension would
        // give them one layout between them.
        let mut name = definition.as_os_str().to_os_string();
        name.push(LAYOUT_SUFFIX);
        PathBuf::from(name)
    }

    /// Reads the layout that belongs to a definition, or the default when there is none.
    ///
    /// A missing file is the default and not an error: a first run has no layout, and a
    /// station that refused to start for that would be unusable exactly once per mission. A
    /// file that exists and does not parse *is* an error, because that is an edit someone
    /// made — replacing it with the default would destroy it on the next save, and the next
    /// save is one click away.
    ///
    /// # Errors
    ///
    /// [`GuiError::Io`] when the file exists and cannot be read, [`GuiError::Layout`] when
    /// its JSON does not parse or its version is not [`LAYOUT_VERSION`]. Both name the path:
    /// the operator's next move is to open, move or delete that file, and an error that
    /// makes them guess which file it meant is an error they act on twice.
    pub fn load(definition: &Path) -> Result<Discarded, GuiError> {
        let path = Self::path_for(definition);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Discarded {
                    layout: Self::default(),
                    note: None,
                });
            }
            // Everything else — a directory in the way, a permission, a device — is real, and
            // starting from the default would overwrite whatever is there on the next save.
            Err(error) => return Err(GuiError::Io(error)),
        };
        // The version is read before the struct is, because `deny_unknown_fields` rejects a
        // file from a build that had a field this one does not — and it rejects it with a
        // message about a key, not about a version, so the operator would be told the wrong
        // thing about a file they did not write.
        let stated = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64));

        if let Some(version) = stated
            && version != u64::from(LAYOUT_VERSION)
        {
            // Settles TODO(gs-gui-layout): discard, and say so. This file is written by the
            // station itself, so a version bump would otherwise stop every station that had
            // ever been used until its operator deleted a file they never wrote. Migration is
            // the better answer and needs a `Layout` per version; refusing is the wrong one,
            // whatever it costs in an arrangement.
            return Ok(Discarded {
                layout: Self::default(),
                note: Some(format!(
                    "{}: layout version {version} is not {LAYOUT_VERSION}, so it was written                      by another build; starting from the default arrangement and leaving the                      file alone until something is saved over it",
                    path.display()
                )),
            });
        }

        Self::from_json(&text)
            .map(|layout| Discarded { layout, note: None })
            .map_err(|error| {
                GuiError::layout(format!(
                    "{}: {error}; move or delete that file to start from the default layout",
                    path.display()
                ))
            })
    }

    /// Parses a layout, refusing anything this build would misread.
    ///
    /// Strict where [`Layout::load`] is forgiving, and deliberately so: `load` is what the
    /// station calls at start-up, where an old file must cost an arrangement and not a
    /// session, while this is what the tests pin and what a caller reaches for when it wants
    /// to know whether a specific string is a layout. A version other than
    /// [`LAYOUT_VERSION`] is an error here and a discarded file there.
    ///
    /// # Errors
    ///
    /// [`GuiError::Layout`] naming the line and column, or the version that was refused.
    pub fn from_json(text: &str) -> Result<Self, GuiError> {
        // `serde_json::Error` already ends its `Display` in "at line L column C", so calling
        // `line()` and `column()` here would print the position twice. `deny_unknown_fields`
        // on the structs above is what turns a misspelled key into one of these messages
        // instead of a field that quietly stays at its default.
        let layout: Self =
            serde_json::from_str(text).map_err(|error| GuiError::layout(error.to_string()))?;
        if layout.version != LAYOUT_VERSION {
            return Err(GuiError::layout(format!(
                "layout version {} is not {LAYOUT_VERSION}; this file was written by another \
                 build and its fields no longer mean what this one would read them as",
                layout.version
            )));
        }
        Ok(layout)
    }

    /// Renders the layout as the JSON that is saved.
    ///
    /// Pretty-printed because this file is read and edited by hand: an operator copying a
    /// plot between two missions does it in a text editor, not in the interface.
    ///
    /// # Errors
    ///
    /// [`GuiError::Layout`] when the value cannot be serialised. Nothing in [`Layout`] can
    /// fail to serialise today — every field is a number, a string or a `Vec` of those — so
    /// this is the case that exists to stay honest if one grows a map with non-string keys.
    pub fn to_json(&self) -> Result<String, GuiError> {
        serde_json::to_string_pretty(self)
            .map_err(|error| GuiError::layout(format!("the layout will not serialise: {error}")))
    }

    /// Writes the layout next to its definition.
    ///
    /// Written to a scratch file and renamed over the target, because a crash partway
    /// through a plain write leaves a truncated file and [`Layout::load`] refuses those —
    /// losing the arrangement in the one situation where the operator most wants it back.
    ///
    /// # Errors
    ///
    /// [`GuiError::Io`] when the file cannot be written, [`GuiError::Layout`] when the value
    /// cannot be serialised.
    pub fn save(&self, definition: &Path) -> Result<(), GuiError> {
        let text = self.to_json()?;
        let target = Self::path_for(definition);
        let scratch = scratch_beside(&target);
        fs::write(&scratch, text)?;
        if let Err(error) = fs::rename(&scratch, &target) {
            // The rename is the whole point; a failed one that left its scratch file behind
            // would put a second thing to explain in the definition's directory.
            let _ignored = fs::remove_file(&scratch);
            return Err(GuiError::Io(error));
        }
        Ok(())
    }

    /// Watches every parameter this layout names. Returns how many resolved.
    ///
    /// Runs under the store's *write* lock, at start-up, once. A name the definition no
    /// longer declares is skipped and left in the layout rather than dropped from it: an
    /// operator who reloads last month's definition for one pass must get the plots back
    /// when this month's returns.
    ///
    /// The count is of names that *resolved*, not of rings that were allocated —
    /// [`ParameterStore::watch`] reports `false` for a parameter already watched, and a
    /// layout naming one twice has still named something this definition declares.
    ///
    /// The depth is not set here. [`crate::App`] does that before calling this, so that the
    /// rings are allocated at the size the layout asks for instead of at the store's and
    /// then resized.
    ///
    /// Both lists are walked, not only [`Layout::watched`]. Every plotted parameter is
    /// supposed to be in `watched` as well — [`crate::App::add_to_plot`] puts it there — but
    /// this file is plain JSON in the definition's directory and an operator editing it by
    /// hand is the expected case, not an abuse. A plot naming a parameter that nothing
    /// watches would draw an empty box for the rest of the session with no error anywhere,
    /// which is the worst shape a failure can take: the station works and shows nothing.
    pub fn restore(&self, db: &XtceDb, store: &mut ParameterStore) -> usize {
        let plotted = self.plots.iter().flat_map(|plot| plot.parameters.iter());
        let mut resolved = 0;
        for name in self.watched.iter().chain(plotted) {
            if let Some(parameter) = db.find_parameter(name) {
                store.watch(parameter);
                resolved += 1;
            }
        }
        resolved
    }

    /// Copies what the store is watching into the layout, before a save.
    ///
    /// Runs under the *read* lock. The store is the truth here — the operator can watch a
    /// parameter from the table without a plot ever existing — and a layout written from the
    /// interface's own idea of what is watched drifts from it.
    ///
    /// Sorted, so that two saves of the same station produce the same file: the store hands
    /// its watched parameters back in arena order, which changes when the definition is
    /// edited, and a file that churns on every save is a file nobody can keep in version
    /// control.
    ///
    /// A parameter the definition does not declare cannot be watched, so nothing it names
    /// can appear here; [`Layout::plots`] is left alone for the reason in [`Layout::restore`].
    pub fn capture(&mut self, db: &XtceDb, store: &ParameterStore) {
        // What survives a save that the store knows nothing about: a name this definition
        // does not declare. `restore` skipped it deliberately and left it in the file — "an
        // operator who reloads last month's definition for one pass must get the plots back
        // when this month's returns" — and clearing the list here would undo that on the
        // first watch the operator makes, permanently, with no message. A save runs on any
        // layout change and not only at closing time, so "the first action of the session"
        // is when it would happen.
        let orphans: Vec<String> = self
            .watched
            .drain(..)
            .filter(|name| db.find_parameter(name).is_none())
            .collect();
        self.watched = orphans;
        for parameter in store.watched() {
            // The same lookup `crate::fmt::qualified_name_of` makes for a table cell, written
            // out here so that what goes into a saved file does not depend on a display
            // module. An id the definition does not contain is skipped rather than written as
            // an empty name that would never resolve again.
            if let Some(declared) = db.parameter(parameter) {
                self.watched
                    .push(db.name(declared.qualified_name).to_owned());
            }
        }
        self.watched.sort_unstable();
        self.history_depth = store.depth();
    }

    /// Which plot a parameter is drawn in, if any.
    ///
    /// The first one: a parameter can be in two plots, and this answers the question the
    /// tree asks — "is this already plotted" — rather than enumerating.
    #[must_use]
    pub fn plot_of(&self, name: &str) -> Option<usize> {
        self.plots.iter().position(|plot| plot.contains(name))
    }

    /// Adds a parameter to a plot.
    ///
    /// Out of range is a no-op: the action came from a frame drawn against an older layout,
    /// which is what the frame after a plot is removed is drawn against.
    pub fn add_to_plot(&mut self, plot: usize, name: impl Into<String>) {
        let Some(plot) = self.plots.get_mut(plot) else {
            return;
        };
        let name = name.into();
        // Twice in one plot is two lines on top of each other and two legend entries for one
        // parameter, which reads as a plot that is drawing something it is not.
        if !plot.contains(&name) {
            plot.parameters.push(name);
        }
    }

    /// Removes a parameter from a plot.
    ///
    /// The *plot* stays even when it empties: an empty plot is a slot the operator is about
    /// to fill, and one that vanished under the pointer is a plot they have to make again.
    pub fn remove_from_plot(&mut self, plot: usize, name: &str) {
        if let Some(plot) = self.plots.get_mut(plot) {
            plot.parameters.retain(|held| held != name);
        }
    }

    /// Adds a plot holding one parameter. Returns its index.
    ///
    /// The caller passes the leaf name as the title — a title reading
    /// `/Mission/Payload/Thermal/TEMP_A` is all path and no name — and the qualified name as
    /// the parameter, because that is what survives a rebuilt definition.
    pub fn new_plot(&mut self, title: impl Into<String>, name: impl Into<String>) -> usize {
        let mut plot = PlotLayout::new(title);
        plot.parameters.push(name.into());
        self.plots.push(plot);
        self.plots.len() - 1
    }

    /// Removes a plot.
    ///
    /// The parameters it held stay *watched*: the history behind them is what makes putting
    /// one back instant, and unwatching here would throw away a pass to save 64 KB. Out of
    /// range does nothing, for the reason in [`Layout::add_to_plot`].
    pub fn remove_plot(&mut self, plot: usize) {
        if plot < self.plots.len() {
            self.plots.remove(plot);
        }
    }
}

/// A scratch path in the same directory as `target`.
///
/// The same directory, because `std::fs::rename` is atomic only within one filesystem and a
/// system temporary directory is regularly on another one — a rename across two is an error,
/// not a copy. The process id is in the name so that two stations sharing a definition
/// directory do not write over each other's scratch file on the way in.
fn scratch_beside(target: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    // `file_name` is `None` only for a path ending in `..`, which `path_for` cannot produce —
    // it appends to whatever it is given. The fallback keeps that from being an unwrap.
    name.push(target.file_name().unwrap_or(LAYOUT_SUFFIX.as_ref()));
    name.push(format!(".{}.tmp", std::process::id()));
    target.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use xtce_gs_core::{Batch, Sample, Utc, Value};
    use xtce_model::{ContainerId, ParamId};

    /// Three parameters in two space systems, which is enough for a name to resolve, a name
    /// to fail to resolve, and an order that is not the sorted one.
    const DEFINITION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xtce:SpaceSystem name="Sat" xmlns:xtce="http://www.omg.org/spec/XTCE/20180204">
  <xtce:TelemetryMetaData>
    <xtce:ParameterTypeSet>
      <xtce:IntegerParameterType name="UINT32" signed="false">
        <xtce:IntegerDataEncoding sizeInBits="32" encoding="unsigned"/>
      </xtce:IntegerParameterType>
    </xtce:ParameterTypeSet>
    <xtce:ParameterSet>
      <xtce:Parameter name="VOLTS" parameterTypeRef="UINT32"/>
      <xtce:Parameter name="AMPS" parameterTypeRef="UINT32"/>
      <xtce:Parameter name="TEMP" parameterTypeRef="UINT32"/>
    </xtce:ParameterSet>
    <xtce:ContainerSet>
      <xtce:SequenceContainer name="PKT">
        <xtce:EntryList>
          <xtce:ParameterRefEntry parameterRef="VOLTS"/>
        </xtce:EntryList>
      </xtce:SequenceContainer>
    </xtce:ContainerSet>
  </xtce:TelemetryMetaData>
</xtce:SpaceSystem>"#;

    fn db() -> XtceDb {
        XtceDb::from_xml(DEFINITION).expect("the test definition loads")
    }

    /// A directory of this process's own, removed by the test that made it.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtce-gs-layout-{}-{tag}", std::process::id()));
        let _ignored = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a temporary directory is creatable");
        dir
    }

    fn arranged() -> Layout {
        let mut layout = Layout::default();
        layout.watched.push("/Sat/VOLTS".to_owned());
        layout.watched.push("/Sat/AMPS".to_owned());
        let plot = layout.new_plot("VOLTS", "/Sat/VOLTS");
        layout.add_to_plot(plot, "/Sat/AMPS");
        layout.plots[plot].autoscale = false;
        layout.plots[plot].y_range = Some([-1.5, 27.0]);
        layout.history_depth = 8192;
        layout.theme = Theme::Dark;
        layout.show_raw = true;
        layout.tree_width = 301.5;
        layout.events_height = 99.0;
        layout
    }

    #[test]
    fn a_default_layout_survives_a_json_round_trip() {
        let layout = Layout::default();
        let json = layout.to_json().expect("the default layout serialises");
        assert_eq!(Layout::from_json(&json).expect("it parses back"), layout);
    }

    #[test]
    fn an_arranged_layout_survives_a_json_round_trip() {
        let layout = arranged();
        let json = layout.to_json().expect("an arranged layout serialises");
        let back = Layout::from_json(&json).expect("it parses back");
        assert_eq!(back, layout);
        assert_eq!(back.plots[0].y_range, Some([-1.5, 27.0]));
        assert_eq!(back.plot_of("/Sat/AMPS"), Some(0));
    }

    #[test]
    fn a_layout_from_another_version_is_refused_by_number() {
        let json = r#"{"version": 0, "watched": ["/Sat/VOLTS"]}"#;
        let error = Layout::from_json(json).expect_err("version 0 is not this build's");
        let message = error.to_string();
        assert!(message.contains('0'), "{message}");
        assert!(
            message.contains(&LAYOUT_VERSION.to_string()),
            "the message has to name the version that would have been read: {message}"
        );
    }

    #[test]
    fn a_version_from_the_future_is_refused_too() {
        let json = format!(r#"{{"version": {}}}"#, LAYOUT_VERSION + 1);
        assert!(Layout::from_json(&json).is_err());
    }

    #[test]
    fn a_misspelled_key_is_refused_rather_than_left_at_its_default() {
        // `history_dept` silently keeping 4 096 is the failure this rejects: the operator
        // edits the file, sees no effect, and has nothing to read.
        let json = r#"{"version": 1, "history_dept": 100}"#;
        let error = Layout::from_json(json).expect_err("an unknown field is refused");
        assert!(error.to_string().contains("history_dept"), "{error}");
    }

    #[test]
    fn a_truncated_file_is_refused_with_its_position() {
        let error = Layout::from_json(r#"{"version": 1, "watched": ["#)
            .expect_err("a truncated document is refused");
        let message = error.to_string();
        assert!(message.contains("line"), "{message}");
        assert!(message.contains("column"), "{message}");
    }

    #[test]
    fn an_empty_file_is_refused_and_not_read_as_a_default() {
        assert!(Layout::from_json("").is_err());
    }

    #[test]
    fn the_layout_path_is_the_whole_definition_path_plus_a_suffix() {
        assert_eq!(
            Layout::path_for(Path::new("/missions/ctim/mission.xml")),
            PathBuf::from("/missions/ctim/mission.xml.gs-layout.json")
        );
        // Two definitions in one directory keep two layouts.
        assert_ne!(
            Layout::path_for(Path::new("m.xml")),
            Layout::path_for(Path::new("m.xtce"))
        );
        assert_eq!(
            Layout::path_for(Path::new("")),
            PathBuf::from(LAYOUT_SUFFIX)
        );
    }

    #[test]
    fn a_missing_layout_file_is_the_default_layout() {
        let missing = std::env::temp_dir().join("xtce-gs-no-such-directory-at-all/mission.xml");
        assert_eq!(
            Layout::load(&missing)
                .expect("a first run has no layout and that is not an error")
                .layout,
            Layout::default()
        );
    }

    #[test]
    fn a_layout_that_will_not_parse_names_the_file_it_came_from() {
        let dir = scratch_dir("unparseable");
        let definition = dir.join("mission.xml");
        fs::write(Layout::path_for(&definition), "{ not json").expect("the file is writable");
        let error = Layout::load(&definition).expect_err("a file that exists must parse");
        assert!(
            error.to_string().contains("mission.xml.gs-layout.json"),
            "the operator has to be told which file to open: {error}"
        );
        let _ignored = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_layout_loads_back_unchanged_and_leaves_no_scratch_file() {
        let dir = scratch_dir("round-trip");
        let definition = dir.join("mission.xml");
        let layout = arranged();
        layout.save(&definition).expect("the layout is writable");
        assert_eq!(
            Layout::load(&definition).expect("it loads back").layout,
            layout
        );

        let left: Vec<_> = fs::read_dir(&dir)
            .expect("the directory is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            left.len(),
            1,
            "the rename is what makes the write atomic; the scratch file must not survive it: \
             {left:?}"
        );
        let _ignored = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_over_an_older_layout_replaces_it_whole() {
        let dir = scratch_dir("replace");
        let definition = dir.join("mission.xml");
        arranged().save(&definition).expect("the first save works");
        Layout::default()
            .save(&definition)
            .expect("the second save works");
        let back = Layout::load(&definition).expect("it loads back").layout;
        assert!(
            back.plots.is_empty(),
            "a shorter layout must not leave the tail of the longer one behind"
        );
        let _ignored = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_where_there_is_no_directory_is_an_io_error_and_not_a_panic() {
        let nowhere = std::env::temp_dir().join("xtce-gs-no-such-directory-at-all/mission.xml");
        let error = Layout::default()
            .save(&nowhere)
            .expect_err("a read-only or missing directory refuses the write");
        assert!(matches!(error, GuiError::Io(_)), "{error}");
    }

    #[test]
    fn restore_counts_the_names_this_definition_still_declares() {
        let db = db();
        let mut store = ParameterStore::new(db.parameters().len(), 16);
        let mut layout = Layout::default();
        layout.watched.push("/Sat/VOLTS".to_owned());
        layout.watched.push("/Sat/GONE".to_owned());
        layout.watched.push("/Sat/TEMP".to_owned());

        assert_eq!(layout.restore(&db, &mut store), 2);
        assert!(store.is_watched(db.find_parameter("/Sat/VOLTS").expect("declared")));
        assert!(!store.is_watched(db.find_parameter("/Sat/AMPS").expect("declared")));
        // The name that did not resolve stays: the definition may come back.
        assert!(layout.watched.iter().any(|name| name == "/Sat/GONE"));
    }

    #[test]
    fn restoring_an_empty_layout_watches_nothing() {
        let db = db();
        let mut store = ParameterStore::new(db.parameters().len(), 16);
        assert_eq!(Layout::default().restore(&db, &mut store), 0);
        assert_eq!(store.watched().count(), 0);
    }

    #[test]
    fn capture_takes_what_the_store_watches_in_a_stable_order() {
        let db = db();
        let mut store = ParameterStore::new(db.parameters().len(), 16);
        // Watched in arena order, which is VOLTS, AMPS, TEMP — not the sorted one.
        for name in ["/Sat/VOLTS", "/Sat/AMPS", "/Sat/TEMP"] {
            store.watch(db.find_parameter(name).expect("declared"));
        }
        store.set_depth(512);

        let mut layout = arranged();
        layout.capture(&db, &store);
        assert_eq!(layout.watched, ["/Sat/AMPS", "/Sat/TEMP", "/Sat/VOLTS"]);
        assert_eq!(layout.history_depth, 512);
        // The store is the truth about what is watched; the plots are not touched by it.
        assert_eq!(layout.plots.len(), 1);
    }

    #[test]
    fn capture_forgets_a_parameter_that_was_unwatched() {
        let db = db();
        let mut store = ParameterStore::new(db.parameters().len(), 16);
        let volts = db.find_parameter("/Sat/VOLTS").expect("declared");
        store.watch(volts);
        let mut layout = Layout::default();
        layout.capture(&db, &store);
        assert_eq!(layout.watched, ["/Sat/VOLTS"]);

        store.unwatch(volts);
        layout.capture(&db, &store);
        assert!(layout.watched.is_empty());
    }

    #[test]
    fn a_value_that_arrived_does_not_make_a_parameter_watched() {
        // `capture` reads the watch list and not the values: a table row is not an arrangement.
        let db = db();
        let mut store = ParameterStore::new(db.parameters().len(), 16);
        store.ingest(&Batch {
            container: ContainerId::new(0),
            received: Utc::from_unix_secs(1),
            spacecraft: None,
            apid: 1,
            sequence: 0,
            sequence_gap: false,
            samples: vec![Sample {
                parameter: ParamId::new(0),
                time: Utc::from_unix_secs(1),
                raw: Value::Unsigned(3),
                eng: Value::Unsigned(3),
            }],
        });

        let mut layout = Layout::default();
        layout.capture(&db, &store);
        assert!(layout.watched.is_empty());
    }

    #[test]
    fn a_parameter_is_not_added_to_one_plot_twice() {
        let mut layout = Layout::default();
        let plot = layout.new_plot("VOLTS", "/Sat/VOLTS");
        layout.add_to_plot(plot, "/Sat/VOLTS");
        assert_eq!(layout.plots[plot].parameters, ["/Sat/VOLTS"]);
    }

    #[test]
    fn adding_to_a_plot_that_is_not_there_does_nothing() {
        let mut layout = Layout::default();
        layout.add_to_plot(0, "/Sat/VOLTS");
        layout.add_to_plot(usize::MAX, "/Sat/VOLTS");
        assert!(layout.plots.is_empty());
    }

    #[test]
    fn a_plot_survives_losing_its_last_parameter() {
        let mut layout = Layout::default();
        let plot = layout.new_plot("VOLTS", "/Sat/VOLTS");
        layout.remove_from_plot(plot, "/Sat/VOLTS");
        assert_eq!(layout.plots.len(), 1);
        assert!(layout.plots[plot].parameters.is_empty());
        assert_eq!(layout.plot_of("/Sat/VOLTS"), None);
    }

    #[test]
    fn removing_from_a_plot_leaves_the_other_plots_alone() {
        let mut layout = Layout::default();
        let first = layout.new_plot("VOLTS", "/Sat/VOLTS");
        let second = layout.new_plot("AMPS", "/Sat/VOLTS");
        layout.remove_from_plot(first, "/Sat/VOLTS");
        assert_eq!(layout.plots[second].parameters, ["/Sat/VOLTS"]);
        layout.remove_from_plot(usize::MAX, "/Sat/VOLTS");
        assert_eq!(layout.plots[second].parameters, ["/Sat/VOLTS"]);
    }

    #[test]
    fn removing_a_plot_out_of_range_leaves_the_rest_alone() {
        let mut layout = arranged();
        layout.remove_plot(7);
        assert_eq!(layout.plots.len(), 1);
        layout.remove_plot(0);
        assert!(layout.plots.is_empty());
        // And the parameters it held are still watched, which is what makes putting it back
        // instant.
        assert_eq!(layout.watched.len(), 2);
    }

    #[test]
    fn a_layout_from_another_build_costs_the_arrangement_and_not_the_session() {
        let dir = std::env::temp_dir().join("xtce-gs-layout-version");
        let _ = fs::create_dir_all(&dir);
        let definition = dir.join("mission.xml");
        let path = Layout::path_for(&definition);
        // A version this build does not know, *and* a field it does not declare: an older
        // build's file looks like both at once, and `deny_unknown_fields` would otherwise
        // report the field and never reach the version.
        fs::write(
            &path,
            format!(
                r#"{{"version": {}, "watched": ["/Sat/TEMP"], "a_field_from_the_future": 3}}"#,
                LAYOUT_VERSION + 1
            ),
        )
        .expect("write");

        let read = Layout::load(&definition).expect("an old layout must not stop the station");
        assert_eq!(
            read.layout,
            Layout::default(),
            "it should start from the default"
        );
        let note = read.note.expect("and it must say what it discarded");
        assert!(note.contains(&(LAYOUT_VERSION + 1).to_string()), "{note}");
        assert!(
            fs::read_to_string(&path).is_ok_and(|text| text.contains("a_field_from_the_future")),
            "the file itself must be left alone until something is saved over it"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_theme_maps_to_the_preference_egui_takes() {
        assert_eq!(Theme::System.preference(), egui::ThemePreference::System);
        assert_eq!(Theme::Dark.preference(), egui::ThemePreference::Dark);
        assert_eq!(Theme::Light.preference(), egui::ThemePreference::Light);
    }
}
