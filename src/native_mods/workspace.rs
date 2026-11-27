//! SDK inputs and writable paths for one native compiler attempt, never reused by a retry.
use super::{sdk_cache::Package, tool, Path};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Component,
};
use sunrise_launcher_smod::{
    index::{library_identity::LibraryIdentity, sha256, sha256_file},
    manifest::Manifest,
};

/// An attempt's folder: this prefix and 6 random characters. Short, because Windows' path limit
/// counts every folder above the deepest file the SDK's build writes (#396).
pub(crate) const ATTEMPT_PREFIX: &str = ".nb-";
/// The folder launchers before 1.3.1 named attempts with, still found and counted on disk.
pub(crate) const OLD_ATTEMPT_PREFIX: &str = ".native-attempt-";
/// The SDK's work folder inside an attempt.
const WORK: &str = "w";
/// The deepest file a Windows build writes under the work folder, as #396 found it: rustc's
/// import definition for a raw-dylib DLL, in Cargo's target folder under the SDK's build key.
const DEEPEST: &str = r"prebuilt-build\94256510f74e-f618d2e4182eb19e\target\x86_64-pc-windows-gnu\release\deps\rustcgsxNAs\api-ms-win-core-backgroundtask-l1-1-0.dll_imports_indirect.def";
/// Windows' `MAX_PATH`, its terminating NUL included, which mingw's `dlltool` doesn't get around.
const MAX_PATH: usize = 260;

/// How long the deepest path of a build in `attempt` would be, when it would pass Windows' limit.
pub(super) fn too_deep(attempt: &Path) -> Option<usize> {
    let length = attempt.as_os_str().len() + 1 + WORK.len() + 1 + DEEPEST.len();
    (length >= MAX_PATH).then_some(length)
}

pub(super) struct Workspace(tempfile::TempDir);

/// One SDK library, with the exact basename it is installed under and the verified
/// bytes. A registry symlink alias is copied as a regular file under its original name.
#[derive(Debug)]
pub(super) struct Library {
    pub(super) name: String,
    pub(super) source: std::path::PathBuf,
    pub(super) sha256: String,
}

impl Library {
    pub(super) fn copy_into(&self, destination: &Path) -> io::Result<()> {
        let output = destination.join(&self.name);
        fs::copy(&self.source, &output)?;
        if sha256_file(&output).is_ok_and(|digest| digest == self.sha256) {
            Ok(())
        } else {
            Err(invalid())
        }
    }
}

impl Workspace {
    pub(super) fn new(parent: &Path) -> io::Result<Self> {
        let parent = crate::backend::plain(parent.canonicalize()?);
        tempfile::Builder::new()
            .prefix(ATTEMPT_PREFIX)
            .rand_bytes(6)
            .tempdir_in(parent)
            .map(Self)
    }

    pub(super) fn path(&self) -> &Path {
        self.0.path()
    }

    /// The folder the SDK builds in.
    pub(super) fn work(&self) -> std::path::PathBuf {
        self.path().join(WORK)
    }

    pub(super) fn isolate(&self, command: &mut std::process::Command) -> io::Result<()> {
        let path = self.path();
        for dir in ["cargo", "target", "tmp"] {
            fs::create_dir_all(path.join(dir))?;
        }
        let tmp = path.join("tmp");
        command
            .env("CARGO_HOME", path.join("cargo"))
            .env("CARGO_TARGET_DIR", path.join("target"))
            .env("TMPDIR", &tmp)
            .env("TMP", &tmp)
            .env("TEMP", &tmp);
        Ok(())
    }

    /// From here on an escaped descendant can outlive the launcher or SDK. It can keep its
    /// private paths; ordinary retry and recovery must not unlink or reuse them.
    pub(super) fn allow_worker(&mut self) {
        self.0.disable_cleanup(true);
    }

    /// A terminal OS proof, not successful compilation by itself, permits cleanup.
    pub(super) fn worker_finished(&mut self) {
        self.0.disable_cleanup(false);
    }

