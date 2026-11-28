//! The CLI and Storage page share one fixed-path inventory.
mod disk_walk;
#[cfg(test)]
mod regressions;
mod report_clearing;
use super::*;
use crate::{backend, instances, native_mods};
use serde::Serialize;
use std::sync::Arc;

use disk_walk::*;

const BUILD_CACHES: [BuildCache; 10] = [
    BuildCache::Source,
    BuildCache::Tools,
    BuildCache::PrebuiltOutput,
    BuildCache::InternalsSync,
    BuildCache::Prebuilt,
    BuildCache::ServerBuilds,
    BuildCache::KeptMods,
    BuildCache::Rust,
    BuildCache::Cargo,
    BuildCache::Scratch,
];

#[derive(Serialize)]
pub struct Row {
    pub category: Category,
    pub bytes: u64,
    pub freed_bytes: u64,
    pub remaining_bytes: u64,
    #[serde(skip)]
    private_compiler_bytes: u64,
    /// Refused or busy units are retained, with their allocated bytes and a visible reason.
    pub kept: Vec<Kept>,
}

impl Row {
    pub fn clearable_bytes(&self) -> u64 {
        self.remaining_bytes
            .saturating_sub(self.private_compiler_bytes)
    }
}

#[derive(Serialize)]
pub struct Kept {
    pub bytes: u64,
    pub reason: String,
}

#[derive(Serialize)]
pub struct Report {
    pub rows: Vec<Row>,
    pub total_bytes: u64,
}

impl Report {
    fn include_retained_compiler(&mut self, bytes: Option<u64>) {
        if let Some(bytes) = bytes {
            let row = self
                .rows
                .iter_mut()
                .find(|r| r.category == Category::BuildFiles)
                .expect("report contains Build files");
            row.private_compiler_bytes = bytes;
            row.bytes = row.bytes.saturating_add(bytes);
            row.remaining_bytes = row.remaining_bytes.saturating_add(bytes);
            self.total_bytes = self.total_bytes.saturating_add(bytes);
            row.kept.push(Kept { bytes, reason: crate::i18n::tr("Private compiler work was kept because the launcher couldn't prove all workers had stopped.").into() });
        }
    }
}

impl Category {
    pub const ALL: [Self; 5] = [
        Self::BuildFiles,
        Self::PrebuiltGames,
        Self::SpareGameBuilds,
        Self::RustCompiler,
        Self::OtherCaches,
    ];
}
