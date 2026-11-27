//! Disposable native compilation outputs only. Installed games and player data are never targets.
//! Keep Cargo's profile directories and lock files in place: unlinking a locked inode would let
//! another Cargo acquire a replacement lock while cleanup was still deleting its outputs.
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const PROFILES: [&str; 2] = ["fast", "release"];

/// The folder under `native/` holding one persistent compiler work folder per game build. One
/// letter, because Windows' path limit counts it (see `WORK_SLOT_HEX` in the native workspace).
pub(crate) const WORK: &str = "w";

#[derive(Debug)]
pub enum Error {
    Busy,
    UnprovenExclusion,
    IncompleteUnit,
    UnpublishedGame,
    ReferencedProfile,
    ResourceDependent(String),
    UnprovenReferences,
    UnsupportedFilesystem,
    Unsafe(PathBuf),
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) struct Cache {
    root: PathBuf,
    directory: Dir,
    _lease: Locked,
}

pub(crate) use sunrise_launcher_smod::locks::{LockMode, Locked, Release};

pub(crate) use sunrise_launcher_smod::runtime_leases::linked;
pub(crate) fn checked(path: &Path, directory: bool) -> Result<Option<Metadata>, Error> {
    sunrise_launcher_smod::runtime_leases::checked(path, directory).map_err(Into::into)
}
impl From<sunrise_launcher_smod::runtime_leases::Error> for Error {
    fn from(error: sunrise_launcher_smod::runtime_leases::Error) -> Self {
        use sunrise_launcher_smod::runtime_leases::Error as Shared;
        match error {
            Shared::Busy => Self::Busy,
            Shared::UnsupportedFilesystem => Self::UnsupportedFilesystem,
            Shared::Unsafe(path) => Self::Unsafe(path),
            Shared::Io(error) => Self::Io(error),
        }
    }
}

/// Whether `target` is where a link the game's prepare step leaves in a cache points: into an
/// installed build under the launcher's own `versions/`. Such a link is counted as nothing and
/// removed as the link itself. Its target is never opened, so the test is on the written path
/// alone, and every other link is refused as before.
pub(crate) fn known_link(target: &Path) -> bool {
    if !target.is_absolute()
        || target
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return false;
    }
    let versions = crate::backend::data_dir().join("versions");
    target.starts_with(&versions)
        || versions.canonicalize().is_ok_and(|canonical| {
            target.starts_with(&canonical) || target.starts_with(crate::backend::plain(canonical))
        })
}

fn lock(path: &Path) -> Result<Locked, Error> {
    checked(path, false)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    locked(file, LockMode::Exclusive)
}

pub(crate) fn locked(file: File, mode: LockMode) -> Result<Locked, Error> {
    sunrise_launcher_smod::locks::locked(file, mode).map_err(|error| match error {
        fs::TryLockError::WouldBlock => Error::Busy,
        fs::TryLockError::Error(error) => Error::Io(error),
    })
}

/// A persistent build folder as [`Cache::work_slot`] left it.
pub(crate) struct Provisioned {
    pub(crate) path: PathBuf,
    /// Nothing is in it: the build that runs there compiles everything.
    pub(crate) cold: bool,
}

/// `name` as one folder name, never a path.
fn single(name: &str) -> Result<&Path, Error> {
    let path = Path::new(name);
    let mut parts = path.components();
    match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(path),
        _ => Err(Error::Unsafe(path.to_owned())),
    }
}

/// Removes `path` itself without opening what a link there points to: a symlink or a junction
/// goes as the link, and a folder with links inside goes with each of them unlinked.
fn remove(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if linked(&meta) {
        // A symlink to a file goes by `remove_file`; on Windows a junction or folder symlink
        // only by `remove_dir`.
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    } else if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// The folders of a work slot the SDK's tool keeps and reuses, checked for links one level at a
/// time before any of them is handed to a compiler: the slot itself, `prebuilt-build` and
/// `prebuilt-package`, each build folder in `prebuilt-build`, and in it `target` and `stage-*`.
fn sdk_owned(slot: &Path) -> Result<(), Error> {
    if checked(slot, true)?.is_none() {
        return Ok(());
    }
    checked(&slot.join("prebuilt-package"), true)?;
    let builds = slot.join("prebuilt-build");
    if checked(&builds, true)?.is_none() {
        return Ok(());
    }
    for build in fs::read_dir(&builds)? {
        let build = build?.path();
        if checked(&build, true)?.is_none() {
            continue;
        }
        for entry in fs::read_dir(&build)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "target" || name.to_string_lossy().starts_with("stage-") {
                checked(&entry.path(), true)?;
            }
        }
    }
    Ok(())
}

fn size(path: &Path) -> Result<u64, Error> {
    let meta = fs::symlink_metadata(path)?;
    if linked(&meta) {
        return Err(Error::Unsafe(path.to_owned()));
    }
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        return Err(Error::Unsafe(path.to_owned()));
    }
    let mut bytes = 0u64;
    for entry in fs::read_dir(path)? {
        bytes = bytes.saturating_add(size(&entry?.path())?);
    }
    Ok(bytes)
}

impl Cache {
    /// One cross-process lease covers staging, Cargo, installation/export and manual clear.
    /// OS locks release on exit, including a crash; a leftover lock file is not a busy marker.
    pub(crate) fn acquire(build_dir: &Path) -> Result<Self, Error> {
        checked(build_dir, true)?;
        fs::create_dir_all(build_dir)?;
        let root = build_dir.canonicalize()?.join("native");
        checked(&root, true)?;
        fs::create_dir_all(&root)?;
        let anchor = root.ancestors().last().expect("canonical root is absolute");
        let mut directory = Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?;
        for component in root.strip_prefix(anchor).expect("root anchor").components() {
            directory = directory.open_dir_nofollow(component.as_os_str())?;
        }
        let lease = lock(&root.join(".build.lock"))?;
        Ok(Self {
            root,
            directory,
            _lease: lease,
        })
    }

