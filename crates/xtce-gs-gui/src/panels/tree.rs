//! The parameter list: everything the definition declares, findable.
//!
//! CTIM declares 9 493 parameters. That is the number this panel is designed around: a widget
//! that builds one row per parameter per frame spends the frame building rows nobody is
//! looking at, and a filter that walks every qualified name per frame spends it comparing
//! strings that have not changed. So the filter runs when the text changes and the result is
//! kept, and the drawing walks only what matched.
//!
//! Grouping is by space system because that is the only structure an XTCE document actually
//! has. It is not a tree of the operator's making — `/Mission/Payload/Thermal` came from
//! whoever wrote the XML — but it is the structure the names are built from, and a flat list
//! of ten thousand names sorted alphabetically puts `BATT_V` next to `BATT_V_RAW` from a
//! different subsystem.

use xtce_gs_core::{ParameterStore, Utc};
use xtce_model::{ParamId, SpaceSystemId, TypeKind, XtceDb};

use crate::fmt;
use crate::panels::Action;

/// Parameters listed before the list stops and says how many more matched.
///
/// A search for `V` on a mission database matches thousands, and drawing them is neither
/// useful nor free. The operator's next keystroke is what narrows it.
pub const MAX_MATCHES: usize = 512;

/// Height of one row, in points.
///
/// Fixed for the same reason the table's is: `egui::ScrollArea::show_rows` decides which rows
/// are visible from a height given in advance, so a group header occupies one row of exactly
/// this height rather than growing the row it sits above.
pub const ROW_HEIGHT: f32 = 18.0;

/// Width of the column that says whether a parameter is watched, in points.
const MARKER_WIDTH: f32 = 14.0;

/// What a watched parameter is marked with.
const WATCHED_MARKER: &str = "●";

/// One parameter that survived the filter.
///
/// Carries what the row draws so that drawing does not go back to the store: `watched` and
/// `seen` are copied under the read lock in [`Tree::refresh`], and drawing a checkbox from a
/// live store would mean holding that lock through the panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Match {
    /// The parameter.
    pub parameter: ParamId,
    /// The space system that declares it, for the group header.
    pub space_system: SpaceSystemId,
    /// Whether history is being kept for it.
    pub watched: bool,
    /// Whether a value for it has ever arrived.
    ///
    /// The distinction the operator needs: a parameter with no value has either not been
    /// sent yet or is in a container this definition never matches, and both look like an
    /// empty row unless the list says which.
    pub seen: bool,
    /// When a value for it last arrived, if one ever has.
    ///
    /// Carried rather than looked up while drawing, for the reason the whole struct exists:
    /// the store's lock is held in [`Tree::refresh`] and not through the panel.
    pub last: Option<Utc>,
    /// Whether it has a place on a numeric axis.
    ///
    /// Decided from the declared type and not from the latest value, so the answer is the
    /// same before the first packet as after it — a menu item that appears once telemetry
    /// starts is a menu item the operator stopped looking for.
    pub plottable: bool,
}

/// The parameter list's own state: what was typed, and what it matched.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    search: String,
    matches: Vec<Match>,
    truncated: usize,
    selected: Option<ParamId>,
    dirty: bool,
}

