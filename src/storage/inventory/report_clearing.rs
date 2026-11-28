//! The inventory's report, and clearing its rows.
use super::*;

impl Storage {
    /// Not deletion candidates: an unproved worker can still be writing to them.
    fn retained_compiler_bytes(
        &self,
        counted: &mut HashSet<(u64, u64)>,
    ) -> Result<Option<u64>, Error> {
        let native = self.root.join("native");
        let mut bytes = None;
        if checked(&native, true)?.is_some() {
            for entry in fs::read_dir(&native)? {
                let entry = entry?;
                if entry.file_name().to_str().is_some_and(|name| {
                    name.starts_with(native_mods::ATTEMPT_PREFIX)
                        || name.starts_with(native_mods::OLD_ATTEMPT_PREFIX)
                }) {
                    // A changing or unreadable tree is still protected; zero means its size is unknown.
                    let size = Tree::build(&entry.path()).map_or(0, |tree| tree.size(counted));
                    bytes = Some(bytes.unwrap_or(0u64).saturating_add(size));
                }
            }
        }
        Ok(bytes)
    }

    pub fn report(&self, clear: Option<Category>, all: bool) -> Result<Report, Error> {
        let registry_entries = match fs::read_dir(self.data.join("run-leases")) {
            Ok(mut entries) => entries.try_fold(0usize, |count, entry| entry.map(|_| count + 1))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        eprintln!(
            "{}",
            crate::i18n::trf(
                "Runtime lease registry entries: {count}",
                &[("count", &registry_entries.to_string())]
            )
        );
        let clearing = clear.is_some() || all;
        // Only manual clear takes the native build lease; read-only reports never make Play wait.
        let lease = if clearing {
            let lease = self.build_lease()?;
            // Also refuse an external Cargo holding a target lock before starting a cleanup.
            lease.bytes()?;
            use sunrise_launcher_smod::runtime_leases::{cleanup::Cleanup, Error as LeaseError};
            match Cleanup::existing(&self.data) {
                Ok(cleanup) => {
                    cleanup.collect_records()?;
                }
                Err(LeaseError::Busy) => {} // Other cache classes can still be reported/retained.
                Err(LeaseError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Some(lease)
        } else {
            None
        };
        let entries = self.entries()?;
        let mut report = self.apply_retaining(entries, clear, all, true, lease.as_ref())?;
        report.include_retained_compiler(self.retained_compiler_bytes(&mut HashSet::new())?);
        if !clearing {
            for entry in self.entries()? {
                if let Err(error) = entry.access().and_then(|access| access.check()) {
                    // A refused unit already has its line.
                    let bytes = match allocated_bytes(&entry.path) {
                        Err(Error::Unsafe(_)) => 0,
                        bytes => bytes?,
                    };
                    if bytes > 0 {
                        report
                            .rows
                            .iter_mut()
                            .find(|row| row.category == entry.category)
                            .expect("the rows have one for every category")
                            .kept
                            .push(Kept {
                                bytes,
                                reason: native_mods::cache_error(error),
                            });
                    }
                }
            }
        }
        Ok(report)
    }

    #[cfg(test)]
    pub(super) fn apply(
        &self,
        entries: Vec<Entry>,
        clear: Option<Category>,
        all: bool,
    ) -> Result<Report, Error> {
        let needs_target_lease = (clear.is_some() || all)
            && entries
                .iter()
                .any(|entry| entry.path == self.root.join("native/target"));
        let lease = needs_target_lease.then(|| self.build_lease()).transpose()?;
        self.apply_retaining(entries, clear, all, false, lease.as_ref())
    }

    #[cfg(test)]
    pub(crate) fn clear_observed(
        &self,
        clear: Option<Category>,
        all: bool,
        before_clear: impl FnMut(&Path),
    ) -> Result<Report, Error> {
        let lease = self.build_lease()?;
        let entries = self.entries()?;
        self.apply_retaining_observed(entries, clear, all, true, Some(&lease), before_clear)
    }

    pub(super) fn apply_retaining(
        &self,
        entries: Vec<Entry>,
        clear: Option<Category>,
        all: bool,
        retain: bool,
        lease: Option<&Cache>,
    ) -> Result<Report, Error> {
        self.apply_retaining_observed(entries, clear, all, retain, lease, |_| {})
    }

    fn apply_retaining_observed(
        &self,
        entries: Vec<Entry>,
        clear: Option<Category>,
        all: bool,
        retain: bool,
        lease: Option<&Cache>,
        mut before_clear: impl FnMut(&Path),
    ) -> Result<Report, Error> {
        // Validate every descendant before any deletion and deduplicate allocated blocks per row.
        let mut rows: Vec<_> = Category::ALL
            .into_iter()
            .map(|category| Row {
                category,
                bytes: 0,
                freed_bytes: 0,
                remaining_bytes: 0,
                private_compiler_bytes: 0,
                kept: Vec::new(),
            })
            .collect();
        // Each folder is walked once, here: the walk validates every descendant and its sizes
        // serve the rows and the kept lines.
        let (entries, mut trees) = if retain {
            refused(entries, &mut rows)?
        } else {
            let trees = entries
                .iter()
                .map(|entry| Tree::build(&entry.path))
                .collect::<Result<Vec<_>, Error>>()?;
            (entries, trees)
        };
        for row in &mut rows {
            row.bytes = allocated(&entries, &trees, row.category);
            row.remaining_bytes = row.bytes;
        }
        for (index, entry) in entries.iter().enumerate() {
            if all || clear == Some(entry.category) {
                before_clear(&entry.path);
                let result = if entry.path == self.root.join("native/target") {
                    lease
                        .expect("manual clear holds the build lease")
                        .clear()
                        .map(|_| ())
                } else {
                    entry.clear()
                };
                match result {
                    // Only a cleared folder is walked again, to see what is left of it.
                    Ok(()) => trees[index] = Tree::build(&entry.path)?,
                    Err(
                        error @ (Error::Busy
                        | Error::UnprovenExclusion
                        | Error::IncompleteUnit
                        | Error::UnpublishedGame
                        | Error::ResourceDependent(_)
                        | Error::ReferencedProfile
                        | Error::UnprovenReferences),
                    ) if retain => {
                        let row = rows
                            .iter_mut()
                            .find(|row| row.category == entry.category)
                            .expect("the rows have one for every category");
                        row.kept.push(Kept {
                            bytes: trees[index].size(&mut HashSet::new()),
                            reason: native_mods::cache_error(error),
                        });
                        // A busy build may be using subsequent build-tool units too. Stop
                        // this clear attempt rather than deleting more after contention.
                        if matches!(entry.access()?, Access::BuildTools) {
                            break;
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        for row in &mut rows {
            if all || clear == Some(row.category) {
                let remaining = allocated(&entries, &trees, row.category);
                row.freed_bytes = row.bytes.saturating_sub(remaining);
                row.remaining_bytes = remaining;
            }
        }
        Ok(Report {
            total_bytes: rows
                .iter()
                .fold(0u64, |total, row| total.saturating_add(row.bytes)),
            rows,
        })
    }
}

/// A row's bytes: its folders' walks, each allocated block counted once across the row.
pub(super) fn allocated(entries: &[Entry], trees: &[Tree], category: Category) -> u64 {
    let mut seen = HashSet::new();
    entries
        .iter()
        .zip(trees)
        .filter(|(entry, _)| entry.category == category)
        .fold(0u64, |bytes, (_, tree)| {
            bytes.saturating_add(tree.size(&mut seen))
        })
}

/// A unit holding a link or file the launcher does not expect is refused whole: it is neither
/// counted nor cleared, and its row says so. The other units are not held up by it.
fn refused(entries: Vec<Entry>, rows: &mut [Row]) -> Result<(Vec<Entry>, Vec<Tree>), Error> {
    let mut sound = Vec::new();
    let mut trees = Vec::new();
    for entry in entries {
        match Tree::build(&entry.path) {
            Ok(tree) => {
                sound.push(entry);
                trees.push(tree);
            }
            Err(error @ Error::Unsafe(_)) => rows
                .iter_mut()
                .find(|row| row.category == entry.category)
                .expect("the rows have one for every category")
                .kept
                .push(Kept {
                    bytes: 0,
                    reason: native_mods::cache_error(error),
                }),
            Err(error) => return Err(error),
        }
    }
    Ok((sound, trees))
}
