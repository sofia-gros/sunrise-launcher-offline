//! Cached storage inventory; filesystem work never runs on the render thread.
use super::*;

use storage::Category;

impl Launcher {
    pub(super) fn refresh_storage(&mut self, clear: Option<Category>, all: bool) {
        if self.storage_pending {
            return;
        }
        self.storage_pending = true;
        self.storage_clearing = clear.is_some() || all;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = storage::Storage::open()
                .and_then(|storage| storage.report(clear, all))
                .map(Box::new)
                .map_err(native_mods::cache_error);
            let _ = tx.send(Msg::Storage(result));
        });
    }

    /// A finished measurement or Clear from the worker.
    pub(super) fn storage_arrived(&mut self, result: Result<Box<storage::Report>, String>) {
        self.storage_pending = false;
        match result {
            Ok(report) => {
                self.storage_report = Some(*report);
                self.storage_error = None;
            }
            Err(error) => self.storage_error = Some(error),
        }
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
        Category::BuildFiles => {
            crate::i18n::tr_key("What building native mods leaves behind: made again when needed.")
        }
        Category::PrebuiltGames => crate::i18n::tr_key(
            "The game packages native mods are built against: downloaded again when needed.",
        ),
        Category::SpareGameBuilds => crate::i18n::tr_key(
            "Game builds kept for running servers: downloaded again when needed.",
        ),
        Category::RustCompiler => crate::i18n::tr_key(
            "The compiler that builds native mods: downloaded again the next time one is built.",
        ),
        Category::OtherCaches => {
            crate::i18n::tr_key("Other downloads kept to make the launcher faster.")
        }
    }
}
