//! Disposable launcher storage. Player-owned files are never accepted as cleanup targets.
use crate::cache_cleanup::{Access, Plan};
pub use crate::native_cache::Error;
use crate::native_cache::{checked, known_link, linked, Cache};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod inventory;
pub use inventory::{Report, Row};

/// Stops a count running on a worker: whoever started it keeps a clone and sets it when the
/// count is no longer wanted. It is never reset, so each count gets its own.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Whether both are the same token, not merely both unset.
    pub fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Shared with the launcher's existing server storage display.
pub fn format_bytes(n: u64) -> String {
    match n {
        0..1_000_000 => format!("{} kB", n.div_ceil(1000)),
        1_000_000..1_000_000_000 => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{:.2} GB", n as f64 / 1e9),
    }
}

/// The rows on the Storage page.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    BuildFiles,
    PrebuiltGames,
    SpareGameBuilds,
    RustCompiler,
    ModBuildCache,
    OtherCaches,
}

/// Fixed build-home caches. Instance and version inventory adds its protected entries separately.
#[derive(Clone, Copy, Debug)]
pub enum BuildCache {
    Source,
    Tools,
    PrebuiltOutput,
    InternalsSync,
    Prebuilt,
    ServerBuilds,
    /// Mod libraries kept between native builds (`native_mods::kept`).
    KeptMods,
    ModBuildCache,
    Rust,
    Cargo,
    Scratch,
}

impl BuildCache {
    fn relative(self) -> &'static str {
        match self {
            Self::Source => "native/source",
            Self::Tools => "native/tools",
            Self::PrebuiltOutput => "native/prebuilt-build",
            Self::InternalsSync => "native/internals-sync",
            Self::Prebuilt => "native/prebuilt",
            Self::ServerBuilds => "native/server-builds",
            Self::KeptMods => "native/kept-mods",
            Self::ModBuildCache => "native/w",
            Self::Rust => "rust",
            Self::Cargo => "cargo",
            Self::Scratch => "tmp",
        }
    }

    fn access(self, path: &Path) -> Result<Access, Error> {
        match self {
            // No game runs from kept mod libraries: a build copies them into its workspace.
            Self::KeptMods | Self::ModBuildCache | Self::Rust | Self::Cargo | Self::Scratch => {
                Ok(Access::BuildTools)
            }
            _ => Access::for_unit(path),
        }
    }

    pub fn category(self) -> Category {
        match self {
            Self::ModBuildCache => Category::ModBuildCache,
            Self::Prebuilt => Category::PrebuiltGames,
            Self::ServerBuilds => Category::SpareGameBuilds,
            Self::Rust | Self::Cargo | Self::Scratch => Category::RustCompiler,
            _ => Category::BuildFiles,
        }
    }
}

/// Disposable storage inventory and manual clear. Read-only reports never hold the native build
/// lease; manual clears acquire it before touching build outputs.
pub struct Storage {
    root: PathBuf,
    directory: Dir,
}

impl Storage {
    pub fn open() -> Result<Self, Error> {
        Self::at(&crate::backend::build_dir())
    }

    fn at(root: &Path) -> Result<Self, Error> {
        checked(root, true)?;
        fs::create_dir_all(root)?;
        let directory = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
        let storage = Self {
            root: root.canonicalize()?,
            directory,
        };
        storage.validate_root()?;
        Ok(storage)
    }

    fn build_lease(&self) -> Result<Cache, Error> {
        self.validate_root()?;
        Cache::acquire(&self.root)
    }

    fn path(&self, cache: BuildCache) -> Result<PathBuf, Error> {
        let mut path = self.root.clone();
        // Validate every ancestor, not only the final directory (Windows junctions included).
        if path.canonicalize()? != path {
            return Err(Error::Unsafe(path));
        }
        for part in cache.relative().split('/') {
            path.push(part);
            checked(&path, true)?;
        }
        Ok(path)
    }

    pub fn bytes(&self, cache: BuildCache) -> Result<u64, Error> {
        if matches!(cache, BuildCache::ModBuildCache) {
            return Ok(self
                .report(None, false)?
                .rows
                .into_iter()
                .find(|row| row.category == cache.category())
                .expect("complete report")
                .bytes);
        }
        allocated_bytes(&self.path(cache)?)
    }

    /// Detach on the same volume before removal. Failed removals remain for the next manual clear.
    pub fn clear(&self, cache: BuildCache) -> Result<u64, Error> {
        if matches!(cache, BuildCache::ModBuildCache) {
            return Ok(self
                .report(Some(cache.category()), false)?
                .rows
                .into_iter()
                .find(|row| row.category == cache.category())
                .expect("complete report")
                .freed_bytes);
        }
        let _lease = self.build_lease()?;
        let path = self.path(cache)?;
        self.trash(&path, cache.access(&path)?)
    }

    pub fn clear_targets(&self) -> Result<u64, Error> {
        // Retain Cargo lock inodes; never rename an entire locked target directory.
        self.build_lease()?.clear()
    }

    fn validate_root(&self) -> Result<(), Error> {
        use cap_fs_ext::MetadataExt;
        checked(&self.root, true)?;
        let current =
            Dir::open_ambient_dir(&self.root, cap_std::ambient_authority())?.dir_metadata()?;
        let held = self.directory.dir_metadata()?;
        if (current.dev(), current.ino()) != (held.dev(), held.ino()) {
            return Err(Error::Unsafe(self.root.clone()));
        }
        Ok(())
    }

