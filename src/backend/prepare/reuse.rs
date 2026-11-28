//! Reusing another build's prepared output instead of preparing again.
use super::{
    check_play_resources, check_prepared, complete_assets_lacking_staged, complete_reports,
    finish_preparation_cancellable, finish_preparation_with_missing, forget_prepared,
    has_runtime_reports, probe_missing_extras, refill_own_store, resources, resources_at,
    resources_compatible, resources_owner, resources_owner_in, LEGACY_OUTPUT_VERSIONS,
    RESOURCES_MANIFEST,
};
use crate::backend::{
    game_version_at, installed_in, prepare_lock, prepare_lock_at, version_dir, versions_dir,
};
use crate::managed_play::Resources;
use std::fs;
use std::path::{Path, PathBuf};

/// The files under `dir/relative`, as paths relative to `dir`.
pub(in crate::backend) fn files_under(dir: &Path, relative: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir.join(relative)) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = relative.join(entry.file_name());
        match entry.file_type() {
            Ok(t) if t.is_dir() => files.extend(files_under(dir, &path)),
            Ok(_) => files.push(path),
            Err(_) => {}
        }
    }
    files
}

pub(crate) fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    copy_dir_except(from, to, None)
}

/// Copy a folder, leaving out one directory or file when given.
pub(crate) fn copy_dir_except(from: &Path, to: &Path, skip: Option<&Path>) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if Some(entry.path().as_path()) == skip {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_except(&entry.path(), &target, skip)?;
        } else {
            crate::preparer::clone_or_copy(&entry.path(), &target)?;
        }
    }
    Ok(())
}

