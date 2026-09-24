//! Current values: what every parameter is worth right now.
//!
//! The panel an operator actually watches. Six columns — name, raw, engineering, unit, age,
//! limit — and the order is not arbitrary: the name is what is scanned for, the engineering
//! value is what is read, and the age is what says whether the value is still true. The raw
//! column is off by default because on a calibrated parameter it is the number nobody wants,
//! and on an uncalibrated one it is the same number twice.
//!
//! # Why rows are copied
//!
//! [`Table::refresh`] copies a [`Row`] per parameter under the read lock, and the drawing
//! reads only those. The alternative — drawing from the store — holds the read lock across a
//! frame, and the decode thread needs the write lock to file the next packet. A few hundred
//! rows of two `Value`s is a few hundred refcount bumps, which is what [`xtce_gs_core::Value`]
//! holds its `Arc`s for.
//!
//! # Why only the visible rows are formatted
//!
//! A definition declares up to 9 493 parameters and a pass fills a few hundred of them.
//! Formatting every row of a table that shows twenty is the mistake this panel exists to
//! avoid, so [`show`] draws through [`egui::ScrollArea::show_rows`], which hands back the
//! visible index range and nothing else. That is also why every row is exactly [`ROW_HEIGHT`]
//! tall and laid out in fixed-width cells rather than in an [`egui::Grid`]: a grid's rows
//! size to their contents, and `show_rows` computes which rows are visible from one height it
//! is given in advance.

use std::cmp::Ordering;
use std::fmt::Write as _;

use xtce_gs_core::{LimitSet, LimitState, ParameterStore, Utc, Value};
use xtce_model::{ParamId, XtceDb};

use crate::fmt;
use crate::panels::Action;

/// Height of one row, in points.
///
/// Fixed, because `egui::ScrollArea::show_rows` needs a row height to know which rows are
/// visible without building the others — which is the difference between building twenty rows
/// a frame and building every parameter the definition declares.
pub const ROW_HEIGHT: f32 = 18.0;

/// Width of each column, in points, in the order the row draws them.
///
/// The name is widest because it is what the eye scans; the limit is narrowest because it is
/// five characters at most — see [`xtce_gs_core::LimitState::label`].
const NAME_WIDTH: f32 = 190.0;
/// Width of the raw column.
const RAW_WIDTH: f32 = 92.0;
/// Width of the engineering column.
const ENG_WIDTH: f32 = 112.0;
/// Width of the unit column.
const UNIT_WIDTH: f32 = 54.0;
/// Width of the age column.
const AGE_WIDTH: f32 = 58.0;
/// Width of the limit column.
const LIMIT_WIDTH: f32 = 52.0;

/// One parameter's current value, copied out of the store.
#[derive(Clone, Debug)]
pub struct Row {
    /// Which parameter.
    pub parameter: ParamId,
    /// When the value was placed — spacecraft time when the packet carried one.
    pub time: Utc,
    /// The bits as they appeared in the packet.
    pub raw: Value,
    /// The value after calibration, enumeration lookup or text decoding.
    pub eng: Value,
    /// How many times it has arrived since the session started.
    pub updates: u32,
    /// Where it sits against its limits, if it has any.
    pub limit: LimitState,
    /// Whether the store is keeping history for it.
    ///
    /// Not a display column: it is what lets the context menu offer the one item that does
    /// something. `ParameterStore::watch` returns `false` for a parameter already watched and
    /// `unwatch` on an unwatched one does nothing, so a menu that offers both always offers
    /// one no-op, and an operator cannot tell which.
    pub watched: bool,
}

/// Which column the table is ordered by.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Sort {
    /// Qualified name, which also groups by space system.
    #[default]
    Name,
    /// Most recently updated first.
    Time,
    /// Worst limit state first — see [`xtce_gs_core::LimitState`]'s ordering.
    Limit,
    /// Most frequently updated first, which is how a chatty APID is found.
    Updates,
}

impl Sort {
    /// What the header of this column says.
    const fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Time => "age",
            Self::Limit => "limit",
            Self::Updates => "updates",
        }
    }
}

/// The table's own state: the rows it drew, and how they are ordered.
#[derive(Debug, Default)]
pub struct Table {
    rows: Vec<Row>,
    filter: String,
    sort: Sort,
    descending: bool,
    watched_only: bool,
}

