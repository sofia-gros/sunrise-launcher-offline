//! The CLI and Storage page share one fixed-path inventory.
mod count;
#[cfg(test)]
#[path = "inventory_regressions.rs"]
mod regressions;
use super::*;
use crate::{backend, instances, native_mods};
use serde::Serialize;
use std::sync::Arc;

// Only files the game can regenerate from authoritative inputs may be offered here.
const PROFILE_GAME_CACHES: [&str; 5] = [
    ".sunrise-cache/behaviours-v7.bin",
    ".sunrise-cache/registry-v1.bin",
    ".sunrise-cache/mod-content",
    ".sunrise-cache/resourcepacks",
    "cache/dx12-shaders.bin",
];

const BUILD_CACHES: [BuildCache; 11] = [
    BuildCache::Source,
    BuildCache::Tools,
    BuildCache::PrebuiltOutput,
    BuildCache::InternalsSync,
    BuildCache::Prebuilt,
    BuildCache::ServerBuilds,
    BuildCache::KeptMods,
    BuildCache::ModBuildCache,
    BuildCache::Rust,
    BuildCache::Cargo,
    BuildCache::Scratch,
];

#[derive(Clone, Serialize)]
pub struct Row {
    pub category: Category,
    pub bytes: u64,
    pub freed_bytes: u64,
    pub remaining_bytes: u64,
    #[serde(skip)]
    pub after_clear: bool,
    #[serde(skip)]
    pub older_build_kept: bool,
    /// Refused or busy units are retained, with their allocated bytes and a visible reason.
    pub kept: Vec<Kept>,
}

impl Row {
    fn new(category: Category) -> Self {
        Self {
            category,
            bytes: 0,
            freed_bytes: 0,
            remaining_bytes: 0,
            after_clear: false,
            older_build_kept: false,
            kept: Vec::new(),
        }
    }

    pub fn clearable_bytes(&self) -> u64 {
        self.remaining_bytes
    }

    fn keep(&mut self, bytes: u64, error: Error, log: bool) {
        if log {
            // Log each refusal without printing paths, build names, or OS error text.
            let reason = match &error {
                Error::Unsafe(_) => "unexpected file or link".into(),
                Error::ResourceDependent(_) => "another build needs these resources".into(),
                Error::Io(error) => {
                    format!("{:?}, OS error {:?}", error.kind(), error.raw_os_error())
                }
                _ => format!("{error:?}"),
            };
            crate::launcher_log::line(format!(
                "Storage Clear kept {:?} unit ({bytes} bytes): {reason}",
                self.category
            ));
        }
        self.kept.push(Kept {
            bytes,
            reason: native_mods::cache_error(error),
        });
    }
}

#[derive(Clone, Serialize)]
pub struct Kept {
    pub bytes: u64,
    pub reason: String,
}

/// The rows counted or cleared so far: all of them once it [`is_complete`](Self::is_complete).
#[derive(Clone, Serialize)]
pub struct Report {
    pub rows: Vec<Row>,
    pub total_bytes: u64,
    /// The count or Clear this report came from, so a window can drop one it has cancelled.
    #[serde(skip)]
    origin: Option<Cancel>,
}

impl Report {
    fn new(rows: Vec<Row>) -> Self {
        Self {
            total_bytes: rows
                .iter()
                .fold(0u64, |total, row| total.saturating_add(row.bytes)),
            rows,
            origin: None,
        }
    }

    /// Whether every row is in: a count in progress hands over the rows it has finished.
    pub fn is_complete(&self) -> bool {
        self.rows.len() == Category::ALL.len()
    }

    pub fn tagged(mut self, origin: &Cancel) -> Self {
        self.origin = Some(origin.clone());
        self
    }

    pub fn is_from(&self, origin: &Cancel) -> bool {
        self.origin.as_ref().is_some_and(|own| own.is(origin))
    }

    fn push(&mut self, row: Row) {
        self.total_bytes = self.total_bytes.saturating_add(row.bytes);
        self.rows.push(row);
    }
}

impl Category {
    pub const ALL: [Self; 6] = [
        Self::BuildFiles,
        Self::PrebuiltGames,
        Self::SpareGameBuilds,
        Self::RustCompiler,
        Self::ModBuildCache,
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

fn compiler_attempt(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.starts_with(native_mods::ATTEMPT_PREFIX)
            || name.starts_with(native_mods::OLD_ATTEMPT_PREFIX)
    })
}