impl Tree {
    /// An empty list with no filter.
    ///
    /// Nothing is scanned here: the first [`Tree::refresh`] fills it. A constructor that
    /// walked the definition would do it before the window's first frame, which is where a
    /// mission database's parameter count is most visible as a delay.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dirty: true,
            ..Self::default()
        }
    }

    /// What the operator typed.
    #[must_use]
    pub fn search(&self) -> &str {
        &self.search
    }

    /// The filter text, for a text field to write into.
    ///
    /// Marks the list dirty unconditionally — a `&mut String` cannot tell whether the caller
    /// changed it. One wasted pass over the names when the operator clicks into the box is
    /// cheaper than a list that does not update when they type.
    pub fn search_mut(&mut self) -> &mut String {
        self.dirty = true;
        &mut self.search
    }

    /// Whether the filter has to be run again.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The parameters that matched, in definition order.
    #[must_use]
    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    /// How many further parameters matched and were not kept — see [`MAX_MATCHES`].
    #[must_use]
    pub const fn truncated(&self) -> usize {
        self.truncated
    }

    /// The parameter whose detail is being shown.
    #[must_use]
    pub const fn selected(&self) -> Option<ParamId> {
        self.selected
    }

    /// Shows a parameter's detail, or nothing.
    pub fn select(&mut self, parameter: Option<ParamId>) {
        self.selected = parameter;
    }

    /// Re-runs the filter when it changed, and refreshes the watched and seen flags.
    ///
    /// The match list is rebuilt only when the filter is dirty; the flags are refreshed on
    /// *every* call. A parameter's first value arrives while the filter has not changed, and
    /// a list that showed it as unseen until the next keystroke is a list that is wrong most
    /// of the time.
    ///
    /// The scan is over `db.parameters()` in arena order, which is document order and
    /// therefore groups a space system's parameters together without a sort. Matching is
    /// against the *qualified* name, so that typing `thermal` finds a subsystem and typing
    /// `TEMP_A` finds a parameter.
    ///
    /// This is the only place in this module that touches the store, and it runs under the
    /// read lock.
    pub fn refresh(&mut self, db: &XtceDb, store: &ParameterStore) {
        if self.dirty {
            self.matches.clear();
            self.truncated = 0;
            for (index, parameter) in db.parameters().iter().enumerate() {
                let qualified = db.name(parameter.qualified_name);
                if !fmt::contains_ignore_ascii_case(qualified, &self.search) {
                    continue;
                }
                if self.matches.len() >= MAX_MATCHES {
                    // Counting, not stopping: a list that stopped scanning at the cap would
                    // say "512 more" for every filter that overflows it, forever.
                    self.truncated += 1;
                    continue;
                }
                let id = ParamId::new(index as u32);
                self.matches.push(Match {
                    parameter: id,
                    space_system: parameter.space_system,
                    watched: false,
                    seen: false,
                    last: None,
                    plottable: is_plottable(db, id),
                });
            }
            self.dirty = false;
        }

        for entry in &mut self.matches {
            entry.watched = store.is_watched(entry.parameter);
            entry.last = store.latest_time(entry.parameter);
            entry.seen = entry.last.is_some();
        }
    }
}

/// Whether a parameter's declared type has a place on a numeric axis.
///
/// Integer, float and boolean do — a boolean plots as 0 and 1, which is how an operator
/// watches a relay. A string, a binary blob, an enumeration label and the composite kinds do
/// not: an enumeration's engineering value is a label, and a label has no order. The two time
/// kinds are left out as well; an `AbsoluteTimeParameterType` is an instant, and a plot of an
/// instant against itself is a straight line the operator did not ask for.
fn is_plottable(db: &XtceDb, parameter: ParamId) -> bool {
    matches!(
        db.type_of(parameter).map(|kind| &kind.kind),
        Some(TypeKind::Integer | TypeKind::Float | TypeKind::Boolean { .. })
    )
}

/// One drawn row: either a group header or a parameter.
///
/// Headers are rows of their own rather than decoration above a parameter's row, because
/// `egui::ScrollArea::show_rows` derives the visible range from a single row height and a
/// count — a header drawn inside another row would make that row taller than [`ROW_HEIGHT`]
/// and drift the range away from what is actually on screen.
enum PlanRow<'a> {
    /// The space system the rows below it belong to.
    Header(SpaceSystemId),
    /// One parameter.
    Parameter(&'a Match),
}

/// The rows to draw, headers included.
///
/// The matches are in definition order, so a space system that differs from the previous
/// row's starts a group and no map is needed.
fn plan(matches: &[Match]) -> impl Iterator<Item = PlanRow<'_>> {
    matches.iter().enumerate().flat_map(move |(index, entry)| {
        let starts_group = index == 0
            || matches
                .get(index - 1)
                .is_some_and(|previous| previous.space_system != entry.space_system);
        starts_group
            .then_some(PlanRow::Header(entry.space_system))
            .into_iter()
            .chain(std::iter::once(PlanRow::Parameter(entry)))
    })
}