/// After an install: when `tag` is not prepared but another installed version made the same
/// resources, copies them (not hard links: preparing again rewrites files in place, which would
/// change both). The player then needs neither their Minecraft folder nor a new preparation to
/// play a release that only changed Sunrise. The caller holds the target's preparation lock.
pub(crate) fn reuse_prepared(versions: &Path, tag: &str) -> Option<String> {
    if resources_owner_in(versions, tag) != tag {
        return None;
    }
    let target = versions.join(tag);
    let to = resources_at(&target);
    if to.join(RESOURCES_MANIFEST).exists() {
        return None;
    }
    let version = game_version_at(&target);
    for other in installed_in(versions)
        .into_iter()
        .filter(|t| t != tag && resources_owner_in(versions, t) == *t)
    {
        let source = versions.join(&other);
        if game_version_at(&source) != version {
            continue;
        }
        let Ok(_source) = prepare_lock_at(&source, &other) else {
            continue;
        };
        let from = resources_at(&source);
        if !resources_compatible(&source, &target, &from, &LEGACY_OUTPUT_VERSIONS) {
            continue;
        }
        // Copied beside it and renamed when complete: an interrupted copy is never "prepared".
        // The name is this call's own, as the window's start and an install can both copy.
        static COPIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let partial = to.with_file_name(format!(".{version}.partial-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&partial);
        // Without the data file: it names its own root, so the copy could not use it.
        let data_file = from.join(crate::preparer::DATA_FILE);
        let copied = copy_dir_except(&from, &partial, Some(&data_file)).and_then(|_| {
            if to.join(RESOURCES_MANIFEST).exists() {
                return Ok(false); // Prepared meanwhile, by another copy or the player.
            }
            Resources::invalidate(&target)?;
            let _ = fs::remove_dir_all(&to);
            fs::rename(&partial, &to).map(|_| true)
        });
        let _ = fs::remove_dir_all(&partial);
        match copied {
            Ok(true) => return Some(other),
            Ok(false) => return None,
            Err(_) => {}
        }
    }
    None
}

/// What an install does once its folder is in place (an imported build's too, `build_import`):
/// another installed version's resources if it has none, whatever its resources lack, then its
/// data file.
pub(crate) fn reuse_prepared_for(tag: &str) {
    reuse_prepared_for_cancellable(tag, None);
}

pub(in crate::backend) fn reuse_prepared_for_cancellable(
    tag: &str,
    stop: Option<&crate::native_mods::Cancellation>,
) {
    if stop.is_some_and(crate::native_mods::Cancellation::is_stopped) {
        return;
    }
    let about = crate::native_mods::about(tag);
    let tag = about.as_ref().map_or(tag, |about| about.base.as_str());
    let Ok(preparation) = prepare_lock(tag) else {
        return;
    };
    let versions = versions_dir();
    reuse_prepared(&versions, tag);
    if stop.is_some_and(crate::native_mods::Cancellation::is_stopped) {
        return;
    }
    if check_prepared(tag) {
        if let Err(error) = finish_preparation_cancellable(tag, &preparation, stop) {
            eprintln!("{tag}: {error}");
        }
    }
    drop(preparation);
    if stop.is_some_and(crate::native_mods::Cancellation::is_stopped) {
        return;
    }
    let (root, resources) = (version_dir(tag), resources(tag));
    let spawned = std::thread::Builder::new()
        .name("Prepared data file".into())
        .spawn(move || {
            if let Err(e) = write_data_file(&root, &resources) {
                eprintln!("{}: not written: {e}", resources.display());
            }
        });
    if let Err(e) = spawned {
        eprintln!("{tag}: the data file was not written: {e}");
    }
}

/// Writes a prepared version's `sunrise-data.bin` so its first start already reads one file, not
/// thousands: over a minute under Windows Defender (#1961), and a player who updates would pay it
/// after every update. After `complete_assets`, as the file answers "missing" for what it lacks.
/// The version's own preparer writes it (`data-file`); the game still writes it when this didn't, or the preparer is too old to. Only looks for the file, as the game keeps it open for the whole run, which would stop a removal on Windows.
pub(in crate::backend) fn write_data_file(root: &Path, resources: &Path) -> Result<(), String> {
    if resources.join(crate::preparer::DATA_FILE).exists()
        || check_play_resources(root, resources).is_err()
    {
        return Ok(());
    }
    let version = root.file_name().unwrap_or_default().to_string_lossy();
    let preparation = prepare_lock_at(root, &version)?;
    match crate::preparer::binary(root) {
        Some(binary) => {
            crate::preparer::data_file(&binary, root, resources, Some(&preparation.handoff(None)))
                .map(drop)
        }
        None => Ok(()),
    }
}

/// `reuse_prepared` for every installed version that is not prepared, oldest first, so a
/// version prepared this way can serve the next. Launchers before 0.1.4 installed new versions
/// without it; this catches those up. Returns each (version, where its resources came from).
/// Prepared versions also get the run-time reports (`complete_reports`) and the built-in packs,
/// panorama and sounds (`complete_assets`) they lack.
pub fn reuse_prepared_everywhere() -> Vec<(String, String)> {
    let versions = versions_dir();
    let reused = reuse_prepared_in(&versions);
    forget_prepared();
    for tag in installed_in(&versions)
        .into_iter()
        .filter(|tag| resources_owner(tag) == *tag)
    {
        let was_reused = reused.iter().any(|(prepared, _)| prepared == &tag);
        // Probe unlocked once; lock only when this base was reused or actually lacks extras.
        let missing = probe_missing_extras(&versions, &tag);
        if !was_reused && missing.as_ref().is_none_or(Vec::is_empty) {
            continue;
        }
        // Here, on the start-up thread, and not in `complete_assets`, which Play also runs: a
        // download of thousands of files is not for the moment the game is waited for.
        let Ok(preparation) = prepare_lock(&tag) else {
            continue;
        };
        let handoff = preparation.handoff(None);
        refill_own_store(&tag, missing.as_ref());
        if was_reused {
            if let Err(error) = finish_preparation_with_missing(&tag, &preparation, missing) {
                eprintln!("{tag}: {error}");
            }
        } else if let Some(lacking) = missing {
            complete_assets_lacking_staged(&tag, &handoff, lacking);
        }
    }
    reused
}

pub(in crate::backend) fn reuse_prepared_in(versions: &Path) -> Vec<(String, String)> {
    let mut tags = installed_in(versions);
    tags.reverse();
    tags.into_iter()
        .filter(|tag| resources_owner_in(versions, tag) == *tag)
        .filter_map(|tag| {
            let target = versions.join(&tag);
            let to = resources_at(&target);
            if to.join(RESOURCES_MANIFEST).exists() && has_runtime_reports(&to) {
                return None;
            }
            let _preparation = prepare_lock_at(&target, &tag).ok()?;
            let from = reuse_prepared(versions, &tag);
            complete_reports(versions, &tag);
            from.map(|from| (tag, from))
        })
        .collect()
}
