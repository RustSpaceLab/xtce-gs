//! The interface, driven by a real session, with no window.
//!
//! Every panel's `refresh` takes core and engine types and no `egui::Context`, so the half of
//! the interface that can be wrong about *data* — an empty parameter tree, a table of no rows,
//! a plot with no points — is testable here. What is not testable here is pixels: that a row
//! is drawn where the layout says, in the colour the theme picked. Those need a window, and a
//! window needs a display.
//!
//! This is the gap `TODO(gs-gui-app)` describes from the other side. It closes the part of it
//! that does not need `App` to be restructured.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use xtce_gs_core::LimitSet;
use xtce_gs_engine::{Session, SessionConfig};
use xtce_gs_gui::Layout;
use xtce_gs_gui::panels::tree::Tree;
use xtce_gs_gui::panels::{events::Events, plots, plots::Plots, status::Status, table::Table};

/// The JPSS definition and recording, vendored here.
///
/// Vendored in this repository since 2026-09-13 — see `testdata/SOURCES.md`.
fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/jpss")
        .join(name)
}

/// Runs a replay to completion and hands back the session it filled.
fn replay() -> (tokio::runtime::Runtime, Session) {
    let definition = testdata("jpss1_geolocation_xtce_v1.xml");
    let recording = testdata("J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1");
    // Both are in this repository. This used to return `None` and let the one test below pass
    // without opening a window on anything, which made the only end-to-end cover the interface
    // has into a test that could not fail.
    assert!(
        definition.is_file() && recording.is_file(),
        "{} and {} are vendored here; see testdata/SOURCES.md",
        definition.display(),
        recording.display()
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    let source = format!("file://{}?rate=0", recording.display())
        .parse()
        .expect("source");
    let config = SessionConfig {
        definition,
        source,
        history_depth: 4096,
        ..SessionConfig::default()
    };

    let session = Session::start(config, runtime.handle(), None).expect("session");
    (runtime, session)
}

#[test]
fn every_panel_fills_from_a_session_that_really_decoded_a_downlink() {
    let (_runtime, session) = replay();

    let db = session.db().clone();

    // The real start-up order, and the reason this test is shaped like this: `App::new`
    // restores the layout *before* the first packet arrives. A parameter's history begins at
    // the moment it is watched, so a test that watched after the replay had finished would
    // assert an empty plot and call it a pass.
    let plotted = db
        .parameters()
        .iter()
        .enumerate()
        .map(|(index, _)| xtce_model::ParamId::new(index as u32))
        .find(|&id| {
            matches!(
                db.type_of(id).map(|kind| &kind.kind),
                Some(xtce_model::TypeKind::Integer | xtce_model::TypeKind::Float)
            )
        })
        .expect("the definition declares something numeric");
    let name = db
        .name(db.parameter(plotted).expect("parameter").qualified_name)
        .to_owned();

    let mut layout = Layout::default();
    layout.new_plot("test", name.clone());
    {
        let mut store = session.store().write().expect("store");
        assert_eq!(
            layout.restore(&db, &mut store),
            1,
            "a plot naming {name} watched nothing"
        );
    }

    // Now let the replay run.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if session.stats().snapshot().packets_decoded >= 7200 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let snapshot = session.stats().snapshot();
    assert_eq!(
        snapshot.packets_decoded, 7200,
        "the session did not decode the recording; nothing below means anything"
    );

    // 1. The tree. The operator's first question about a definition is which parameters ever
    //    come down, so `seen` is the field that matters, not the row count.
    let mut tree = Tree::new();
    {
        let store = session.store().read().expect("store");
        tree.refresh(&db, &store);
    }
    assert_eq!(
        tree.matches().len(),
        db.parameters().len(),
        "an empty search must match every parameter"
    );
    let seen = tree.matches().iter().filter(|entry| entry.seen).count();
    assert!(
        seen >= 27,
        "the JPSS container has 27 parameters and the tree marked {seen} as seen"
    );
    assert!(
        tree.matches().iter().any(|entry| entry.plottable),
        "nothing in the tree can be plotted, so no plot can ever be opened"
    );

    // 2. The table.
    let mut table = Table::new();
    {
        let store = session.store().read().expect("store");
        table.refresh(&store, &db, &LimitSet::new());
    }
    assert!(
        table.rows().len() >= 27,
        "the table has {} rows after 7200 packets",
        table.rows().len()
    );

    // 3. The plots, filled from the ring that was allocated before the first packet.
    let mut plots = Plots::new();
    plots.sync(&layout, &db);
    {
        let store = session.store().read().expect("store");
        plots.refresh(
            &store,
            plots::View {
                x_range: None,
                paused: false,
                width: 1000.0,
            },
        );
    }
    assert_eq!(
        plots.groups().len(),
        1,
        "the plot group did not survive sync"
    );
    let points: usize = plots
        .groups()
        .iter()
        .flatten()
        .map(plots::Series::len)
        .sum();
    assert!(
        points > 0,
        "the plot drew nothing from 7200 packets of a parameter it was watching from the \
         first one — this is the failure that looks like a working station"
    );
    assert!(
        points <= xtce_gs_gui::panels::plots::pixel_budget(1000.0),
        "the plot handed {points} points to a widget 1000 pixels wide"
    );

    // 4. The status bar and the log, which do not need history.
    let mut status = Status::new();
    status.refresh(session.stats(), session.is_running());
    let mut events = Events::new();
    {
        let log = session.events().lock().expect("events");
        events.refresh(&log);
    }
    assert!(
        events.visible().count() > 0,
        "a session that opened a file and decoded 7200 packets logged nothing"
    );

    session.shutdown();
    eprintln!(
        "tree {} rows ({seen} seen), table {} rows, plot points {points}, log {} lines",
        tree.matches().len(),
        table.rows().len(),
        events.visible().count()
    );
}