impl Table {
    /// An empty table sorted by name.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The rows as of the last refresh.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The substring rows are filtered by.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The filter, for a text field to write into.
    ///
    /// Unlike the tree's, this needs no dirty flag: the filter is applied while the rows are
    /// being copied, which happens every frame anyway.
    pub fn filter_mut(&mut self) -> &mut String {
        &mut self.filter
    }

    /// Which column the rows are ordered by.
    #[must_use]
    pub const fn sort(&self) -> Sort {
        self.sort
    }

    /// Orders by a column, reversing when it is already the one in use.
    ///
    /// That is what a second click on a header means everywhere else, and a table that
    /// reset to ascending instead would make "worst limit last" unreachable.
    pub fn set_sort(&mut self, sort: Sort) {
        if self.sort == sort {
            self.descending = !self.descending;
        } else {
            self.sort = sort;
            self.descending = false;
        }
    }

    /// Whether the order is reversed.
    #[must_use]
    pub const fn is_descending(&self) -> bool {
        self.descending
    }

    /// Whether only watched parameters are listed.
    #[must_use]
    pub const fn watched_only(&self) -> bool {
        self.watched_only
    }

    /// Lists only watched parameters, or everything that has arrived.
    pub fn set_watched_only(&mut self, only: bool) {
        self.watched_only = only;
    }

    /// Copies the current values out of the store.
    ///
    /// Fills from [`ParameterStore::seen`] and not from every index: a store built for a
    /// mission database has 9 493 slots of which a pass fills a few hundred, and walking all
    /// of them per frame is the cost this panel is shaped to avoid.
    ///
    /// The limit state is [`LimitSet::evaluate`] against the qualified name, which is a
    /// string hash per row per frame — acceptable at a few hundred rows, and the reason the
    /// limits are keyed by name rather than by index is in [`xtce_gs_core::limits`].
    ///
    /// Runs under the read lock, and is the only method here that names the store.
    pub fn refresh(&mut self, store: &ParameterStore, db: &XtceDb, limits: &LimitSet) {
        self.rows.clear();
        for parameter in store.seen() {
            if self.watched_only && !store.is_watched(parameter) {
                continue;
            }
            let qualified = fmt::qualified_name_of(db, parameter);
            if !fmt::contains_ignore_ascii_case(qualified, &self.filter) {
                continue;
            }
            let Some(sample) = store.latest(parameter) else {
                continue;
            };
            self.rows.push(Row {
                parameter,
                time: sample.time,
                raw: sample.raw.clone(),
                eng: sample.eng.clone(),
                updates: store.updates(parameter),
                limit: limits.evaluate(qualified, &sample.eng),
                // Free: `watched_only` above already asks the store the same question, and
                // `is_watched` is one `Option` check against a `Vec` slot.
                watched: store.is_watched(parameter),
            });
        }
        self.sort_rows(db);
    }

    /// Orders `rows` by the column in use.
    ///
    /// The *first* click on a column is the order that column is worth looking at, which for
    /// three of the four is the largest first: the newest value, the worst limit, the
    /// chattiest parameter. Only the name sorts ascending. [`Table::set_sort`] then reverses
    /// on a second click, so `descending` here means "reversed from that", which is what
    /// [`Table::is_descending`] documents and not "reversed from `Ord`".
    ///
    /// The name is the tie-break in every order, and it is looked up twice per comparison
    /// rather than cached on the [`Row`]: a row carries the values an operator reads, and a
    /// few hundred of them make a handful of array indexings per comparison, not a hash.
    fn sort_rows(&mut self, db: &XtceDb) {
        let reversed = self.descending;
        let direction = |ordering: Ordering| {
            if reversed {
                ordering.reverse()
            } else {
                ordering
            }
        };
        let sort = self.sort;
        self.rows.sort_by(|a, b| {
            let primary = match sort {
                Sort::Name => Ordering::Equal,
                Sort::Time => b.time.cmp(&a.time),
                Sort::Limit => b.limit.cmp(&a.limit),
                Sort::Updates => b.updates.cmp(&a.updates),
            };
            let by_name = fmt::qualified_name_of(db, a.parameter)
                .cmp(fmt::qualified_name_of(db, b.parameter));
            direction(primary.then(by_name))
        });
    }
}