fn is_compiler_attempt(relative: &Path) -> bool {
    relative.parent() == Some(Path::new("native"))
        && relative.file_name().is_some_and(compiler_attempt)
}

struct Entry {
    category: Category,
    directory: Option<Arc<Dir>>,
    relative: PathBuf,
    path: PathBuf,
    profile: Option<Arc<(PathBuf, String)>>,
}

impl Entry {
    fn fixed(root: &Root, relative: &Path, category: Category) -> Result<Self, Error> {
        let mut path = root.path.clone();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let std::path::Component::Normal(component) = component else {
                return Err(Error::Unsafe(relative.to_owned()));
            };
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(meta)
                    if linked(&meta)
                        && !(components.peek().is_none()
                            && (category == Category::ModBuildCache
                                || is_compiler_attempt(relative))) =>
                {
                    return Err(Error::Unsafe(path));
                }
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
            profile: None,
        })
    }

    fn tree(&self, cancel: &Cancel) -> Result<Tree, Error> {
        if self.category == Category::ModBuildCache || is_compiler_attempt(&self.relative) {
            Tree::mod_work(&self.path, cancel)
        } else {
            Tree::build_until(&self.path, cancel)
        }
    }

    /// The lease integration changes this classification, not deletion code.
    fn access(&self) -> Result<Access, Error> {
        #[cfg(test)]
        ACCESS.with(|calls| calls.set(calls.get() + 1));
        if self.build_tool() {
            Ok(Access::BuildTools)
        } else {
            Access::for_unit(&self.path)
        }
    }

    fn build_tool(&self) -> bool {
        matches!(
            self.category,
            Category::RustCompiler | Category::ModBuildCache
        ) || is_compiler_attempt(&self.relative)
            || self.relative.starts_with(BuildCache::KeptMods.relative())
    }

    #[cfg(test)]
    fn clear(&self) -> Result<(), Error> {
        self.remove(false)
    }

    fn remove(&self, explicit: bool) -> Result<(), Error> {
        let owns_links =
            self.category == Category::ModBuildCache || is_compiler_attempt(&self.relative);
        if !owns_links && !self.path.try_exists()? {
            return Ok(());
        }
        let Some(root) = &self.directory else {
            return Ok(());
        };
        let _profile = if let Some(profile) = &self.profile {
            let (home, version) = profile.as_ref();
            let owner = instances::lifecycle::owner_file(&backend::game_home(), home)
                .map_err(|_| Error::Busy)?;
            let claim = crate::profile_txn::Admission::claim(home, owner.as_deref())
                .map_err(|_| Error::Busy)?;
            if backend::in_use(version) {
                return Err(Error::Busy);
            }
            Some(claim)
        } else {
            None
        };
        if !explicit {
            let access = self.access()?;
            if matches!(access, Access::LeasedGame) {
                return crate::cache_cleanup::delete_game_unit(&self.path);
            }
            access.check()?;
        } else if !self.build_tool() && !self.relative.starts_with("native/target") {
            let data = present(backend::data_dir().canonicalize())?;
            if data.as_ref().is_some_and(|data| {
                self.path.starts_with(data) || self.path.starts_with(backend::plain(data.clone()))
            }) {
                return crate::cache_cleanup::clear_claimed_unit(&self.path);
            }
        }
        let mut parent = root.try_clone()?;
        for part in self.relative.parent().unwrap_or(Path::new("")).components() {
            parent = parent.open_dir_nofollow(part.as_os_str())?;
        }
        let name = Path::new(self.relative.file_name().expect("entry has a name"));
        if owns_links {
            let Some(meta) = present(parent.symlink_metadata(name))? else {
                return Ok(());
            };
            // On Windows, a junction is a directory entry; unlink it without opening it.
            #[cfg(windows)]
            if linked(&meta) {
                use std::os::windows::fs::MetadataExt;
                use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
                if meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    return present(parent.remove_dir(name)).map(|_| ());
                }
            }
            if linked(&meta) || meta.is_file() {
                return present(parent.remove_file(name)).map(|_| ());
            }
            // Keep the no-follow parent as the boundary throughout recursive removal.
            return present(parent.remove_dir_all(name)).map(|_| ());
        }
        Storage::clear_entry(root, &parent, name)
    }
}