/// Draws the parameter list. Returns what the operator asked for.
///
/// A click selects; a double click watches or unwatches by the row's current state; the
/// context menu offers the existing plots and a new one, and only for a row whose
/// [`Match::plottable`] is true — a label has no position on an axis, and an enabled menu
/// item that draws nothing is worse than a missing one. `plots` is how many plots the layout
/// holds, which is all the menu needs to know about them.
pub fn show(ui: &mut egui::Ui, tree: &mut Tree, db: &XtceDb, now: Utc, plots: usize) -> Action {
    let mut action = Action::None;

    ui.horizontal(|ui| {
        ui.label("search");
        // Through a copy, because `Tree::search_mut` marks the list dirty and handing the
        // field itself to the text field would mark it dirty every frame — which is a rebuild
        // of every qualified name in the definition, sixty times a second, to find out that
        // nothing was typed. One small `String` per frame is the cheaper half of that trade.
        let mut text = tree.search().to_owned();
        let response = ui.add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(f32::INFINITY)
                .hint_text("qualified name"),
        );
        if response.changed() {
            tree.search_mut().clone_from(&text);
        }
    });

    let selected = tree.selected();
    let truncated = tree.truncated();
    let matches = tree.matches();
    let rows = plan(matches).count();

    // One buffer for the hover text of every row drawn this frame, rather than one per row.
    let mut hover = String::new();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, rows, |ui, range| {
            let visible = plan(matches).skip(range.start).take(range.len());
            for row in visible {
                match row {
                    PlanRow::Header(system) => draw_header(ui, db, system),
                    PlanRow::Parameter(entry) => {
                        action =
                            action.or(draw_row(ui, entry, db, now, selected, plots, &mut hover));
                    }
                }
            }
        });

    if truncated > 0 {
        // The count is the whole point: an operator who cannot see it does not know whether
        // to narrow the search.
        ui.weak(format!("… and {truncated} more, narrow the search"));
    }

    action
}

/// One group header: the space system the rows below it belong to.
fn draw_header(ui: &mut egui::Ui, db: &XtceDb, system: SpaceSystemId) {
    let name = db
        .space_system(system)
        .map_or("", |system| db.name(system.qualified_name));
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| ui.add(egui::Label::new(egui::RichText::new(name).strong()).truncate()),
    );
}