/// Draws the value table. Returns what the operator asked for.
///
/// Only the rows `egui::ScrollArea::show_rows` reports as visible are formatted, and every
/// cell formats into `scratch`, cleared per cell: one `String` for the whole table's
/// *formatting* rather than one per cell per frame.
///
/// That is the formatting allocation and not the whole cost of a cell. egui takes its text by
/// value: `impl From<&str> for WidgetText` is `text.to_owned()` (egui 0.35 `widget_text.rs`),
/// so each `Label::new`, each `Button::selectable` and each `on_hover_text` copies the
/// `scratch` it was handed into a `String` of its own — a tooltip's text eagerly, whether or
/// not it is ever shown. `draw_row` makes seven of those per visible row, eight with the raw
/// column shown, before the context menu is opened. The scratch buys the formatting, not the
/// copy; anyone trying to make this path allocation-free has to start at `WidgetText` and not
/// here.
///
/// `Table::sort_rows` also allocates: `slice::sort_by` is a merge sort with a temporary
/// buffer. It stays, because that buffer is one allocation per [`Table::refresh`] over a few
/// hundred rows, against the per-cell copies above that it is already amortised over.
/// Hoisting a scratch onto [`Table`] for it would be optimising the cheapest allocation on
/// the path.
///
/// A click on a row's name returns [`Action::Select`]; its context menu offers watch,
/// unwatch and a new plot — an operator who found a parameter in the table must not have to
/// find it again in the tree to plot it.
pub fn show(
    ui: &mut egui::Ui,
    table: &mut Table,
    db: &XtceDb,
    now: Utc,
    show_raw: bool,
    plots: usize,
    scratch: &mut String,
) -> Action {
    let mut action = Action::None;

    ui.horizontal(|ui| {
        ui.label("filter");
        ui.add(
            egui::TextEdit::singleline(table.filter_mut())
                .desired_width(160.0)
                .hint_text("qualified name"),
        );
        let mut only = table.watched_only();
        if ui.checkbox(&mut only, "watched only").changed() {
            table.set_watched_only(only);
        }
        if ui
            .small_button(Sort::Updates.label())
            .on_hover_text("order by how often each parameter arrives")
            .clicked()
        {
            table.set_sort(Sort::Updates);
        }
        scratch.clear();
        let _ = write!(scratch, "{} rows", table.rows().len());
        ui.weak(&*scratch);
    });

    // The header. Only the three columns that carry an order are clickable: a unit and a raw
    // value are not orders anybody asks a table for.
    ui.horizontal(|ui| {
        ui.set_height(ROW_HEIGHT);
        if header(ui, table, Sort::Name, NAME_WIDTH, scratch) {
            table.set_sort(Sort::Name);
        }
        if show_raw {
            label_cell(ui, RAW_WIDTH, "raw");
        }
        label_cell(ui, ENG_WIDTH, "value");
        label_cell(ui, UNIT_WIDTH, "unit");
        if header(ui, table, Sort::Time, AGE_WIDTH, scratch) {
            table.set_sort(Sort::Time);
        }
        if header(ui, table, Sort::Limit, LIMIT_WIDTH, scratch) {
            table.set_sort(Sort::Limit);
        }
    });
    ui.separator();

    let rows = table.rows();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
            for index in range {
                let Some(row) = rows.get(index) else {
                    // `show_rows` clamps its range to the count it was given, so this is
                    // unreachable — and a table that indexed blindly would take the window
                    // down the day it is not.
                    continue;
                };
                action = action.or(draw_row(ui, row, db, now, show_raw, plots, scratch));
            }
        });

    action
}

/// One clickable column header. Returns whether it was clicked.
fn header(ui: &mut egui::Ui, table: &Table, sort: Sort, width: f32, scratch: &mut String) -> bool {
    scratch.clear();
    scratch.push_str(sort.label());
    if table.sort() == sort {
        // The arrow reports the order the rows are actually in, which is not the same as
        // `descending`: the first click on a value column is largest-first — the newest
        // sample, the worst limit, the chattiest parameter — and only the name starts
        // ascending. See [`Table::sort_rows`].
        let ascending = (sort == Sort::Name) != table.is_descending();
        scratch.push_str(if ascending { " ▲" } else { " ▼" });
    }
    ui.add_sized(
        egui::vec2(width, ROW_HEIGHT),
        egui::Button::selectable(table.sort() == sort, &*scratch),
    )
    .clicked()
}

/// One cell of plain text, clipped to its column.
fn label_cell(ui: &mut egui::Ui, width: f32, text: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ROW_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| ui.add(egui::Label::new(text).truncate()),
    );
}

