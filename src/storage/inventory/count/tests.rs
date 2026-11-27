use super::*;
use crate::storage::tests::Home;
use crate::storage::{access_calls, entries_calls, walks};

#[test]
fn a_count_discovers_once_walks_each_unit_once_and_classifies_nothing() {
    let home = Home::new();
    home.seed("rust/old/payload");
    home.seed("native/source/rev/lib.rs");
    home.seed("native/target/debug/output");
    let storage = Storage::at(&home.0).unwrap();
    let found = storage.entries().unwrap().len();
    let before = (walks(), entries_calls(), access_calls());
    let mut handed_over = Vec::new();
    let report = storage
        .count(&Cancel::default(), |report| {
            handed_over.push(report.rows.len());
        })
        .unwrap()
        .unwrap();
    let after = storage.entries().unwrap().len();
    assert_eq!(entries_calls() - before.1, 2, "one for the count, one here");
    assert!(
        (found.min(after)..=found.max(after)).contains(&(walks() - before.0)),
        "each unit is walked once, not again for its notes"
    );
    assert_eq!(access_calls(), before.2, "counting never classifies a unit");
    assert_eq!(handed_over, [1, 2, 3, 4, 5]);
    assert!(report.is_complete());
    assert!(report.rows.iter().all(|row| row.freed_bytes == 0));
    assert!(report.total_bytes > 0);
}

#[test]
fn a_count_hands_over_each_row_as_it_finishes() {
    let home = Home::new();
    home.seed("rust/old/payload");
    let storage = Storage::at(&home.0).unwrap();
    let cancel = Cancel::default();
    let mut order = Vec::new();
    let report = storage
        .count(&cancel, |report| {
            order.push(report.rows.last().unwrap().category);
            assert!(report.is_from(&cancel));
            assert!(!report.is_from(&Cancel::default()));
        })
        .unwrap()
        .unwrap();
    assert_eq!(order, Category::ALL);
    assert_eq!(report.rows.len(), Category::ALL.len());
}

#[test]
fn cancelling_a_count_stops_it_before_the_next_row() {
    let home = Home::new();
    home.seed("native/source/rev/lib.rs");
    home.seed("rust/old/payload");
    let storage = Storage::at(&home.0).unwrap();
    let cancel = Cancel::default();
    let mut handed_over = 0;
    let result = storage
        .count(&cancel, |_| {
            handed_over += 1;
            cancel.cancel();
        })
        .unwrap();
    assert!(result.is_none(), "a cancelled count has no whole report");
    assert_eq!(handed_over, 1);
}

#[test]
fn a_count_cancelled_before_it_starts_discovers_and_walks_nothing() {
    let home = Home::new();
    home.seed("rust/old/payload");
    let storage = Storage::at(&home.0).unwrap();
    let cancel = Cancel::default();
    cancel.cancel();
    let before = (walks(), entries_calls());
    assert!(storage.count(&cancel, |_| panic!("no row")).unwrap().is_none());
    assert_eq!((walks(), entries_calls()), before);
}

#[test]
fn a_walk_cancelled_part_way_stops_descending() {
    let home = Home::new();
    home.seed("rust/a/b/c/payload");
    let cancel = Cancel::default();
    let whole = Tree::build_until(&home.0.join("rust"), &cancel).unwrap();
    assert!(whole.size(&mut HashSet::new()) > 0);
    cancel.cancel();
    let stopped = Tree::build_until(&home.0.join("rust"), &cancel).unwrap();
    assert_eq!(stopped.size(&mut HashSet::new()), 0);
}

#[test]
fn clear_asks_for_no_count_and_survives_a_cancelled_one() {
    let home = Home::new();
    let payload = home.seed("rust/old/payload");
    let storage = Storage::at(&home.0).unwrap();
    let cancel = Cancel::default();
    cancel.cancel();
    assert!(storage.count(&cancel, |_| {}).unwrap().is_none());
    let report = storage.report(Some(Category::RustCompiler), false).unwrap();
    let row = &report.rows[3];
    assert_eq!(row.category, Category::RustCompiler);
    assert!(row.freed_bytes > 0, "Clear measures what it frees itself");
    assert!(!payload.exists());
}

#[test]
fn a_count_reports_a_refused_unit_on_its_row_without_holding_up_the_rest() {
    let home = Home::new();
    let kept = home.seed("rust/old/payload");
    home.seed("worlds/keep");
    #[cfg(unix)]
    {
        home.seed("native/source/rev/lib.rs");
        std::os::unix::fs::symlink(home.0.join("worlds"), home.0.join("native/source/rev/link"))
            .unwrap();
    }
    let storage = Storage::at(&home.0).unwrap();
    let report = storage.report(None, false).unwrap();
    assert!(report.rows[3].bytes > 0);
    #[cfg(unix)]
    assert_eq!(report.rows[0].kept.len(), 1);
    assert!(kept.exists());
    assert!(home.0.join("worlds/keep").exists());
}
