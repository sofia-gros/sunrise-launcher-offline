use super::*;

fn work(clearing: bool) -> Work {
    Work {
        cancel: storage::Cancel::default(),
        clearing,
    }
}

#[test]
fn clear_is_offered_whatever_is_known_of_sizes() {
    assert!(clear_offered(None), "nothing counted yet");
    assert!(clear_offered(Some(&work(false))), "a count is in flight");
    assert!(
        !clear_offered(Some(&work(true))),
        "a Clear is already running"
    );
}

#[test]
fn the_question_asks_without_a_size_when_the_rows_are_not_counted() {
    for asked in [None, Some(Category::OtherCaches)] {
        let unsized_question = clear_question(asked, None);
        assert!(unsized_question.contains("Close the game first."));
        assert!(!unsized_question.contains("Up to"), "{unsized_question}");
        assert!(!unsized_question.contains("{size}"), "{unsized_question}");
        let sized = clear_question(asked, Some(2_000_000));
        assert!(sized.contains("Close the game first."));
        assert!(sized.contains("2.0 MB"), "{sized}");
    }
}

#[test]
fn idle_clear_explains_only_a_wholly_protected_row() {
    let line = kept_line(2_000_000, 0, false, true, false).unwrap();
    assert_eq!(
        line,
        "2.0 MB is protected and can't be cleared. See the launcher log for details."
    );
    assert_eq!(kept_line(2_000_000, 1_000_000, false, true, false), None);
}

#[test]
fn clear_reports_one_total_for_all_kept_units_in_a_row() {
    let kept = [1_000_000, 2_000_000];
    let line = kept_line(kept.iter().sum(), 0, true, true, true).unwrap();
    assert_eq!(
        line,
        "3.0 MB stayed. Remove older game builds if you no longer use them, then try again."
    );
}
