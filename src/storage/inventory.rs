//! The CLI and Storage page share one fixed-path inventory.
#[cfg(test)]
#[path = "inventory_regressions.rs"]
mod regressions;
use super::*;
use crate::{backend, instances, native_mods};
use serde::Serialize;
use std::sync::Arc;

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

struct Root {
    path: PathBuf,
    directory: Option<Arc<Dir>>,
}
impl Root {
    fn open(root: &Path) -> Result<Self, Error> {
        let directory = match Dir::open_ambient_dir(root, cap_std::ambient_authority()) {
            Ok(directory) => Some(Arc::new(directory)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        checked(root, true)?;
        if root.exists() {
            // Windows canonical paths gain a `\\?\` prefix and long names, so compare ancestors' reparse points instead.
            #[cfg(windows)]
            for ancestor in root.ancestors().filter(|path| path.parent().is_some()) {
                checked(ancestor, true)?;
            }
            #[cfg(not(windows))]
            if root.canonicalize()? != root {
                return Err(Error::Unsafe(root.to_owned()));
            }
        }
        Ok(Self {
            path: root.to_owned(),
            directory,
        })
    }
}

struct Entry {
    category: Category,
    directory: Option<Arc<Dir>>,
    relative: PathBuf,
    path: PathBuf,
}

impl Entry {
    fn fixed(root: &Root, relative: &Path, category: Category) -> Result<Self, Error> {
        let mut path = root.path.clone();
        for component in relative.components() {
            let std::path::Component::Normal(component) = component else {
                return Err(Error::Unsafe(relative.to_owned()));
            };
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(meta) if linked(&meta) => return Err(Error::Unsafe(path)),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self {
            category,
            directory: root.directory.clone(),
            relative: relative.to_owned(),
            path,
        })
    }

    /// The lease integration changes this classification, not deletion code.
    fn access(&self) -> Result<Access, Error> {
        match self.category {
            Category::RustCompiler => Ok(Access::BuildTools),
            // No game runs from kept mod libraries: a build copies them into its workspace.
            _ if self.relative.starts_with(BuildCache::KeptMods.relative()) => {
                Ok(Access::BuildTools)
            }
            _ => Access::for_unit(&self.path),
        }
    }

    fn clear(&self) -> Result<(), Error> {
        if !self.path.try_exists()? {
            return Ok(());
        }
        let Some(root) = &self.directory else {
            return Ok(());
        };
        let access = self.access()?;
        if matches!(access, Access::LeasedGame) {
            return crate::cache_cleanup::delete_game_unit(&self.path);
        }
        access.check()?;
        let mut parent = root.try_clone()?;
        for part in self.relative.parent().unwrap_or(Path::new("")).components() {
            parent = parent.open_dir_nofollow(part.as_os_str())?;
        }
        Storage::clear_entry(root, &parent, Path::new(self.relative.file_name().unwrap()))
    }
}

fn usage_stamp(name: &Path) -> bool {
    let name = name.as_os_str().to_string_lossy();
    name == ".last-used" || (name.starts_with('.') && name.ends_with(".last-used"))
}

fn add(
    entries: &mut Vec<Entry>,
    root: &Root,
    relative: &str,
    category: Category,
) -> Result<(), Error> {
    entries.push(Entry::fixed(root, Path::new(relative), category)?);
    Ok(())
}

impl Storage {
    fn entries(&self) -> Result<Vec<Entry>, Error> {
        #[cfg(test)]
        ENTRIES.with(|calls| calls.set(calls.get() + 1));
        let mut entries = Vec::new();
        let build_root = Root::open(&self.root)?;
        for cache in BUILD_CACHES {
            if matches!(
                cache,
                BuildCache::Rust
                    | BuildCache::Source
                    | BuildCache::Tools
                    | BuildCache::PrebuiltOutput
                    | BuildCache::Prebuilt
                    | BuildCache::InternalsSync
            ) {
                let parent = self.root.join(cache.relative());
                if checked(&parent, true)?.is_some() {
                    for child in fs::read_dir(parent)? {
                        let child = child?;
                        if usage_stamp(Path::new(&child.file_name())) {
                            continue;
                        }
                        entries.push(Entry::fixed(
                            &build_root,
                            &Path::new(cache.relative()).join(child.file_name()),
                            cache.category(),
                        )?);
                    }
                }
            } else {
                add(
                    &mut entries,
                    &build_root,
                    cache.relative(),
                    cache.category(),
                )?;
            }
        }
        let build_files = Category::BuildFiles;
        for name in [
            "built.json",
            "internals.json",
            "build.log",
            "catalog.partial",
        ] {
            add(
                &mut entries,
                &build_root,
                &format!("native/{name}"),
                build_files,
            )?;
        }
        // The target lock files must remain in place. Their contents are inventoried here but
        // deletion goes through Cache, which also acquires Cargo's locks.
        let target = self.root.join("native/target");
        if checked(&target, true)?.is_some() {
            for child in fs::read_dir(&target)? {
                let child = child?;
                if crate::cache_cleanup::reserved(Path::new(&child.file_name()))
                    || usage_stamp(Path::new(&child.file_name()))
                {
                    continue;
                }
                let relative = Path::new("native/target").join(child.file_name());
                checked(&child.path(), child.file_type()?.is_dir())?;
                if child.file_type()?.is_dir() && child.file_name().to_string_lossy().contains('-')
                {
                    // Cross-compilation target triples own independent profile units too.
                    for profile in fs::read_dir(child.path())? {
                        let profile = profile?;
                        if crate::cache_cleanup::reserved(Path::new(&profile.file_name()))
                            || usage_stamp(Path::new(&profile.file_name()))
                        {
                            continue;
                        }
                        entries.push(Entry::fixed(
                            &build_root,
                            &relative.join(profile.file_name()),
                            build_files,
                        )?);
                    }
                } else {
                    entries.push(Entry::fixed(&build_root, &relative, build_files)?);
                }
            }
        }
        let data = Root::open(&backend::data_dir())?;
        let played: HashSet<_> = instances::list()
            .into_iter()
            .filter_map(|instance| instance.resolve_version(None).ok())
            .collect();
        for version in backend::installed() {
            if native_mods::is_native(&version)
                && !played.contains(&version)
                && native_mods::resource_dependent(&version).is_none()
                && !backend::in_use(&version)
            {
                add(
                    &mut entries,
                    &data,
                    &format!("versions/{version}"),
                    Category::SpareGameBuilds,
                )?;
            }
        }
        // Interrupted removal deliberately loses completion. Keep the remaining unit
        // visible for residual reporting and a safe retry, even though installed() omits it.
        match fs::read_dir(data.path.join("versions")) {
            Ok(children) => {
                for child in children {
                    let child = child?;
                    if child.file_type()?.is_dir()
                        && !child.path().join(".installed").is_file()
                        && !child.file_name().to_string_lossy().starts_with(".trash-")
                    {
                        entries.push(Entry::fixed(
                            &data,
                            &Path::new("versions").join(child.file_name()),
                            Category::SpareGameBuilds,
                        )?);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        for relative in ["cache/github", "mod-browser", "profile-heads"] {
            add(&mut entries, &data, relative, Category::OtherCaches)?;
        }
        for instance in instances::list() {
            // A missing version cannot prove that the instance is idle. Keep its caches.
            let Ok(version) = instance.resolve_version(None) else {
                continue;
            };
            if backend::in_use(&version) {
                continue;
            }
            let root = Root::open(&instance.home)?;
            old_logs(&mut entries, &root)?;
            unused_jars(&mut entries, &root)?;
            for relative in [".sunrise-cache", "cache"] {
                add(&mut entries, &root, relative, Category::OtherCaches)?;
            }
        }
        // Detached trees can remain beside any managed cache after an interrupted removal.
        let parents: HashSet<_> = entries
            .iter()
            .filter_map(|entry| entry.path.parent().map(Path::to_owned))
            .chain([
                self.root.clone(),
                data.path.clone(),
                data.path.join("versions"),
            ])
            .collect();
        for parent in parents {
            let root = Root::open(&parent)?;
            if root.directory.is_none() {
                continue;
            }
            for entry in fs::read_dir(&parent)? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with(".trash-") {
                    entries.push(Entry::fixed(
                        &root,
                        Path::new(&entry.file_name()),
                        Category::OtherCaches,
                    )?);
                }
            }
        }
        Ok(entries)
    }

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
        let registry_entries = match fs::read_dir(backend::data_dir().join("run-leases")) {
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
            match Cleanup::existing(&backend::data_dir()) {
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
                            .unwrap()
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
    fn apply(
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

    fn apply_retaining(
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
                            .unwrap();
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
fn allocated(entries: &[Entry], trees: &[Tree], category: Category) -> u64 {
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
                .unwrap()
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

fn old_logs(entries: &mut Vec<Entry>, home: &Root) -> Result<(), Error> {
    for directory in ["logs", "crash-reports"] {
        let folder = Entry::fixed(home, Path::new(directory), Category::OtherCaches)?.path;
        if checked(&folder, true)?.is_none() {
            continue;
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&folder)? {
            let entry = entry?;
            if crate::cache_cleanup::reserved(Path::new(&entry.file_name()))
                || usage_stamp(Path::new(&entry.file_name()))
            {
                continue;
            }
            let Some(meta) = checked(&entry.path(), false)? else {
                continue;
            };
            files.push((meta.modified()?, entry.file_name()));
        }
        files.sort();
        files.pop(); // Always retain the latest report, even when it is older than fourteen days.
        for (modified, name) in files {
            if modified
                .elapsed()
                .is_ok_and(|age| age.as_secs() > 14 * 86400)
            {
                entries.push(Entry::fixed(
                    home,
                    &Path::new(directory).join(name),
                    Category::OtherCaches,
                )?);
            }
        }
    }
    Ok(())
}

fn unused_jars(entries: &mut Vec<Entry>, home: &Root) -> Result<(), Error> {
    use sunrise_launcher_smod::{
        mod_list::{DIR, NATIVE_DIR},
        Package,
    };
    let folder = Entry::fixed(home, Path::new("jars"), Category::OtherCaches)?.path;
    if checked(&folder, true)?.is_none() {
        return Ok(());
    }
    let mut referenced = HashSet::new();
    for directory in [DIR, NATIVE_DIR] {
        let path = Entry::fixed(home, Path::new(directory), Category::OtherCaches)?.path;
        for package in sunrise_launcher_smod::find(&path) {
            // A malformed package cannot prove a jar is unused. Retain all jars in that case.
            let Ok(package) = Package::read(&package) else {
                return Ok(());
            };
            referenced.extend(
                package
                    .manifest
                    .jar
                    .iter()
                    .map(|jar| jar.cache_path(&folder)),
            );
        }
    }
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        if !crate::cache_cleanup::reserved(Path::new(&entry.file_name()))
            && !usage_stamp(Path::new(&entry.file_name()))
            && !referenced.contains(&entry.path())
        {
            entries.push(Entry::fixed(
                home,
                &Path::new("jars").join(entry.file_name()),
                Category::OtherCaches,
            )?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod manual_tests {
    use super::*;

    #[test]
    fn refused_units_are_residual_with_bytes_and_reason() {
        let home = crate::storage::tests::Home::new();
        let file = home.seed("game/output");
        let storage = Storage::at(&home.0).unwrap();
        let root = Root::open(&home.0).unwrap();
        let entries = vec![Entry::fixed(&root, Path::new("game"), Category::BuildFiles).unwrap()];
        let directory = file.parent().unwrap();
        // Give inline-directory filesystems enough names to allocate directory blocks too.
        for index in 0..32 {
            fs::write(
                directory.join(format!("padding-{index:02}-{}", "x".repeat(180))),
                [],
            )
            .unwrap();
        }
        // Independent filesystem allocations, not the recursive production sum.
        let own_bytes = |path: &Path| {
            super::super::allocation(path, &fs::symlink_metadata(path).unwrap())
                .unwrap()
                .1
        };
        let bytes = own_bytes(directory)
            + fs::read_dir(directory)
                .unwrap()
                .map(|entry| own_bytes(&entry.unwrap().path()))
                .sum::<u64>();
        let report = storage
            .apply_retaining(entries, None, true, true, None)
            .unwrap();
        let row = report
            .rows
            .iter()
            .find(|row| row.category == Category::BuildFiles)
            .unwrap();
        assert_eq!(row.bytes, bytes);
        assert_eq!(row.freed_bytes, 0);
        assert_eq!(row.remaining_bytes, bytes);
        assert_eq!(row.kept.len(), 1);
        assert_eq!(row.kept[0].bytes, bytes);
        assert!(!row.kept[0].reason.is_empty());
        assert!(file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_unit_with_an_unexpected_link_is_refused_on_its_row_and_holds_up_no_other() {
        let home = crate::storage::tests::Home::new();
        let good = home.seed("rust/old/payload");
        let bad = home.seed("cache/payload");
        home.seed("worlds/keep");
        std::os::unix::fs::symlink(home.0.join("worlds"), home.0.join("cache/link")).unwrap();
        let storage = Storage::at(&home.0).unwrap();
        let root = Root::open(&home.0).unwrap();
        let entries = vec![
            Entry::fixed(&root, Path::new("cache"), Category::OtherCaches).unwrap(),
            Entry::fixed(&root, Path::new("rust/old"), Category::RustCompiler).unwrap(),
        ];
        let report = storage
            .apply_retaining(entries, None, true, true, None)
            .unwrap();
        let row = |category| {
            report
                .rows
                .iter()
                .find(|row| row.category == category)
                .unwrap()
        };
        let refused = row(Category::OtherCaches);
        assert_eq!((refused.bytes, refused.freed_bytes), (0, 0));
        assert_eq!(refused.kept.len(), 1);
        assert!(refused.kept[0].reason.contains("link"));
        assert!(bad.exists());
        assert!(home.0.join("worlds/keep").exists());
        assert!(row(Category::RustCompiler).freed_bytes > 0);
        assert!(row(Category::RustCompiler).kept.is_empty());
        assert!(!good.exists());
    }

    #[test]
    fn retained_compiler_bytes_cannot_be_cleared() {
        let home = crate::storage::tests::Home::new();
        let old_worker = home.seed("native/.nb-old/compiler/w");
        let older_launchers = home.seed("native/.native-attempt-older/compiler/work");
        let storage = Storage::at(&home.0).unwrap();
        let report = storage.report(None, false).unwrap();
        let protected = &report.rows[0];
        assert_eq!(protected.category, Category::BuildFiles);
        assert!(protected.remaining_bytes > 0);
        assert_eq!(protected.bytes, protected.remaining_bytes);
        assert_eq!(protected.clearable_bytes(), 0);
        assert_eq!(protected.kept[0].bytes, protected.remaining_bytes);
        let manual = storage.report(Some(Category::BuildFiles), false).unwrap();
        assert_eq!(manual.rows[0].clearable_bytes(), 0);
        assert!(old_worker.is_file());
        assert!(older_launchers.is_file());
    }

    #[test]
    fn a_refused_unit_is_walked_once_and_a_cleared_one_twice() {
        let home = crate::storage::tests::Home::new();
        home.seed("rust/old/payload");
        let storage = Storage::at(&home.0).unwrap();
        let root = Root::open(&home.0).unwrap();
        #[cfg(unix)]
        {
            home.seed("cache/payload");
            std::os::unix::fs::symlink(home.0.join("rust"), home.0.join("cache/link")).unwrap();
        }
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut entries =
            vec![Entry::fixed(&root, Path::new("rust/old"), Category::RustCompiler).unwrap()];
        #[cfg(unix)]
        entries.push(Entry::fixed(&root, Path::new("cache"), Category::OtherCaches).unwrap());
        let count = entries.len();
        let walks = crate::storage::walks();
        let report = storage
            .apply_retaining(entries, None, true, true, None)
            .unwrap();
        // The refused folder was walked once (and stopped there); the sound one once, then once
        // more after it was cleared.
        assert_eq!(crate::storage::walks() - walks, count + 1);
        assert!(report.rows[3].freed_bytes > 0);
        #[cfg(unix)]
        assert_eq!(report.rows[4].kept.len(), 1);
    }
}
