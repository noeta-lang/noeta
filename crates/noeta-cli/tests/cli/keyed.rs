//! Keyed-list **structural** reconciliation for the `para.html` LiveView package, driven headlessly
//! through `noeta run` (no socket needed — the reconciliation is pure Noeta over `std.reactive`).
//!
//! `examples/para-html/liveview-structural/structural_demo.noe` exercises two levels: the diff **algorithm**
//! (`keyed_op_stream` — the minimal insert/remove/move plan for a key-order change) and the
//! **reconciliation** the websocket session runs (`reconcile_region` over a real `view()`, asserting
//! the wire frame per append/prepend/remove/reorder and that a reorder leaves row content untouched).
//! `events_demo.noe` is the pre-existing per-row *content* path — a regression guard that keyed lists
//! still route inline handlers and reactive row markup after the structural rework.

use crate::support::*;

/// The structural demo prints its algorithm op streams, then a reconcile section whose every line is
/// `true` — the minimal frames landed and the reorder re-rendered nothing.
#[test]
fn keyed_structural_reconciliation_emits_minimal_ops() {
    lang()
        .current_dir(workspace().join("examples/para-html/liveview-structural"))
        .arg("run")
        .arg("structural_demo.noe")
        .assert()
        .success()
        .stdout(
            "\n\
             ins d @end\n\
             ins z a\n\
             rm b\n\
             mv b d\n\
             mv a @end; mv b a\n\
             rm b; ins d @end; mv a d\n\
             --- reconcile ---\n\
             append: true\n\
             prepend: true\n\
             remove: true\n\
             reorder: true\n\
             reorder-content-untouched: true\n",
        );
}

/// The per-row content path (keyed rows with inline `on_click`, reactive row markup) still works
/// after the structural rework — unchanged output from the events demo.
#[test]
fn keyed_per_row_content_and_inline_handlers_still_route() {
    lang()
        .current_dir(workspace().join("examples/para-html/liveview-events"))
        .arg("run")
        .arg("events_demo.noe")
        .assert()
        .success()
        .stdout("true\ntrue\n2\n0\nAda\ntrue\ntrue\nfalse\n");
}

/// The structural showcase app (buttons that append/prepend/remove/reorder a keyed list) type-checks
/// — a compile guard for the runnable demo referenced from the LiveView docs.
#[test]
fn keyed_structural_showcase_app_type_checks() {
    lang()
        .current_dir(workspace().join("examples/para-html/liveview-structural-app"))
        .arg("check")
        .arg("app.noe")
        .assert()
        .success();
}