    /// Copy the installed SDK inputs, never giving a compiler links into the shared cache.
    pub(super) fn package(&self, package: &Package) -> io::Result<Package> {
        let root = self.path().join("package");
        fs::create_dir(&root)?;
        let private = Package::at(&root);
        for (from, to) in [
            (&package.runtime, &private.runtime),
            (&package.api, &private.api),
            (&package.engine, &private.engine),
        ] {
            match fs::symlink_metadata(from) {
                Ok(meta) if meta.is_dir() && !crate::native_cache::linked(&meta) => {
                    copy_slice(from, to)?;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
                Ok(_) => return Err(invalid()),
            }
        }
        Ok(private)
    }

    pub(super) fn import_log(&self) -> io::Result<String> {
        let work = self.work();
        let log = work.join("build.log");
        match regular_under(self.path(), &log) {
            Ok(()) => Ok(tool::tail(fs::File::open(log)?)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(err),
        }
    }

    /// Bind the exact SDK path and bytes of every published library, including ordinary unstamped
    /// dependencies. Only the registry route can add Cargo output or Cargo-home libraries.
    pub(super) fn verify_outputs<'a>(
        &self,
        built: &tool::Built,
        package: &Package,
        rust: &crate::rust_toolchain::Toolchain,
        expected_route: Option<tool::CheckRoute>,
        revision: &str,
        mods: impl Iterator<Item = &'a Manifest>,
    ) -> io::Result<Vec<Library>> {
        let work = self.work();
        if expected_route.is_some_and(|route| route != built.route) {
            return Err(invalid());
        }
        // Both SDK runtime slices use this filename; installation renames the server image.
        let binary = crate::backend::plain(built.binary.clone());
        let runtime_binary = binary == package.runtime.join("bin").join(crate::backend::CLIENT)
            && regular_under(self.path(), &binary).is_ok();
        match built.route {
            tool::CheckRoute::PrebuiltApi if !runtime_binary => return Err(invalid()),
            tool::CheckRoute::Prebuilt
                if !runtime_binary
                    && (!binary.starts_with(&work)
                        || regular_under(self.path(), &binary).is_err()) =>
            {
                return Err(invalid())
            }
            _ => (),
        }
        let wanted: BTreeMap<_, _> = mods.map(|m| (m.id.as_str(), m.version.as_str())).collect();
        let used: BTreeSet<_> = built.mods.iter().map(String::as_str).collect();
        if used.len() != built.mods.len()
            || !used.iter().copied().eq(wanted.keys().copied())
            || built.libraries.is_empty()
        {
            return Err(invalid());
        }
        let std_lib = rust.target_libdir();
        let mingw = rust.mingw_bin();
        // Registry compilation returns the merged package's executable and places the
        // stamped patch in <target>/<triple>/release. The latter binds the one allowed
        // Cargo target root without recalculating the SDK's private build key.
        let whole = work.join("prebuilt-package");
        let registry_target = if built.route == tool::CheckRoute::Prebuilt
            && binary == whole.join("bin").join(crate::backend::CLIENT)
        {
            let patch = built
                .records
                .iter()
                .find(|row| matches!(row.kind, tool::LibraryKind::Patch))
                .ok_or_else(invalid)?;
            let patch = built
                .libraries
                .iter()
                .find(|file| {
                    file.file_name()
                        .is_some_and(|name| name == patch.file.as_str())
                })
                .ok_or_else(invalid)?;
            let patch = crate::backend::plain(patch.clone());
            let builds = work.join("prebuilt-build");
            let release = patch
                .strip_prefix(&builds)
                .ok()
                .and_then(Path::parent)
                .ok_or_else(invalid)?;
            let folders: Vec<_> = release.components().collect();
            match folders.as_slice() {
                [Component::Normal(key), Component::Normal(target), Component::Normal(triple), Component::Normal(release)]
                    if *target == "target"
                        && *triple == rust.host.target
                        && *release == "release" =>
                {
                    Some(builds.join(key).join("target"))
                }
                _ => return Err(invalid()),
            }
        } else {
            None
        };
        let cargo_home = self.path().join("cargo");
        let joined_runtime = whole.join("deps");
        let stamped_names: BTreeSet<_> =
            built.records.iter().map(|row| row.file.as_str()).collect();
        let mut files = BTreeMap::new();
        for supplied in &built.libraries {
            // SDK directory scans can return canonical-prefixed Windows paths. The plan
            // otherwise uses the launcher's plain form; this does not resolve any links.
            let file = crate::backend::plain(supplied.clone());
            let name = file
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(crate::browser::safe_name)
                .ok_or_else(invalid)?;
            let dynamic = [".so", ".dll", ".dylib"]
                .iter()
                .any(|suffix| name.ends_with(suffix));
            let versioned_so = name
                .rsplit_once(".so.")
                .is_some_and(|(base, version)| !base.is_empty() && !version.is_empty());
            let stamped = stamped_names.contains(name);
            let dependency = !stamped && registry_target.is_some();
            if !dynamic && !(dependency && versioned_so) {
                return Err(invalid());
            }
            let runtime =
                || file.starts_with(&package.runtime) && regular_under(self.path(), &file).is_ok();
            let standard = || {
                (name.starts_with("libstd-") || name.starts_with("std-"))
                    && std_lib.as_ref().is_some_and(|dir| {
                        file == dir.join(name) && regular_under(dir, &file).is_ok()
                    })
            };
            let gnu = || {
                matches!(name, "libgcc_s_seh-1.dll" | "libwinpthread-1.dll")
                    && file == mingw.join(name)
                    && regular_under(&mingw, &file).is_ok()
            };
            let produced = (stamped && file.starts_with(&work))
                || (!stamped
                    && built.route == tool::CheckRoute::Prebuilt
                    && file.starts_with(&joined_runtime));
            let source = if dynamic
                && ((produced && regular_under(self.path(), &file).is_ok())
                    || runtime()
                    || standard()
                    || gnu())
            {
                file.clone()
            } else if let Some(target) = registry_target.as_ref().filter(|_| dependency) {
                registry_library(self.path(), [target, &cargo_home], &file)?
            } else {
                return Err(invalid());
            };
            let sha256 = sha256_file(&source).map_err(io::Error::other)?;
            let library = Library {
                name: name.to_owned(),
                source,
                sha256,
            };
            if files.insert(name.to_ascii_lowercase(), library).is_some() {
                return Err(invalid());
            }
        }
        let mut stamped = BTreeSet::new();
        let mut recorded = BTreeSet::new();
        let mut patch = false;
        // Each library row's identity, by mod id, and the native mods each one says it links.
        let mut identities = BTreeMap::new();
        let mut linked = Vec::new();
        for row in &built.records {
            let Some(file) = files
                .get(&row.file.to_ascii_lowercase())
                .filter(|file| file.name == row.file)
            else {
                return Err(invalid());
            };
            let id = row.mod_id.as_str();
            let version = match row.kind {
                tool::LibraryKind::Mod if recorded.insert(id) => {
                    wanted.get(id).copied().ok_or_else(invalid)?
                }
                tool::LibraryKind::Patch
                    if built.route == tool::CheckRoute::Prebuilt
                        && id == "sunrise_patch"
                        && !patch =>
                {
                    patch = true;
                    "0"
                }
                _ => return Err(invalid()),
            };
            let rust = crate::rust_toolchain::VERSION;
            // Since sunrise#5136 the identity also names the mod's files and the identity of each
            // native mod the library links; SDKs from before it stamp only the five fields.
            let identity_matches = match LibraryIdentity::parse(&row.identity) {
                Some(identity) => {
                    linked.push(identity.dependencies().ok_or_else(invalid)?);
                    identity.build == revision
                        && identity.id == id
                        && identity.version == version
                        && identity.rust == rust
                        && identity.panic == "unwind"
                }
                None => row.identity == format!("{revision} {id} {version} {rust} unwind"),
            };
            if !stamped.insert(&row.file) || !identity_matches || file.sha256 != row.sha256 {
                return Err(invalid());
            }
            if matches!(row.kind, tool::LibraryKind::Mod) {
                identities.insert(id, row.identity.as_str());
            }
        }
        if recorded != used {
            return Err(invalid());
        }
        // A library links only mods of this build, each as the identity this build recorded for it.
        for (id, digest) in linked.into_iter().flatten() {
            if identities
                .get(id)
                .is_none_or(|identity| sha256(identity.as_bytes()) != digest)
            {
                return Err(invalid());
            }
        }
        Ok(files.into_values().collect())
    }
}

fn copy_slice(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if crate::native_cache::linked(&meta) {
            return Err(invalid());
        }
        let to = to.join(entry.file_name());
        if meta.is_dir() {
            copy_slice(&entry.path(), &to)?;
        } else if meta.is_file() {
            crate::preparer::clone_or_copy(&entry.path(), &to)?;
            // The tool names its build folder from each slice file's path, length and modified
            // time (`kept_build`). A copy stamped now (Linux's copy and clone) would name another
            // folder every attempt, and no kept library would ever be found there.
            times_handle(&to)?.set_modified(meta.modified()?)?;
        } else {
            return Err(invalid());
        }
    }
    Ok(())
}

