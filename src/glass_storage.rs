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
                ("category", tr(category_label(category))),
                ("size", &size),
            ],
        ),
        (Some(category), None) => trf(
            "Clear {category}? Close the game first. Files a game or build is using stay.",
            &[("category", tr(category_label(category)))],
        ),
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
        self.storage_pending = true;
        self.storage_error = None;
        if !clearing {
            self.storage_report = None;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let send = |result: Result<Box<storage::Report>, String>| {
                let _ = tx.send(Msg::Storage(result));
            };
            let result = storage::Storage::open().and_then(|storage| {
                if clearing {
                    storage
                        .report(clear, all)
                        .map(|report| Some(report.tagged(&cancel)))
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
    pub(super) fn storage_arrived(&mut self, result: Result<Box<storage::Report>, String>) {
        let work = self.ui.settings.storage_work.as_ref();
        match result {
            Ok(report) => {
                // What a count that was cancelled or overtaken sent before it noticed is dropped.
                if !work.is_some_and(|work| report.is_from(&work.cancel)) {
                    return;
                }
                if report.is_complete() {
                    self.ui.settings.storage_work = None;
                    self.storage_pending = false;
                }
                self.storage_report = Some(*report);
                self.storage_error = None;
            }
            Err(error) => {
                self.ui.settings.storage_work = None;
                self.storage_pending = false;
                self.storage_error = Some(error);
            }
        }
    }

    /// Counts the rows again when the page is shown with some of them missing: an earlier
    /// count was stopped by leaving it. A failed count waits for an explicit Recount.
    pub(super) fn resume_storage_count(&mut self) {
        let incomplete = self
            .storage_report
            .as_ref()
            .is_none_or(|report| !report.is_complete());
        if incomplete && !self.storage_pending && self.storage_error.is_none() {
            self.refresh_storage(None, false);
        }
    }

    /// Leaving Storage cancels what is still being counted, the installed builds' sizes too. A
    /// Clear in flight runs to its end.
    pub(super) fn stop_storage_work(&mut self) {
        if let Some(count) = self.ui.settings.storage_work.take_if(|work| !work.clearing) {
            count.cancel.cancel();
            self.storage_pending = false;
        }
        self.stop_storage_builds_count();
    }
}

pub(crate) fn category_label(category: Category) -> &'static str {
    match category {
        Category::BuildFiles => "Build files",
        Category::PrebuiltGames => "Prebuilt games",
        Category::SpareGameBuilds => "Spare game builds",
        Category::RustCompiler => "Rust compiler",
        Category::OtherCaches => "Other caches",
    }
}

/// What a row holds, and that clearing it costs only a download or a rebuild.
pub(crate) fn category_about(category: Category) -> &'static str {
    match category {
        Category::BuildFiles => "What building native mods leaves behind: made again when needed.",
        Category::PrebuiltGames => {
            "The game packages native mods are built against: downloaded again when needed."
        }
        Category::SpareGameBuilds => {
            "Game builds kept for running servers: downloaded again when needed."
        }
        Category::RustCompiler => {
            "The compiler that builds native mods: downloaded again the next time one is built."
        }
        Category::OtherCaches => "Other downloads kept to make the launcher faster.",
    }
}
