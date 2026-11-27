//! Storage inventory: rows count on a background worker and fill in as each finishes;
//! filesystem work never runs on the render thread.
use super::*;

use storage::Category;

#[cfg(test)]
mod tests;

/// The one worker in flight: a count, which leaving the page stops, or a Clear, which it never
/// does.
pub(crate) struct Work {
    cancel: storage::Cancel,
    clearing: bool,
}

/// Whether Clear can start independently of any count in flight.
pub(crate) fn clear_offered(work: Option<&Work>) -> bool {
    !work.is_some_and(|work| work.clearing)
}

/// A row says nothing about partial protection until a Clear has tried it.
pub(super) fn kept_line(
    remaining: u64,
    clearable: u64,
    after_clear: bool,
    has_refusals: bool,
    older_build_kept: bool,
) -> Option<String> {
    if remaining == 0 {
        return (after_clear && has_refusals).then(|| {
            tr("Some files couldn't be cleared. See the launcher log for details.").into()
        });
    }
    if !after_clear && clearable > 0 {
        return None;
    }
    let size = storage::format_bytes(remaining);
    Some(if !after_clear {
        trf(
            "{size} is protected and can't be cleared. See the launcher log for details.",
            &[("size", &size)],
        )
    } else if older_build_kept {
        trf(
            "{size} stayed. Remove older game builds if you no longer use them, then try again.",
            &[("size", &size)],
        )
    } else {
        trf("{size} stayed. Close the game, wait for any builds to finish, then try again. See the launcher log if files still remain.", &[("size", &size)])
    })
}

/// What the confirmation asks: with the size to free when the rows asked about are counted, and
/// without one when they are not.
pub(crate) fn clear_question(asked: Option<Category>, bytes: Option<u64>) -> String {
    let size = bytes.map(storage::format_bytes);
    match (asked, size) {
        (None, Some(size)) => trf(
            "Clear everything kept for reuse? Up to {size} is freed. Close the game first. Files a game or build is using stay.",
            &[("size", &size)],
        ),
        (None, None) => tr(
            "Clear everything kept for reuse? Close the game first. Files a game or build is using stay.",
        )
        .into(),
        (Some(category), Some(size)) => trf(
            "Clear {category}? Up to {size} is freed. Close the game first. Files a game or build is using stay.",
            &[
                ("category", category_label(category)),
                ("size", &size),
            ],
        ),
        (Some(category), None) => trf(
            "Clear {category}? Close the game first. Files a game or build is using stay.",
            &[("category", category_label(category))],
        ),
    }
}

impl settings::SettingsView {
    /// Reject queued outcomes from a cancelled or superseded worker before changing the view.
    fn accept_storage(
        &mut self,
        token: &storage::Cancel,
        result: &Result<Box<storage::Report>, String>,
    ) -> Option<bool> {
        let work = self
            .storage_work
            .as_ref()
            .filter(|work| work.cancel.is(token))?;
        let clearing = work.clearing;
        if result.as_ref().map_or(true, |report| report.is_complete()) {
            self.storage_work = None;
        }
        Some(clearing)
    }
}