/// One parameter row.
fn draw_row(
    ui: &mut egui::Ui,
    entry: &Match,
    db: &XtceDb,
    now: Utc,
    selected: Option<ParamId>,
    plots: usize,
    hover: &mut String,
) -> Action {
    use std::fmt::Write as _;

    let mut action = Action::None;
    ui.horizontal(|ui| {
        ui.set_height(ROW_HEIGHT);

        let marker = if entry.watched { WATCHED_MARKER } else { "" };
        ui.add_sized(
            egui::vec2(MARKER_WIDTH, ROW_HEIGHT),
            egui::Label::new(marker).truncate(),
        );

        let mut text = egui::RichText::new(fmt::name_of(db, entry.parameter));
        if !entry.seen {
            // Never arrived: either not sent yet, or in a container this definition never
            // matches. Weak is the only difference the operator needs at a glance.
            text = text.weak();
        }
        let response = ui.add_sized(
            egui::vec2(ui.available_width(), ROW_HEIGHT),
            egui::Button::selectable(selected == Some(entry.parameter), text),
        );

        hover.clear();
        hover.push_str(fmt::qualified_name_of(db, entry.parameter));
        hover.push_str(if entry.watched {
            "\nwatched · last "
        } else {
            "\nnot watched · last "
        });
        // `never` for a parameter the definition declares and the spacecraft has not sent,
        // which is the first question anyone asks of a definition they did not write.
        fmt::age_of(hover, now, entry.last);
        let response = response.on_hover_text(&*hover);

        if response.double_clicked() {
            action = if entry.watched {
                Action::Unwatch(entry.parameter)
            } else {
                Action::Watch(entry.parameter)
            };
        } else if response.clicked() {
            action = Action::Select(entry.parameter);
        }

        if entry.plottable {
            response.context_menu(|ui| {
                for plot in 0..plots {
                    hover.clear();
                    let _ = write!(hover, "plot in {}", plot + 1);
                    if ui.button(&*hover).clicked() {
                        action = Action::AddToPlot {
                            parameter: entry.parameter,
                            plot,
                        };
                        ui.close();
                    }
                }
                if ui.button("new plot").clicked() {
                    action = Action::NewPlot(entry.parameter);
                    ui.close();
                }
            });
        }
    });
    action
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use xtce_gs_core::{Sample, Utc, Value};

    use super::*;

    /// Two space systems, a plottable parameter and two that are not.
    const DEFINITION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SpaceSystem xmlns="http://www.omg.org/spec/XTCE/20180204" name="Sat">
  <TelemetryMetaData>
    <ParameterTypeSet>
      <FloatParameterType name="Kelvin" sizeInBits="32"><UnitSet/><FloatDataEncoding sizeInBits="32"/></FloatParameterType>
      <StringParameterType name="Tag">
        <UnitSet/>
        <StringDataEncoding><SizeInBits><Fixed><FixedValue>64</FixedValue></Fixed></SizeInBits></StringDataEncoding>
      </StringParameterType>
      <EnumeratedParameterType name="Mode">
        <UnitSet/>
        <IntegerDataEncoding sizeInBits="8" encoding="unsigned"/>
        <EnumerationList><Enumeration value="0" label="SAFE"/><Enumeration value="1" label="NOMINAL"/></EnumerationList>
      </EnumeratedParameterType>
    </ParameterTypeSet>
    <ParameterSet>
      <Parameter name="TEMP_A" parameterTypeRef="Kelvin"/>
      <Parameter name="LABEL" parameterTypeRef="Tag"/>
      <Parameter name="MODE" parameterTypeRef="Mode"/>
    </ParameterSet>
    <ContainerSet/>
  </TelemetryMetaData>
  <SpaceSystem name="Thermal">
    <TelemetryMetaData>
      <ParameterTypeSet>
        <IntegerParameterType name="Counts"><UnitSet/><IntegerDataEncoding sizeInBits="8" encoding="unsigned"/></IntegerParameterType>
      </ParameterTypeSet>
      <ParameterSet>
        <Parameter name="HEATER_ON" parameterTypeRef="Counts"/>
      </ParameterSet>
      <ContainerSet/>
    </TelemetryMetaData>
  </SpaceSystem>
</SpaceSystem>"#;

    fn db() -> XtceDb {
        XtceDb::from_xml(DEFINITION).expect("the definition loads")
    }

    fn id(db: &XtceDb, name: &str) -> ParamId {
        db.find_parameter(name)
            .unwrap_or_else(|| panic!("{name} is declared"))
    }

    fn store(db: &XtceDb) -> ParameterStore {
        ParameterStore::new(db.parameters().len(), 16)
    }

    fn names(tree: &Tree, db: &XtceDb) -> Vec<String> {
        tree.matches()
            .iter()
            .map(|entry| fmt::qualified_name_of(db, entry.parameter).to_owned())
            .collect()
    }

    /// A definition with `count` parameters in one space system.
    fn wide_definition(count: usize) -> String {
        let mut parameters = String::new();
        for index in 0..count {
            let _ = write!(
                parameters,
                "<Parameter name=\"P{index:04}\" parameterTypeRef=\"U8\"/>"
            );
        }
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<SpaceSystem xmlns="http://www.omg.org/spec/XTCE/20180204" name="Big">
  <TelemetryMetaData>
    <ParameterTypeSet>
      <IntegerParameterType name="U8"><UnitSet/><IntegerDataEncoding sizeInBits="8" encoding="unsigned"/></IntegerParameterType>
    </ParameterTypeSet>
    <ParameterSet>{parameters}</ParameterSet>
    <ContainerSet/>
  </TelemetryMetaData>
</SpaceSystem>"#
        )
    }

    #[test]
    fn an_empty_filter_matches_everything_in_definition_order() {
        let db = db();
        let store = store(&db);
        let mut tree = Tree::new();
        tree.refresh(&db, &store);

        assert_eq!(
            names(&tree, &db),
            [
                "/Sat/TEMP_A",
                "/Sat/LABEL",
                "/Sat/MODE",
                "/Sat/Thermal/HEATER_ON",
            ],
            "arena order is document order, which groups a space system together"
        );
        assert_eq!(tree.truncated(), 0);
        assert!(!tree.is_dirty(), "the filter is clean once it has been run");
    }

    #[test]
    fn the_filter_matches_the_qualified_name_case_insensitively() {
        let db = db();
        let store = store(&db);
        let mut tree = Tree::new();

        tree.search_mut().push_str("thermal");
        tree.refresh(&db, &store);
        assert_eq!(
            names(&tree, &db),
            ["/Sat/Thermal/HEATER_ON"],
            "typing a subsystem finds the subsystem"
        );

        tree.search_mut().clear();
        tree.search_mut().push_str("temp_a");
        tree.refresh(&db, &store);
        assert_eq!(names(&tree, &db), ["/Sat/TEMP_A"]);

        tree.search_mut().clear();
        tree.search_mut().push_str("no such parameter");
        tree.refresh(&db, &store);
        assert!(tree.matches().is_empty());
    }

    #[test]
    fn the_truncated_count_keeps_counting_past_the_cap() {
        let extra = 88;
        let db = XtceDb::from_xml(&wide_definition(MAX_MATCHES + extra))
            .expect("the wide definition loads");
        let store = store(&db);
        let mut tree = Tree::new();
        tree.refresh(&db, &store);

        assert_eq!(tree.matches().len(), MAX_MATCHES);
        assert_eq!(
            tree.truncated(),
            extra,
            "a scan that stopped at the cap would say the same number for every filter"
        );
    }

    #[test]
    fn the_flags_follow_the_store_without_the_filter_changing() {
        let db = db();
        let mut store = store(&db);
        let mut tree = Tree::new();
        tree.refresh(&db, &store);
        assert!(tree.matches().iter().all(|entry| !entry.seen));
        assert!(!tree.is_dirty());

        store.watch(id(&db, "TEMP_A"));
        store.push(Sample {
            parameter: id(&db, "TEMP_A"),
            time: Utc::from_unix_secs(1),
            raw: Value::Unsigned(1),
            eng: Value::Float(300.0),
        });
        tree.refresh(&db, &store);

        let temp = tree
            .matches()
            .iter()
            .find(|entry| entry.parameter == id(&db, "TEMP_A"))
            .copied()
            .expect("TEMP_A matched an empty filter");
        assert!(temp.seen, "a first value must show without a keystroke");
        assert!(temp.watched);
        assert_eq!(tree.matches().len(), 4, "the match list was not rebuilt");
    }

    #[test]
    fn only_a_type_with_a_numeric_axis_is_plottable() {
        let db = db();
        let store = store(&db);
        let mut tree = Tree::new();
        tree.refresh(&db, &store);

        let plottable = |name: &str| {
            tree.matches()
                .iter()
                .find(|entry| entry.parameter == id(&db, name))
                .is_some_and(|entry| entry.plottable)
        };
        assert!(plottable("TEMP_A"), "a float plots");
        assert!(plottable("HEATER_ON"), "an integer plots");
        assert!(!plottable("LABEL"), "a string has no position on an axis");
        assert!(
            !plottable("MODE"),
            "an enumeration's engineering value is a label, and a label has no order"
        );
    }

    #[test]
    fn a_group_header_is_drawn_once_per_space_system() {
        let db = db();
        let store = store(&db);
        let mut tree = Tree::new();
        tree.refresh(&db, &store);

        let headers = plan(tree.matches())
            .filter(|row| matches!(row, PlanRow::Header(_)))
            .count();
        assert_eq!(headers, 2, "two space systems declare parameters");
        assert_eq!(
            plan(tree.matches()).count(),
            tree.matches().len() + headers,
            "every header is a row of its own, or `show_rows` draws the wrong range"
        );
    }

    #[test]
    fn an_empty_list_draws_without_taking_the_window_down() {
        let db = db();
        let mut tree = Tree::new();
        // Never refreshed: no matches, and `show_rows` is asked for a total of zero.
        egui::__run_test_ui(|ui| {
            assert!(show(ui, &mut tree, &db, Utc::EPOCH, 0).is_none());
        });
    }

    #[test]
    fn a_filled_list_draws_and_asks_for_nothing_on_its_own() {
        let db = db();
        let store = store(&db);
        let mut tree = Tree::new();
        tree.refresh(&db, &store);
        tree.select(Some(id(&db, "TEMP_A")));

        egui::__run_test_ui(|ui| {
            assert!(
                show(ui, &mut tree, &db, Utc::EPOCH, 3).is_none(),
                "nothing was clicked"
            );
        });
        assert_eq!(tree.matches().len(), 4, "drawing does not change the list");
        assert!(
            !tree.is_dirty(),
            "drawing the search box must not mark the filter dirty on its own"
        );
    }
}