/// A folder's entries for a report that holds no build lease, so a folder or entry can vanish
/// while it is read: that one is skipped, as the size walk skips one. Every other failure, and
/// every link check on what is found, still stands.
fn children(folder: &Path) -> Result<Vec<fs::DirEntry>, Error> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut found = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => found.push(entry),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(test)]
    LISTED.with(|hook| {
        if let Some(hook) = &*hook.borrow() {
            hook(folder);
        }
    });
    Ok(found)
}

// Lets a test change the disk between a folder's listing and what is done with its entries.
#[cfg(test)]
type ListedHook = Box<dyn Fn(&Path)>;
#[cfg(test)]
thread_local! {
    static LISTED: std::cell::RefCell<Option<ListedHook>> =
        const { std::cell::RefCell::new(None) };
}

/// `Ok(None)` for a result that failed only because its entry vanished.
fn present<T>(result: std::io::Result<T>) -> Result<Option<T>, Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Says how many runtime lease registry entries there are, on stderr, before a report starts.
fn note_lease_registry() -> Result<(), Error> {
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
    Ok(())
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
    /// What a report or Clear works on, found once: the fixed build-home units, the spare game
    /// builds and each profile's caches, and detached trash beside any of them.
    fn entries(&self) -> Result<Vec<Entry>, Error> {
        #[cfg(test)]
        ENTRIES.with(|calls| calls.set(calls.get() + 1));
        let mut entries = Vec::new();
        self.build_entries(&mut entries, &Root::open(&self.root)?)?;
        let data = Root::open(&backend::data_dir())?;
        // One read-only snapshot of the profiles says both which builds they play and where
        // their folders are. An unreadable one proves neither, so it offers no spare build and
        // no profile cache.
        let profiles = instances::list_for_storage().ok();
        if let Some(profiles) = &profiles {
            spare_builds(&mut entries, &data, profiles)?;
        }
        // Interrupted removal deliberately loses completion. Keep the remaining unit
        // visible for residual reporting and a safe retry, even though installed() omits it.
        for child in children(&data.path.join("versions"))? {
            if present(child.file_type())?.is_some_and(|kind| kind.is_dir())
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
        let game = backend::game_home();
        let overlaps_profile = |path: &Path| {
            backend::plain(path.to_owned()) == backend::plain(game.clone())
                || profiles.as_ref().is_some_and(|profiles| {
                    profiles.iter().any(|profile| {
                        backend::plain(path.to_owned()) == backend::plain(profile.home.clone())
                    })
                })
        };
        if !overlaps_profile(&data.path) {
            for relative in ["cache/github", "mod-browser", "profile-heads"] {
                add(&mut entries, &data, relative, Category::OtherCaches)?;
            }
        }
        if let Some(profiles) = &profiles {
            profile_caches(&mut entries, profiles)?;
        }
        self.detached_trash(&mut entries, &data, profiles.as_deref().unwrap_or(&[]))?;
        Ok(entries)
    }

    fn build_entries(&self, entries: &mut Vec<Entry>, build_root: &Root) -> Result<(), Error> {
        let native = Entry::fixed(build_root, Path::new("native"), Category::BuildFiles)?;
        for cache in BUILD_CACHES {
            if matches!(
                cache,
                BuildCache::Rust
                    | BuildCache::Source
                    | BuildCache::Tools
                    | BuildCache::PrebuiltOutput
                    | BuildCache::Prebuilt
                    | BuildCache::InternalsSync
                    | BuildCache::ModBuildCache
            ) {
                let parent = self.root.join(cache.relative());
                if checked(&parent, true)?.is_some() {
                    for child in children(&parent)? {
                        let name = child.file_name();
                        if usage_stamp(Path::new(&name))
                            || (matches!(cache, BuildCache::ModBuildCache)
                                && crate::cache_cleanup::reserved(Path::new(&name)))
                        {
                            continue;
                        }
                        entries.push(Entry::fixed(
                            build_root,
                            &Path::new(cache.relative()).join(name),
                            cache.category(),
                        )?);
                    }
                }
            } else {
                add(entries, build_root, cache.relative(), cache.category())?;
            }
        }
        let build_files = Category::BuildFiles;
        for child in children(&native.path)? {
            if compiler_attempt(&child.file_name())
                && present(child.file_type())?
                    .is_some_and(|kind| kind.is_dir() || kind.is_symlink())
            {
                entries.push(Entry::fixed(
                    build_root,
                    &Path::new("native").join(child.file_name()),
                    build_files,
                )?);
            }
        }
        for name in [
            "built.json",
            "internals.json",
            "build.log",
            "catalog.partial",
        ] {
            add(entries, build_root, &format!("native/{name}"), build_files)?;
        }
        // The target lock files must remain in place. Their contents are inventoried here but
        // deletion goes through Cache, which also acquires Cargo's locks.
        let target = self.root.join("native/target");
        if checked(&target, true)?.is_some() {
            for child in children(&target)? {
                if crate::cache_cleanup::reserved(Path::new(&child.file_name()))
                    || usage_stamp(Path::new(&child.file_name()))
                {
                    continue;
                }
                let relative = Path::new("native/target").join(child.file_name());
                let Some(kind) = present(child.file_type())? else {
                    continue;
                };
                checked(&child.path(), kind.is_dir())?;
                if kind.is_dir() && child.file_name().to_string_lossy().contains('-') {
                    // Cross-compilation target triples own independent profile units too.
                    for profile in children(&child.path())? {
                        if crate::cache_cleanup::reserved(Path::new(&profile.file_name()))
                            || usage_stamp(Path::new(&profile.file_name()))
                        {
                            continue;
                        }
                        entries.push(Entry::fixed(
                            build_root,
                            &relative.join(profile.file_name()),
                            build_files,
                        )?);
                    }
                } else {
                    entries.push(Entry::fixed(build_root, &relative, build_files)?);
                }
            }
        }
        Ok(())
    }

    /// Detached trees can remain beside any managed cache after an interrupted removal.
    fn detached_trash(
        &self,
        entries: &mut Vec<Entry>,
        data: &Root,
        profiles: &[instances::Instance],
    ) -> Result<(), Error> {
        let parents: HashSet<_> = entries
            .iter()
            .filter(|entry| {
                entry.profile.is_none()
                    && !PROFILE_GAME_CACHES
                        .iter()
                        .any(|relative| entry.relative == Path::new(relative))
            })
            .filter_map(|entry| entry.path.parent().map(Path::to_owned))
            .chain([
                self.root.clone(),
                data.path.clone(),
                data.path.join("versions"),
            ])
            .collect();
        let work = self.root.join(BuildCache::ModBuildCache.relative());
        for parent in parents {
            // Detached slots inside `native/w` are already inventoried as Mod build cache.
            let plain = backend::plain(parent.clone());
            if parent == work
                || plain == backend::plain(backend::game_home())
                || profiles
                    .iter()
                    .any(|profile| plain == backend::plain(profile.home.clone()))
            {
                continue;
            }
            let root = Root::open(&parent)?;
            if root.directory.is_none() {
                continue;
            }
            for entry in children(&parent)? {
                let name = entry.file_name();
                let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(".trash-"))
                else {
                    continue;
                };
                let mut numbers = suffix.split('-');
                if numbers.next().is_some_and(|pid| pid.parse::<u32>().is_ok())
                    && numbers
                        .next()
                        .is_some_and(|stamp| stamp.parse::<u128>().is_ok())
                    && numbers
                        .next()
                        .is_none_or(|counter| counter.parse::<u64>().is_ok())
                    && numbers.next().is_none()
                {
                    entries.push(Entry::fixed(
                        &root,
                        Path::new(&entry.file_name()),
                        Category::OtherCaches,
                    )?);
                }
            }
        }
        Ok(())
    }

    /// A read-only count, or a manual Clear of one row or all of them under the build lease.
    pub fn report(&self, clear: Option<Category>, all: bool) -> Result<Report, Error> {
        if clear.is_none() && !all {
            return self.report_counting(|_| {});
        }
        note_lease_registry()?;
        // Only manual clear takes the native build lease; read-only reports never make Play wait.
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
        let entries = self.entries()?;
        self.apply_retaining(entries, clear, all, true, Some(&lease))
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
        let mut rows: Vec<_> = Category::ALL.into_iter().map(Row::new).collect();
        // Each folder is walked once, here: the walk validates every descendant and its sizes
        // serve the rows and the kept lines.
        let (entries, mut trees) = if retain {
            refused(entries, &mut rows, &Cancel::default(), |category| {
                all || clear == Some(category)
            })?
        } else {
            let trees = entries
                .iter()
                .map(|entry| entry.tree(&Cancel::default()))
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
                    entry.remove(retain)
                };
                match result {
                    // Only a cleared folder is walked again, to see what is left of it.
                    Ok(()) => trees[index] = entry.tree(&Cancel::default())?,
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
                        row.older_build_kept |= matches!(error, Error::UnprovenExclusion);
                        row.keep(trees[index].size(&mut HashSet::new()), error, true);
                        // A busy build may be using subsequent build-tool units too. Stop
                        // this clear attempt rather than deleting more after contention.
                        if entry.build_tool() {
                            break;
                        }
                    }
                    Err(error @ Error::Io(_))
                        if retain
                            && (is_compiler_attempt(&entry.relative)
                                || entry.category == Category::ModBuildCache) =>
                    {
                        // A failed recursive removal can already have freed some files. If
                        // the rest can't be measured, conservatively keep its original size.
                        if let Ok(tree) = entry.tree(&Cancel::default()) {
                            trees[index] = tree;
                        }
                        let row = rows
                            .iter_mut()
                            .find(|row| row.category == entry.category)
                            .expect("all categories have a row");
                        row.keep(trees[index].size(&mut HashSet::new()), error, true);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        for row in &mut rows {
            if all || clear == Some(row.category) {
                row.after_clear = true;
                let remaining = allocated(&entries, &trees, row.category);
                row.freed_bytes = row.bytes.saturating_sub(remaining);
                row.remaining_bytes = remaining;
            }
        }
        Ok(Report::new(rows))
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
fn refused(
    entries: Vec<Entry>,
    rows: &mut [Row],
    cancel: &Cancel,
    clearing: impl Fn(Category) -> bool,
) -> Result<(Vec<Entry>, Vec<Tree>), Error> {
    let mut sound = Vec::new();
    let mut trees = Vec::new();
    for entry in entries {
        match entry.tree(cancel) {
            Ok(tree) => {
                sound.push(entry);
                trees.push(tree);
            }
            Err(error @ Error::Unsafe(_)) => rows
                .iter_mut()
                .find(|row| row.category == entry.category)
                .unwrap()
                .keep(0, error, clearing(entry.category)),
            Err(Error::Io(error))
                if clearing(entry.category)
                    && (error.kind() == std::io::ErrorKind::PermissionDenied
                        || cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))) =>
            {
                rows.iter_mut()
                    .find(|row| row.category == entry.category)
                    .unwrap()
                    .keep(0, Error::Io(error), true);
            }
            Err(error) => return Err(error),
        }
    }
    Ok((sound, trees))
}

/// Native builds no profile plays and nothing is running or building from.
fn spare_builds(
    entries: &mut Vec<Entry>,
    data: &Root,
    profiles: &[instances::Instance],
) -> Result<(), Error> {
    let played: HashSet<_> = profiles
        .iter()
        .filter_map(|instance| instance.resolve_version(None).ok())
        .collect();
    for version in backend::installed() {
        if native_mods::is_native(&version)
            && !played.contains(&version)
            && native_mods::resource_dependent(&version).is_none()
            && !backend::in_use(&version)
        {
            add(
                entries,
                data,
                &format!("versions/{version}"),
                Category::SpareGameBuilds,
            )?;
        }
    }
    Ok(())
}

/// The caches, old logs and unused jars of every profile whose game is not running.
fn profile_caches(entries: &mut Vec<Entry>, profiles: &[instances::Instance]) -> Result<(), Error> {
    for instance in profiles {
        // A missing version cannot prove that the instance is idle. Keep its caches.
        let Ok(version) = instance.resolve_version(None) else {
            continue;
        };
        if backend::in_use(&version) {
            continue;
        }
        let root = Root::open(&instance.home)?;
        let start = entries.len();
        old_logs(entries, &root)?;
        unused_jars(entries, &root)?;
        profile_game_caches(entries, &root)?;
        let profile = Arc::new((instance.home.clone(), version));
        for entry in &mut entries[start..] {
            entry.profile = Some(profile.clone());
        }
    }
    Ok(())
}

fn profile_game_caches(entries: &mut Vec<Entry>, home: &Root) -> Result<(), Error> {
    for relative in PROFILE_GAME_CACHES {
        add(entries, home, relative, Category::OtherCaches)?;
    }
    Ok(())
}

fn old_logs(entries: &mut Vec<Entry>, home: &Root) -> Result<(), Error> {
    for directory in ["logs", "crash-reports"] {
        let folder = Entry::fixed(home, Path::new(directory), Category::OtherCaches)?.path;
        if checked(&folder, true)?.is_none() {
            continue;
        }
        let mut files = Vec::new();
        for entry in children(&folder)? {
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
    for entry in children(&folder)? {
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
    fn profile_game_cache_inventory_counts_only_the_rebuildable_entries() {
        let home = crate::storage::tests::Home::new();
        let files = [
            home.seed(".sunrise-cache/behaviours-v7.bin"),
            home.seed(".sunrise-cache/registry-v1.bin"),
            home.seed(".sunrise-cache/mod-content/out"),
            home.seed(".sunrise-cache/resourcepacks/out"),
            home.seed("cache/dx12-shaders.bin"),
        ];
        let canaries = [
            home.seed(".sunrise-cache/player-data"),
            home.seed("cache/unknown/keep"),
            home.seed("cache/.trash-unrelated/keep"),
        ];
        let root = Root::open(&home.0).unwrap();
        let mut entries = Vec::new();
        profile_game_caches(&mut entries, &root).unwrap();
        // Simulate a profile separate from both the build root and launcher data directory.
        let launcher = crate::storage::tests::Home::new();
        let storage = Storage::at(&launcher.0).unwrap();
        let data = Root::open(&launcher.0).unwrap();
        storage.detached_trash(&mut entries, &data, &[]).unwrap();
        assert_eq!(entries.len(), 5);
        for file in files {
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| file.starts_with(&entry.path))
                    .count(),
                1
            );
        }
        for file in canaries {
            assert!(!entries.iter().any(|entry| file.starts_with(&entry.path)));
        }
    }

    #[test]
    fn refused_units_are_residual_with_bytes_and_reason() {
        let home = crate::storage::tests::Home::new();
        let file = home.seed("game/output");
        let storage = Storage::at(&home.0).unwrap();
        let root = Root::open(&home.0).unwrap();
        let entries = vec![Entry::fixed(&root, Path::new("game"), Category::RustCompiler).unwrap()];
        let lock = home.seed("game/.cargo-lock");
        let lock = fs::File::options()
            .read(true)
            .write(true)
            .open(lock)
            .unwrap();
        lock.lock().unwrap();
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
            .find(|row| row.category == Category::RustCompiler)
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

    #[cfg(unix)]
    #[test]
    fn retained_compiler_attempt_with_outside_link_is_cleared_without_following_it() {
        let home = crate::storage::tests::Home::new();
        let payload = home.seed("native/.nb-retained/output");
        let outside = home.seed("player-data/keep");
        let link = payload.with_file_name("link");
        std::os::unix::fs::symlink(outside.parent().unwrap(), &link).unwrap();
        let storage = Storage::at(&home.0).unwrap();
        let root = Root::open(&home.0).unwrap();
        let entries = vec![Entry::fixed(
            &root,
            Path::new("native/.nb-retained"),
            Category::BuildFiles,
        )
        .unwrap()];
        let report = storage
            .apply_retaining(entries, Some(Category::BuildFiles), false, true, None)
            .unwrap();
        let row = report
            .rows
            .iter()
            .find(|row| row.category == Category::BuildFiles)
            .unwrap();
        assert!(row.freed_bytes > 0);
        assert!(row.kept.is_empty());
        assert!(!payload.parent().unwrap().exists());
        assert!(outside.is_file());
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
        assert!(
            report
                .rows
                .iter()
                .find(|row| row.category == Category::RustCompiler)
                .unwrap()
                .freed_bytes
                > 0
        );
        #[cfg(unix)]
        assert_eq!(
            report
                .rows
                .iter()
                .find(|row| row.category == Category::OtherCaches)
                .unwrap()
                .kept
                .len(),
            1
        );
    }
}
