//! The servers' programs: an installed Sunrise version, laid out as install.sh lays it out.

use super::*;

pub fn runtime_root() -> PathBuf {
    backend::data_dir().join("server-runtime")
}

/// `sunrise-server` of the release in use, once one is set up.
pub fn program() -> Option<PathBuf> {
    Some(runtime_root().join("current").join(PROGRAM)).filter(|p| p.is_file())
}

/// The release the servers run.
pub fn runtime_release() -> Option<String> {
    let target = fs::canonicalize(runtime_root().join("current")).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// Where `sunrise-server` finds the game files: the folder install.sh keeps across updates.
pub fn game_files(root: &Path, game_version: &str) -> PathBuf {
    root.join("game-files/sunrise-client").join(game_version)
}

/// Whether the servers' game files are there.
pub fn game_files_ready() -> bool {
    let root = runtime_root();
    // The current release carries the lock it was built with, even after the
    // launcher installation it came from has been removed.
    backend::hosting_game_version(&root.join("current")).is_ok_and(|game_version| {
        game_files(&root, &game_version)
            .join("sunrise-resources.json")
            .is_file()
    })
}

/// Whether an installed version can host: it ships `sunrise-server` and is prepared.
pub fn can_host(tag: &str) -> Result<(), String> {
    host_resources(tag).map(|_| ()).map_err(String::from)
}

/// Why the servers' programs couldn't be set up. Only [`SetUpError::NotPrepared`] is answered
/// with Prepare; every other reason (a prepare already running, a file that couldn't be copied)
/// is only said.
#[derive(Debug, PartialEq)]
pub enum SetUpError {
    /// The version's game files aren't prepared yet.
    NotPrepared(String),
    Other(String),
}

impl std::fmt::Display for SetUpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPrepared(message) | Self::Other(message) => f.write_str(message),
        }
    }
}

impl From<String> for SetUpError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl From<SetUpError> for String {
    fn from(error: SetUpError) -> Self {
        error.to_string()
    }
}

struct HostResources {
    owner: String,
    base: PathBuf,
    game_version: String,
    folder: PathBuf,
}

fn host_resources(tag: &str) -> Result<HostResources, SetUpError> {
    if !backend::version_dir(tag).join(PROGRAM).is_file() {
        return Err(SetUpError::Other(trf(
            "Sunrise {tag} has no server program. Install a newer game build first.",
            &[("tag", tag)],
        )));
    }
    let (owner, base) = backend::installed_resources_root(tag)?;
    let game_version = backend::hosting_game_version(&base)?;
    let folder = backend::resources_for_game(&base, &game_version);
    if !backend::prepared_at(&folder) {
        return Err(SetUpError::NotPrepared(trf(
            "Sunrise {tag} isn't prepared yet: press Prepare on the Play page (it uses your own Minecraft {version})",
            &[("tag", tag), ("version", game_version.as_str())],
        )));
    }
    Ok(HostResources {
        owner,
        base,
        game_version,
        folder,
    })
}

/// Sets up the servers' programs from the installed version `tag` (if none are yet) and copies its
/// prepared game files for them. Safe to run again: it only refreshes the game files.
pub fn set_up(tag: &str) -> Result<(), SetUpError> {
    let resources = host_resources(tag)?;
    // Keep the base's prepared files stable while completing reports and copying them.
    let _preparation = backend::prepare_lock_at(&resources.base, &resources.owner)?;
    backend::complete_reports_at(
        &backend::versions_dir(),
        &resources.owner,
        &resources.base,
        &resources.folder,
    );
    set_up_in(
        &runtime_root(),
        &backend::version_dir(tag),
        tag,
        &resources.game_version,
        &resources.folder,
    )
    .map_err(SetUpError::Other)
}

/// Copies a release with its already validated version and prepared resource folder.
pub(super) fn set_up_in(
    root: &Path,
    version: &Path,
    tag: &str,
    game_version: &str,
    resources: &Path,
) -> Result<(), String> {
    let releases = root.join("releases");
    fs::create_dir_all(&releases).map_err(|e| format!("{}: {e}", releases.display()))?;
    let release = releases.join(tag);
    let current = root.join("current");
    if current.exists() {
        let active_version = backend::hosting_game_version(&current)?;
        if active_version != game_version {
            return Err(trf(
                "The active server runtime uses Minecraft {active}, but Sunrise {tag} uses Minecraft {selected}. Choose a Sunrise build for Minecraft {active}.",
                &[
                    ("active", &active_version),
                    ("tag", tag),
                    ("selected", game_version),
                ],
            ));
        }
    } else {
        if !release.join(PROGRAM).is_file() {
            let scratch = releases.join(format!(".{tag}.copying"));
            let _ = fs::remove_dir_all(&scratch);
            copy_release(version, &scratch).map_err(|e| {
                trf(
                    "copying {tag}: {error}",
                    &[("tag", tag), ("error", &e.to_string())],
                )
            })?;
            let _ = fs::remove_dir_all(&release);
            fs::rename(&scratch, &release).map_err(|e| e.to_string())?;
        }
        let local = release.join(".local");
        fs::create_dir_all(&local).map_err(|e| e.to_string())?;
        let link = local.join("sunrise-client");
        if link.symlink_metadata().is_err() {
            fs::create_dir_all(root.join("game-files/sunrise-client"))
                .map_err(|e| e.to_string())?;
            point(&link, &root.join("game-files/sunrise-client")).map_err(|e| e.to_string())?;
        }
        point(&current, &release).map_err(|e| e.to_string())?;
    }
    // The game files: a copy (preparing again rewrites files in place), swapped in whole.
    // Native builds share their base's prepared resources, as Play does; only the
    // programs above come from the selected build.
    let from = resources;
    let to = game_files(root, game_version);
    let fresh = to.with_file_name(format!(".{game_version}.copying"));
    let _ = fs::remove_dir_all(&fresh);
    copy_dir(from, &fresh).map_err(|e| {
        format!(
            "copying the game files from {} to {}: {e}",
            from.display(),
            fresh.display()
        )
    })?;
    let old = to.with_file_name(format!(".{game_version}.old"));
    let _ = fs::remove_dir_all(&old);
    if to.exists() {
        fs::rename(&to, &old).map_err(|e| e.to_string())?;
    }
    fs::rename(&fresh, &to).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

/// A release folder without the launcher's own files (its prepared copy, install marker and the
/// game's macOS app bundle).
fn copy_release(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".local" || name == ".installed" || name == "Sunrise.app" {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Makes `link` point at the folder `target`, as install.sh (a symbolic link) and install.ps1 (a
/// junction, which needs no admin rights) do.
pub(super) fn point(link: &Path, target: &Path) -> std::io::Result<()> {
    let fresh = link.with_extension("switching");
    let _ = fs::remove_file(&fresh);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, &fresh)?;
        fs::rename(&fresh, link)
    }
    #[cfg(windows)]
    {
        let _ = fs::remove_dir(link);
        crate::sources::junction(target, link)
    }
}