/// One row of the table.
fn draw_row(
    ui: &mut egui::Ui,
    row: &Row,
    db: &XtceDb,
    now: Utc,
    show_raw: bool,
    plots: usize,
    scratch: &mut String,
) -> Action {
    let mut action = Action::None;
    ui.horizontal(|ui| {
        ui.set_height(ROW_HEIGHT);
        // A row out of limits is coloured whole. Colouring only the value cell puts the
        // colour where the eye is not when it is scanning the name column.
        if row.limit.is_violation() {
            let colour = fmt::limit_color(row.limit, ui.visuals());
            ui.visuals_mut().override_text_color = Some(colour);
        }

        let name = ui
            .add_sized(
                egui::vec2(NAME_WIDTH, ROW_HEIGHT),
                egui::Button::selectable(false, fmt::name_of(db, row.parameter)),
            )
            .on_hover_text(fmt::qualified_name_of(db, row.parameter));
        if name.clicked() {
            action = action.or(Action::Select(row.parameter));
        }
        name.context_menu(|ui| {
            // One item, not two: the row knows which of them would do something.
            if row.watched {
                if ui.button("unwatch").clicked() {
                    action = Action::Unwatch(row.parameter);
                    ui.close();
                }
            } else if ui.button("watch").clicked() {
                action = Action::Watch(row.parameter);
                ui.close();
            }
            // The same targets the tree offers, for the same reason: an operator comparing a
            // current against a voltage wants them on one axis, and reaching the tree to do
            // it means losing the row they were reading.
            for plot in 0..plots {
                scratch.clear();
                let _ = write!(scratch, "plot in {}", plot + 1);
                if ui.button(&*scratch).clicked() {
                    action = Action::AddToPlot {
                        parameter: row.parameter,
                        plot,
                    };
                    ui.close();
                }
            }
            if ui.button("new plot").clicked() {
                action = Action::NewPlot(row.parameter);
                ui.close();
            }
        });

        if show_raw {
            scratch.clear();
            fmt::value(scratch, &row.raw);
            label_cell(ui, RAW_WIDTH, scratch);
        }

        scratch.clear();
        fmt::value(scratch, &row.eng);
        label_cell(ui, ENG_WIDTH, scratch);

        label_cell(
            ui,
            UNIT_WIDTH,
            fmt::unit_of(db, row.parameter).unwrap_or(""),
        );

        scratch.clear();
        fmt::age(scratch, now, row.time);
        // Deliberately not `abs()`: a *negative* age is a spacecraft clock ahead of the
        // ground, which is a wrong epoch and not a stale value. Greying it would hide the one
        // display that shows the epoch is wrong on the first packet — the minus sign the age
        // itself carries.
        let stale = now.secs_since(row.time) >= fmt::AGE_STALE_SECONDS;
        ui.allocate_ui_with_layout(
            egui::vec2(AGE_WIDTH, ROW_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                if stale {
                    let weak = ui.visuals().weak_text_color();
                    ui.add(egui::Label::new(egui::RichText::new(&*scratch).color(weak)).truncate());
                } else {
                    ui.add(egui::Label::new(&*scratch).truncate());
                }
            },
        );

        let limit_colour = fmt::limit_color(row.limit, ui.visuals());
        ui.allocate_ui_with_layout(
            egui::vec2(LIMIT_WIDTH, ROW_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(row.limit.label()).color(limit_colour))
                        .truncate(),
                )
            },
        )
        .inner
        .on_hover_text(match row.limit {
            LimitState::Unknown => "no limit is defined for this parameter",
            LimitState::Nominal => "inside every limit",
            LimitState::Warning => "outside a warning limit",
            LimitState::Alarm => "outside an alarm limit",
        });
    });
    action
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use xtce_gs_core::{Limit, Range, Sample};

    use super::*;

    /// Four parameters in two space systems, which is enough for a name order to be visible.
    const DEFINITION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SpaceSystem xmlns="http://www.omg.org/spec/XTCE/20180204" name="Sat">
  <TelemetryMetaData>
    <ParameterTypeSet>
      <FloatParameterType name="Kelvin" sizeInBits="32">
        <UnitSet><Unit form="calibrated">K</Unit></UnitSet>
        <FloatDataEncoding sizeInBits="32"/>
      </FloatParameterType>
    </ParameterTypeSet>
    <ParameterSet>
      <Parameter name="TEMP_A" parameterTypeRef="Kelvin"/>
      <Parameter name="TEMP_B" parameterTypeRef="Kelvin"/>
    </ParameterSet>
    <ContainerSet/>
  </TelemetryMetaData>
  <SpaceSystem name="Power">
    <TelemetryMetaData>
      <ParameterTypeSet>
        <FloatParameterType name="Volts" sizeInBits="32">
          <UnitSet><Unit form="calibrated">V</Unit></UnitSet>
          <FloatDataEncoding sizeInBits="32"/>
        </FloatParameterType>
      </ParameterTypeSet>
      <ParameterSet>
        <Parameter name="BATT_V" parameterTypeRef="Volts"/>
        <Parameter name="BUS_V" parameterTypeRef="Volts"/>
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

    fn file(store: &mut ParameterStore, parameter: ParamId, value: f64, at: i64) {
        store.push(Sample {
            parameter,
            time: Utc::from_unix_secs(at),
            raw: Value::Unsigned(value as u64),
            eng: Value::Float(value),
        });
    }

    fn names(table: &Table, db: &XtceDb) -> Vec<String> {
        table
            .rows()
            .iter()
            .map(|row| fmt::qualified_name_of(db, row.parameter).to_owned())
            .collect()
    }

    #[test]
    fn only_the_parameters_that_have_arrived_are_listed() {
        let db = db();
        let mut store = store(&db);
        file(&mut store, id(&db, "TEMP_A"), 300.0, 100);

        let mut table = Table::new();
        table.refresh(&store, &db, &LimitSet::new());

        assert_eq!(names(&table, &db), ["/Sat/TEMP_A"]);
        assert_eq!(table.rows().len(), 1, "the other three slots are empty");
    }

    #[test]
    fn a_refresh_replaces_the_rows_rather_than_appending_to_them() {
        let db = db();
        let mut store = store(&db);
        file(&mut store, id(&db, "TEMP_A"), 300.0, 100);

        let mut table = Table::new();
        table.refresh(&store, &db, &LimitSet::new());
        table.refresh(&store, &db, &LimitSet::new());
        assert_eq!(table.rows().len(), 1);
    }

    #[test]
    fn the_filter_matches_the_qualified_name_whatever_its_case() {
        let db = db();
        let mut store = store(&db);
        file(&mut store, id(&db, "TEMP_A"), 300.0, 100);
        file(&mut store, id(&db, "BATT_V"), 3.3, 100);

        let mut table = Table::new();
        table.filter_mut().push_str("power");
        table.refresh(&store, &db, &LimitSet::new());
        assert_eq!(names(&table, &db), ["/Sat/Power/BATT_V"]);

        table.filter_mut().clear();
        table.filter_mut().push_str("temp_a");
        table.refresh(&store, &db, &LimitSet::new());
        assert_eq!(names(&table, &db), ["/Sat/TEMP_A"]);

        table.filter_mut().clear();
        table.filter_mut().push_str("nothing matches this");
        table.refresh(&store, &db, &LimitSet::new());
        assert!(table.rows().is_empty());
    }

    #[test]
    fn watched_only_hides_what_is_not_kept() {
        let db = db();
        let mut store = store(&db);
        store.watch(id(&db, "BATT_V"));
        file(&mut store, id(&db, "TEMP_A"), 300.0, 100);
        file(&mut store, id(&db, "BATT_V"), 3.3, 100);

        let mut table = Table::new();
        table.set_watched_only(true);
        table.refresh(&store, &db, &LimitSet::new());
        assert_eq!(names(&table, &db), ["/Sat/Power/BATT_V"]);
    }

    #[test]
    fn the_default_order_is_the_name_and_it_groups_by_space_system() {
        let db = db();
        let mut store = store(&db);
        file(&mut store, id(&db, "TEMP_B"), 1.0, 100);
        file(&mut store, id(&db, "BUS_V"), 1.0, 100);
        file(&mut store, id(&db, "TEMP_A"), 1.0, 100);
        file(&mut store, id(&db, "BATT_V"), 1.0, 100);

        let mut table = Table::new();
        table.refresh(&store, &db, &LimitSet::new());
        assert_eq!(
            names(&table, &db),
            [
                "/Sat/Power/BATT_V",
                "/Sat/Power/BUS_V",
                "/Sat/TEMP_A",
                "/Sat/TEMP_B",
            ]
        );
    }

    #[test]
    fn the_first_click_on_the_age_column_puts_the_newest_first() {
        let db = db();
        let mut store = store(&db);
        file(&mut store, id(&db, "TEMP_A"), 1.0, 100);
        file(&mut store, id(&db, "TEMP_B"), 1.0, 500);

        let mut table = Table::new();
        table.set_sort(Sort::Time);
        assert!(!table.is_descending(), "a fresh column is not reversed");
        table.refresh(&store, &db, &LimitSet::new());
        assert_eq!(
            names(&table, &db),
            ["/Sat/TEMP_B", "/Sat/TEMP_A"],
            "the youngest age is the most recent sample"
        );
    }

    #[test]
    fn the_first_click_on_the_limit_column_puts_the_worst_first() {
        let db = db();
        let mut store = store(&db);
        let mut limits = LimitSet::new();
        limits.insert(
            "/Sat/TEMP_A",
            Limit {
                warning: Range::new(0.0, 10.0),
                alarm: Range::new(-10.0, 20.0),
            },
        );
        limits.insert(
            "/Sat/TEMP_B",
            Limit {
                warning: Range::new(0.0, 10.0),
                alarm: Range::new(-10.0, 20.0),
            },
        );
        // TEMP_A is nominal, TEMP_B is in alarm, and the name order would put A first.
        file(&mut store, id(&db, "TEMP_A"), 5.0, 100);
        file(&mut store, id(&db, "TEMP_B"), 99.0, 100);
        file(&mut store, id(&db, "BATT_V"), 3.3, 100);

        let mut table = Table::new();
        table.set_sort(Sort::Limit);
        table.refresh(&store, &db, &limits);
        assert_eq!(
            names(&table, &db),
            ["/Sat/TEMP_B", "/Sat/TEMP_A", "/Sat/Power/BATT_V"],
            "alarm, then nominal, then the one with no limit at all"
        );
    }

    #[test]
    fn a_second_click_on_the_same_column_reverses_it() {
        let db = db();
        let mut store = store(&db);
        file(&mut store, id(&db, "TEMP_A"), 1.0, 100);
        file(&mut store, id(&db, "TEMP_B"), 1.0, 500);

        let mut table = Table::new();
        table.set_sort(Sort::Time);
        table.set_sort(Sort::Time);
        assert!(table.is_descending());
        table.refresh(&store, &db, &LimitSet::new());
        assert_eq!(names(&table, &db), ["/Sat/TEMP_A", "/Sat/TEMP_B"]);
    }

    #[test]
    fn a_non_numeric_value_is_listed_and_has_no_limit_state() {
        let db = db();
        let mut store = store(&db);
        let mut limits = LimitSet::new();
        limits.insert(
            "/Sat/TEMP_A",
            Limit {
                warning: Range::new(0.0, 10.0),
                alarm: Range::new(-10.0, 20.0),
            },
        );
        store.push(Sample {
            parameter: id(&db, "TEMP_A"),
            time: Utc::from_unix_secs(1),
            raw: Value::Unsigned(2),
            eng: Value::Label(Arc::from("SAFE")),
        });

        let mut table = Table::new();
        table.refresh(&store, &db, &limits);
        let row = table
            .rows()
            .first()
            .expect("the label is a row like any other");
        assert_eq!(row.limit, LimitState::Unknown);
        assert_eq!(row.eng.as_str(), Some("SAFE"));
    }

    #[test]
    fn an_empty_table_draws_without_taking_the_window_down() {
        // `ScrollArea::show_rows` with a total of zero is the first frame of every session.
        let db = db();
        let mut table = Table::new();
        let mut scratch = String::new();
        egui::__run_test_ui(|ui| {
            let action = show(
                ui,
                &mut table,
                &db,
                Utc::from_unix_secs(1_000),
                true,
                1,
                &mut scratch,
            );
            assert!(action.is_none());
        });
    }

    #[test]
    fn a_table_of_rows_draws_and_asks_for_nothing_on_its_own() {
        let db = db();
        let mut store = store(&db);
        file(&mut store, id(&db, "TEMP_A"), 300.0, 100);
        file(&mut store, id(&db, "BATT_V"), 3.3, 100);
        let mut table = Table::new();
        table.refresh(&store, &db, &LimitSet::new());

        let mut scratch = String::new();
        egui::__run_test_ui(|ui| {
            let action = show(
                ui,
                &mut table,
                &db,
                Utc::from_unix_secs(1_000),
                false,
                1,
                &mut scratch,
            );
            assert!(action.is_none(), "nothing was clicked");
        });
        assert_eq!(table.rows().len(), 2, "drawing does not change the rows");
    }
}