/// A handle that may set `file`'s times even when the file is read-only: on Windows it asks
/// for attribute writes only, elsewhere setting times needs ownership, not write permission.
fn times_handle(file: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES
        fs::File::options().access_mode(0x100).open(file)
    }
    #[cfg(not(windows))]
    {
        fs::File::open(file)
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid native SDK output")
}

// The SDK preserves linked library basenames (e.g. libx.so -> libx.so.1). Only a
// final file symlink within the registry's exact roots is allowed. Keep the resolved
// checked source, not the symlink, and verify the published copy's digest again.
fn registry_library(
    workspace: &Path,
    roots: [&Path; 2],
    path: &Path,
) -> io::Result<std::path::PathBuf> {
    let allowed = |file: &Path| roots.iter().any(|root| file.starts_with(root));
    if !allowed(path) {
        return Err(invalid());
    }
    checked_under(workspace, path.parent().ok_or_else(invalid)?, true)?;
    let source = if fs::symlink_metadata(path)?.file_type().is_symlink() {
        crate::backend::plain(path.canonicalize()?)
    } else {
        path.to_owned()
    };
    if !allowed(&source) {
        return Err(invalid());
    }
    regular_under(workspace, &source)?;
    Ok(source)
}

// Refuse reparse points and links at every component, including the root. The compiler never
// receives the installation destination; the launcher copies these bytes out of the work tree.
pub(super) fn regular_under(root: &Path, file: &Path) -> io::Result<()> {
    checked_under(root, file, false)
}

fn checked_under(root: &Path, file: &Path, directory: bool) -> io::Result<()> {
    let tail = file.strip_prefix(root).map_err(|_| invalid())?;
    let mut path = root.to_path_buf();
    let mut parts = tail.components().peekable();
    if parts.peek().is_none() {
        return Err(invalid());
    }
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir() || crate::native_cache::linked(&meta) {
        return Err(invalid());
    }
    while let Some(std::path::Component::Normal(part)) = parts.next() {
        path.push(part);
        let meta = fs::symlink_metadata(&path)?;
        if crate::native_cache::linked(&meta) {
            return Err(invalid());
        }
        if parts.peek().is_none() {
            return if (directory && meta.is_dir()) || (!directory && meta.is_file()) {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        if !meta.is_dir() {
            return Err(invalid());
        }
    }
    Err(invalid())
}

#[cfg(test)]
mod tests;