impl Launcher {
    /// Counts every row (`clear` and `all` unset), or clears one row or all of them. A count
    /// shows nothing it counted before: each row says "Counting…" until its own count is in.
    /// A Clear overtakes a count in flight and never waits for one.
    pub(super) fn refresh_storage(&mut self, clear: Option<Category>, all: bool) {
        let clearing = clear.is_some() || all;
        match self.ui.settings.storage_work.take() {
            Some(work) if work.clearing || !clearing => {
                self.ui.settings.storage_work = Some(work);
                return;
            }
            Some(work) => work.cancel.cancel(),
            None => {}
        }
        let cancel = storage::Cancel::default();
        self.ui.settings.storage_work = Some(Work {
            cancel: cancel.clone(),
            clearing,
        });
        self.storage_error = None;
        self.storage_report = None;
        if clearing {
            self.ui.settings.storage_clear = None;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let send = |result: Result<Box<storage::Report>, String>| {
                let _ = tx.send(Msg::Storage(cancel.clone(), result));
            };
            let result = storage::Storage::open().and_then(|storage| {
                if clearing {
                    storage.report(clear, all).map(Some)
                } else {
                    // The last row arrives as the whole report, below.
                    storage.count(&cancel, |rows| {
                        if !rows.is_complete() {
                            send(Ok(Box::new(rows.clone())));
                        }
                    })
                }
            });
            match result {
                Ok(Some(report)) => send(Ok(Box::new(report))),
                // Cancelled: the window has moved on and wants nothing from this worker.
                Ok(None) => {}
                Err(_) if cancel.is_cancelled() => {}
                Err(error) => send(Err(native_mods::cache_error(error))),
            }
        });
    }

    /// Rows counted so far, or the finished count or Clear, from the worker.
    pub(super) fn storage_arrived(
        &mut self,
        token: storage::Cancel,
        result: Result<Box<storage::Report>, String>,
    ) {
        let Some(clearing) = self.ui.settings.accept_storage(&token, &result) else {
            return;
        };
        if clearing {
            self.ui.settings.storage_clear = Some(result);
            if self.ui.page == frame::Page::Settings
                && self.ui.settings.section == settings::Section::Storage
            {
                self.refresh_storage(None, false);
            }
            return;
        }
        match result {
            Ok(report) => self.storage_report = Some(*report),
            Err(error) => self.storage_error = Some(error),
        }
    }

    /// Starts the initial count if no entry or background refresh has started one yet.
    pub(super) fn resume_storage_count(&mut self) {
        if self.storage_report.is_none() && self.storage_error.is_none() {
            self.refresh_storage(None, false);
        }
    }

    /// Leaving Storage cancels what is still being counted, the installed builds' sizes too. A
    /// Clear in flight runs to its end.
    pub(super) fn stop_storage_work(&mut self) {
        if let Some(count) = self.ui.settings.storage_work.take_if(|work| !work.clearing) {
            count.cancel.cancel();
        }
        self.stop_storage_builds_count();
    }
}

pub(crate) fn category_label(category: Category) -> &'static str {
    match category {
        Category::BuildFiles => tr("Build files"),
        Category::ModBuildCache => tr("Mod build cache"),
        Category::PrebuiltGames => tr("Prebuilt games"),
        Category::SpareGameBuilds => tr("Spare game builds"),
        Category::RustCompiler => tr("Rust compiler"),
        Category::OtherCaches => tr("Other caches"),
    }
}

/// What a row holds, and that clearing it costs only a download or a rebuild.
pub(crate) fn category_about(category: Category) -> &'static str {
    match category {
        Category::BuildFiles => tr("What building native mods leaves behind: made again when needed."),
        Category::ModBuildCache => tr("Compiled parts kept between mod builds so changes finish faster: built again when needed."),
        Category::PrebuiltGames => {
            tr("The game packages native mods are built against: downloaded again when needed.")
        }
        Category::SpareGameBuilds => {
            tr("Game builds kept for running servers: downloaded again when needed.")
        }
        Category::RustCompiler => {
            tr("The compiler that builds native mods: downloaded again the next time one is built.")
        }
        Category::OtherCaches => tr("Other downloads kept to make the launcher faster."),
    }
}

#[cfg(test)]
#[test]
fn storage_busy_follows_the_active_worker_slot() {
    let mut view = settings::SettingsView::default();
    assert!(!view.storage_busy());
    for clearing in [false, true] {
        view.storage_work = Some(Work {
            cancel: storage::Cancel::default(),
            clearing,
        });
        assert!(view.storage_busy());
        assert_eq!(clear_offered(view.storage_work.as_ref()), !clearing);
        view.storage_work.take();
        assert!(!view.storage_busy());
    }
}
