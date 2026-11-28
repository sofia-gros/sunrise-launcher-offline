//! What the inventory finds on disk: the fixed roots, their entries, and the logs and jars it may clear.
use super::*;

pub(super) struct Root {
    path: PathBuf,
    pub(super) directory: Option<Arc<Dir>>,
}
impl Root {
    pub(super) fn open(root: &Path) -> Result<Self, Error> {
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

pub(super) struct Entry {
    pub(super) category: Category,
    pub(super) directory: Option<Arc<Dir>>,
    pub(super) relative: PathBuf,
    pub(super) path: PathBuf,
}

impl Entry {
    pub(super) fn fixed(root: &Root, relative: &Path, category: Category) -> Result<Self, Error> {
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
    pub(super) fn access(&self) -> Result<Access, Error> {
        match self.category {
            Category::RustCompiler => Ok(Access::BuildTools),
            // No game runs from kept mod libraries: a build copies them into its workspace.
            _ if self.relative.starts_with(BuildCache::KeptMods.relative()) => {
                Ok(Access::BuildTools)
            }
            _ => Access::for_unit(&self.path),
        }
    }

    pub(super) fn clear(&self) -> Result<(), Error> {
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

pub(super) fn usage_stamp(name: &Path) -> bool {
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
    pub(super) fn entries(&self) -> Result<Vec<Entry>, Error> {
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
        let data = Root::open(&self.data)?;
        let played: HashSet<_> = instances::list_in(&self.game, &self.data)
            .into_iter()
            .filter_map(|instance| instance.resolve_version(None).ok())
            .collect();
        for version in backend::installed_in(&self.data.join("versions")) {
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
        for instance in instances::list_in(&self.game, &self.data) {
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

pub(super) fn unused_jars(entries: &mut Vec<Entry>, home: &Root) -> Result<(), Error> {
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
