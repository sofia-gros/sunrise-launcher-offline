use super::*;

/// A workspace with a `work/build.log` holding `text`.
fn workspace(text: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    fs::create_dir(&work).unwrap();
    let log = work.join("build.log");
    fs::write(&log, text).unwrap();
    (dir, log)
}

#[test]
fn the_tail_keeps_the_last_lines_and_skips_blank_ones() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("build.log");
    fs::write(&log, "a\n\nb\r\n   \nc\nd\n").unwrap();
    assert_eq!(tail(&log, 2).as_deref(), Some("c\nd"));
    assert_eq!(tail(&log, 10).as_deref(), Some("a\nb\nc\nd"));
    fs::write(&log, "\n \n").unwrap();
    assert_eq!(tail(&log, 3), None);
    assert_eq!(tail(&dir.path().join("missing.log"), 3), None);
}

#[test]
fn a_long_log_is_read_from_its_end_without_its_partial_first_line() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("build.log");
    let body: String = (0..20_000).map(|n| format!("line {n}\n")).collect();
    fs::write(&log, body).unwrap();
    assert_eq!(
        tail(&log, 3).as_deref(),
        Some("line 19997\nline 19998\nline 19999")
    );
}

#[test]
fn one_line_longer_than_the_read_still_shows_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("build.log");
    for ending in ["", "\n", "\r\n", "\n \t\n"] {
        fs::write(&log, format!("start\n{}end{ending}", "x".repeat(100_000))).unwrap();
        let shown = tail(&log, 3).expect("a long line should still show its end");
        assert!(shown.ends_with("xend"));
        assert_eq!(shown.len() as u64, LIMIT - ending.len() as u64);
    }
}

#[test]
fn the_guard_names_the_log_only_while_held() {
    let slot = Slot(Mutex::new(None));
    let (dir, log) = workspace("compiling\n");
    {
        let _live = slot.name(dir.path().to_owned(), log.clone());
        assert_eq!(slot.tail(5).as_deref(), Some("compiling"));
    }
    assert_eq!(slot.tail(5), None);
}

#[test]
fn an_earlier_guard_leaves_a_later_name_alone() {
    let slot = Slot(Mutex::new(None));
    let (first_dir, first) = workspace("first\n");
    let (second_dir, second) = workspace("second\n");
    let earlier = slot.name(first_dir.path().to_owned(), first);
    let _later = slot.name(second_dir.path().to_owned(), second);
    drop(earlier);
    assert_eq!(slot.tail(5).as_deref(), Some("second"));
}

#[test]
fn a_log_outside_its_workspace_is_not_read() {
    let slot = Slot(Mutex::new(None));
    let (_dir, log) = workspace("elsewhere\n");
    let other = tempfile::tempdir().unwrap();
    let _live = slot.name(other.path().to_owned(), log);
    assert_eq!(slot.tail(5), None);
}

#[cfg(unix)]
#[test]
fn a_link_in_place_of_the_log_is_not_followed() {
    let slot = Slot(Mutex::new(None));
    let (dir, log) = workspace("");
    let secret = dir.path().join("secret");
    fs::write(&secret, "private\n").unwrap();
    fs::remove_file(&log).unwrap();
    std::os::unix::fs::symlink(&secret, &log).unwrap();
    let _live = slot.name(dir.path().to_owned(), log.clone());
    assert_eq!(slot.tail(5), None);
    assert_eq!(tail(&log, 5), None);
}

#[cfg(unix)]
#[test]
fn a_pipe_in_place_of_the_log_is_refused_without_waiting() {
    use std::os::unix::ffi::OsStrExt as _;
    let slot = Slot(Mutex::new(None));
    let (dir, log) = workspace("");
    fs::remove_file(&log).unwrap();
    let name = std::ffi::CString::new(log.as_os_str().as_bytes()).unwrap();
    // SAFETY: `name` is a valid NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let _live = slot.name(dir.path().to_owned(), log.clone());
    // Without a writer, a blocking open would never return; both must come back empty.
    assert_eq!(slot.tail(5), None);
    assert_eq!(tail(&log, 5), None);
}