    fn trash(&self, path: &Path, access: Access) -> Result<u64, Error> {
        self.validate_root()?;
        let bytes = allocated_bytes(path)?;
        if !path.try_exists()? {
            return Ok(0);
        }
        if matches!(access, Access::LeasedGame) {
            crate::cache_cleanup::delete_game_unit(path)?;
            return Ok(bytes.saturating_sub(allocated_bytes(path)?));
        }
        access.check()?;
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| Error::Unsafe(path.to_owned()))?;
        let mut parent = self.directory.try_clone()?;
        let mut parts = relative.components().peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                Self::clear_entry(&self.directory, &parent, Path::new(part.as_os_str()))?;
            } else {
                parent = parent.open_dir_nofollow(part.as_os_str())?;
            }
        }
        Ok(bytes.saturating_sub(allocated_bytes(path)?))
    }

    fn clear_entry(trash: &Dir, parent: &Dir, name: &Path) -> Result<(), Error> {
        let meta = parent.symlink_metadata(name)?;
        let plan = if meta.is_dir() && !meta.file_type().is_symlink() {
            Plan::pin(&parent.open_dir_nofollow(name)?)?
        } else {
            Plan::pin_file(parent, name)?
        };
        plan.clear(trash)
    }
}

pub fn allocated_bytes(path: &Path) -> Result<u64, Error> {
    Ok(Tree::build(path)?.size(&mut HashSet::new()))
}

// How many folders `Tree::build` has walked on this thread, so tests can prove a cleanup
// pass walks each folder once.
#[cfg(test)]
thread_local! {
    static ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ACCESS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many units have had their executable use classified on this thread.
#[cfg(test)]
pub(crate) fn access_calls() -> usize {
    ACCESS.with(|calls| calls.get())
}
#[cfg(test)]
pub(crate) fn walks() -> usize {
    WALKS.with(|walks| walks.get())
}

#[cfg(test)]
pub(crate) fn entries_calls() -> usize {
    ENTRIES.with(|calls| calls.get())
}

/// One validated walk of a folder: every file and directory's allocated size and identity,
/// refusing any link or file the launcher does not expect. Sizes are read from it afterwards,
/// each with its own set of already-counted blocks, without touching the disk again.
pub(crate) struct Tree(Option<Node>);

struct Node {
    id: (u64, u64),
    bytes: u64,
    children: Vec<Node>,
}

impl Tree {
    pub(crate) fn build(path: &Path) -> Result<Self, Error> {
        Self::walk(path, None, false)
    }

    /// Stops descending once `cancel` is set. What it returns then is partial, so a caller that
    /// sees the token set afterwards must discard the tree.
    pub(crate) fn build_until(path: &Path, cancel: &Cancel) -> Result<Self, Error> {
        Self::walk(path, Some(cancel), false)
    }

    /// Mod work owns its link entries, not the directories or files they point to.
    fn mod_work(path: &Path, cancel: &Cancel) -> Result<Self, Error> {
        Self::walk(path, Some(cancel), true)
    }

    fn walk(path: &Path, cancel: Option<&Cancel>, skip_links: bool) -> Result<Self, Error> {
        #[cfg(test)]
        WALKS.with(|walks| walks.set(walks.get() + 1));
        Self::node(path, cancel, skip_links).map(Self)
    }

    fn node(path: &Path, cancel: Option<&Cancel>, skip_links: bool) -> Result<Option<Node>, Error> {
        if cancel.is_some_and(Cancel::is_cancelled) {
            return Ok(None);
        }
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if linked(&meta) {
            if skip_links {
                return Ok(None);
            }
            // A known link holds nothing of its own, and what it points at is not this cache's.
            return match fs::read_link(path) {
                Ok(target) if known_link(&target) => Ok(None),
                _ => Err(Error::Unsafe(path.to_owned())),
            };
        }
        if !meta.is_file() && !meta.is_dir() {
            return Err(Error::Unsafe(path.to_owned()));
        }
        let (id, bytes) = match allocation(path, &meta) {
            Ok(allocated) => allocated,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut children = Vec::new();
        if meta.is_dir() {
            let entries = match fs::read_dir(path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                children.extend(Self::node(&entry.path(), cancel, skip_links)?);
            }
        }
        Ok(Some(Node {
            id,
            bytes,
            children,
        }))
    }

    /// The allocated bytes not already in `seen`: a block reached twice (a hard link, or a file
    /// shared with an earlier folder using the same set) counts once, and a directory already
    /// counted is not entered again.
    pub(crate) fn size(&self, seen: &mut HashSet<(u64, u64)>) -> u64 {
        fn count(node: &Node, seen: &mut HashSet<(u64, u64)>) -> u64 {
            if !seen.insert(node.id) {
                return 0;
            }
            node.children.iter().fold(node.bytes, |total, child| {
                total.saturating_add(count(child, seen))
            })
        }
        self.0.as_ref().map_or(0, |node| count(node, seen))
    }
}

#[cfg(unix)]
fn allocation(_path: &Path, meta: &fs::Metadata) -> Result<((u64, u64), u64), Error> {
    use std::os::unix::fs::MetadataExt;
    Ok(((meta.dev(), meta.ino()), meta.blocks().saturating_mul(512)))
}

#[cfg(windows)]
fn allocation(path: &Path, _meta: &fs::Metadata) -> Result<((u64, u64), u64), Error> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_STANDARD_INFO,
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let mut standard: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    // Both structures are OS-owned output buffers; file keeps the handle live.
    unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), &mut info) == 0
            || GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    if info.dwFileAttributes & 0x400 != 0 {
        return Err(Error::Unsafe(path.to_owned()));
    }
    Ok((
        (
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        ),
        standard.AllocationSize as u64,
    ))
}

#[cfg(test)]
#[path = "storage/tests.rs"]
mod tests;