    pub(crate) fn bytes(&self) -> Result<u64, Error> {
        self.process(None, false)
    }

    /// `path` as a link-free folder of this cache, made when it is absent. Anything else there (a
    /// link, a junction, a file) is removed as itself, never followed, and replaced by an empty
    /// folder. The lease must be held; the parent of `path` is a folder this cache already owns.
    fn own(&self, path: &Path) -> Result<(), Error> {
        match checked(path, true) {
            Ok(Some(_)) => (),
            Ok(None) => fs::create_dir(path)?,
            Err(Error::Unsafe(_)) => {
                remove(path)?;
                fs::create_dir(path)?;
            }
            Err(error) => return Err(error),
        }
        self.unchanged_path(path)
    }

    /// Where [`Self::work_slot`] puts `slot`, before anything is made there.
    pub(crate) fn work_slot_path(&self, slot: &str) -> PathBuf {
        crate::backend::plain(self.root.join(WORK).join(slot))
    }

    /// The folder the compiler's own build folders persist in for the game build `slot` names,
    /// `native/w/<slot>`, checked link-free below the lease. A slot holding a link where the
    /// SDK keeps its own folders is removed whole (never followed) and made again empty: it
    /// builds cold once, with no repair of what was there.
    pub(crate) fn work_slot(&self, slot: &str) -> Result<Provisioned, Error> {
        let work = self.root.join(WORK);
        let path = work.join(single(slot)?);
        self.own(&work)?;
        match sdk_owned(&path) {
            Ok(()) => (),
            Err(Error::Unsafe(_)) => remove(&path)?,
            Err(error) => return Err(error),
        }
        self.own(&path)?;
        let cold = fs::read_dir(&path)?.next().is_none();
        Ok(Provisioned {
            path: crate::backend::plain(path),
            cold,
        })
    }

    /// [`Self::work_slot`] after removing what the slot held: the one cold build a slot that
    /// failed gets.
    pub(crate) fn fresh_work_slot(&self, slot: &str) -> Result<Provisioned, Error> {
        let work = self.root.join(WORK);
        let leaf = single(slot)?;
        // A completed compiler could have changed this parent since the slot was attached.
        // Check it from the held native directory before inspecting the slot by path.
        let _parent = self.directory.open_dir_nofollow(WORK)?;
        self.unchanged_path(&work)?;
        remove(&work.join(leaf))?;
        self.work_slot(slot)
    }

    /// Cargo's home, `<build folder>/cargo`, the same folder Storage counts as build files,
    /// checked link-free below the lease. A link there is removed as itself and the folder made
    /// again.
    pub(crate) fn cargo_home(&self) -> Result<PathBuf, Error> {
        let build = self
            .root
            .parent()
            .expect("the native folder is inside the build folder");
        let path = build.join("cargo");
        self.own(&path)?;
        Ok(crate::backend::plain(path))
    }
    pub(crate) fn clear(&self) -> Result<u64, Error> {
        crate::cache_cleanup::Access::IncompleteGame.check()?;
        self.process(None, true)
    }
    #[cfg(test)]
    pub(crate) fn trim(&self, limit: u64) -> Result<u64, Error> {
        if limit == 0 {
            return Ok(0);
        }
        if self.bytes()? <= limit {
            return Ok(0);
        }
        crate::cache_cleanup::Access::IncompleteGame.check()?;
        self.process(Some(limit), true)
    }

    // Detect a managed directory redirected through an ancestor link after taking the lease.
    fn unchanged_path(&self, path: &Path) -> Result<(), Error> {
        if path.canonicalize()? != path {
            return Err(Error::Unsafe(path.to_owned()));
        }
        Ok(())
    }

    fn process(&self, limit: Option<u64>, clear: bool) -> Result<u64, Error> {
        checked(&self.root, true)?;
        self.unchanged_path(&self.root)?;
        let target = self.root.join("target");
        if checked(&target, true)?.is_none() {
            return Ok(0);
        }
        self.unchanged_path(&target)?;
        let mut locks = Vec::new();
        let mut folders = Vec::new();
        for profile in PROFILES {
            let folder = target.join(profile);
            if checked(&folder, true)?.is_some() {
                // Also protects an older launcher's Cargo or one started outside our lease.
                self.unchanged_path(&folder)?;
                locks.push(lock(&folder.join(".cargo-lock"))?);
                folders.push(folder);
            }
        }
        // Validate ALL profiles and descendants before deleting ANY output. Never traverse links.
        let mut entries = Vec::new();
        let mut bytes = 0u64;
        for folder in folders {
            for entry in fs::read_dir(folder)? {
                let entry = entry?;
                if entry.file_name() == ".cargo-lock" {
                    continue;
                }
                bytes = bytes.saturating_add(size(&entry.path())?);
                entries.push(entry.path());
            }
        }
        if !clear {
            return Ok(bytes);
        }
        if limit.is_some_and(|limit| bytes <= limit) {
            return Ok(0);
        }
        let records = self.root.join("built.json");
        let has_records = checked(&records, false)?.is_some();
        for path in entries {
            self.unchanged_path(&path)?;
            // The launcher lease and all Cargo locks remain held through removal. These are
            // fixed children of the two validated cache profiles, never caller-supplied paths.
            if fs::symlink_metadata(&path)?.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        if has_records {
            fs::remove_file(records)?;
        }
        drop(locks);
        Ok(bytes)
    }
}

#[cfg(test)]
#[path = "native_cache/tests.rs"]
mod tests;
