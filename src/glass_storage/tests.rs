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
    assert!(!clear_offered(Some(&work(true))), "a Clear is already running");
}

#[test]
fn the_question_asks_without_a_size_when_the_rows_are_not_counted() {
    for asked in [None, Some(Category::OtherCaches)] {
        let unsized_question = clear_question(asked, None);
        assert!(!unsized_question.contains("Up to"), "{unsized_question}");
        assert!(!unsized_question.contains("{size}"), "{unsized_question}");
        let sized = clear_question(asked, Some(2_000_000));
        assert!(sized.contains("2.0 MB"), "{sized}");
    }
}
