//! A file given to `instance import`: what it is, and an instance made from a shared one.
//!
//! - A `.zip` laid out as a game folder, such as [`crate::export`] writes: a new instance.
//! - A zip with `pack.mcmeta`: a data pack (`data/`) or a resource pack; with `shaders/`, or a
//!   `.sshader`: a shader pack.
//!
//! Only what an instance holds is taken from a shared zip ([`crate::export::FILES`],
//! [`crate::export::FOLDERS`], `saves/`), and every path is checked to stay inside its folder.
use crate::browser::installed;
use crate::export::{FILES, FOLDERS};
use crate::i18n::{tr, trf};
use crate::instances::{self, Instance};
use crate::packs;
use crate::zip_intake::{self, Archive};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// What a dropped file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// A shared instance: a new instance is made from it.
    Instance,
    /// A resource or shader pack, for the selected instance.
    Pack(packs::Kind),
    /// A data pack: it goes into a world.
    DataPack,
    /// A game build: a `sunrise-client` program, or a release archive or an
    /// Actions artifact around one. `build_import` installs it as its own
    /// version beside the releases, so a build can be played without waiting
    /// for a Nightly.
    Build,
}

/// Whether `file` holds a game build (`build_import::holds_game`). Asked only
/// where the cheap checks have not matched: it reads the whole program.
fn is_build(file: &Path) -> bool {
    crate::build_import::holds_game(file)
}

fn open(file: &Path) -> Result<Archive<fs::File>, String> {
    let archive = fs::File::open(file).map_err(|e| format!("{}: {e}", name_of(file)))?;
    zip_intake::open(archive).map_err(|e| format!("{}: {e}", name_of(file)))
}

fn name_of(file: &Path) -> String {
    file.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// What `file` is, from its name and what is inside.
pub fn what(file: &Path) -> Result<Dropped, String> {
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    match ext.as_deref() {
        Some("sshader") => return packs::kind_of(file).map(Dropped::Pack),
        // A game build: the program itself, or a release archive around it.
        // Taken on the extension alone, unread, so that `build_import` is the
        // one to say why a file with this name is not a game -- a better answer
        // than this function's "nothing in it that Sunrise reads".
        Some("exe" | "gz" | "tgz") => return Ok(Dropped::Build),
        Some("mrpack") => {
            return Err(trf(
                "{name} is a Modrinth modpack: Sunrise doesn't import those.",
                &[("name", &name_of(file))],
            ))
        }
        Some("zip") => {}
        // A build for Linux or macOS is named `sunrise-client` with no
        // extension, so a file nothing else claims is read before giving up.
        _ if is_build(file) => return Ok(Dropped::Build),
        _ => {
            let kinds = crate::i18n::tr_key("a .zip, a .sshader or a Sunrise game build");
            return Err(trf(
                "{name} can't be imported: drop {kinds}.",
                &[("name", &name_of(file)), ("kinds", tr(kinds))],
            ));
        }
    }
    let zip = open(file)?;
    let has = |n: &str| zip.index_for_name(n).is_some();
    let names: Vec<_> = zip
        .file_names()
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let under = |p: &str| names.iter().any(|n| n.starts_with(p));
    if !has("pack.mcmeta") && (has("manifest.json") || prism_instance(&names)) {
        return Err(trf(
            "{name} is a Java modpack: Sunrise doesn't import those.",
            &[("name", &name_of(file))],
        ));
    }
    if has("pack.mcmeta") {
        return Ok(match under("data/") {
            true => Dropped::DataPack,
            false => Dropped::Pack(packs::Kind::Resource),
        });
    }
    if under("shaders/") {
        return Ok(Dropped::Pack(packs::Kind::Shader));
    }
    if FILES.iter().any(|f| has(f))
        || under("saves/")
        || FOLDERS.iter().any(|f| under(&format!("{f}/")))
    {
        return Ok(Dropped::Instance);
    }
    // An Actions artifact is a zip holding the release archive: the last thing
    // a zip can be, once nothing a pack or an instance holds has matched.
    if is_build(file) {
        return Ok(Dropped::Build);
    }
    Err(trf(
        "{name} is neither a pack, a shared profile nor a game build: nothing in it that Sunrise reads.",
        &[("name", &name_of(file))],
    ))
}

/// Where an archive entry goes in the instance's folder, when it is one an instance holds and
/// stays inside it.
fn target(home: &Path, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    if relative.ends_with('/') || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    // The browser's list of what it installed is the launcher's, never a pack's: it names the
    // files Update and Remove delete.
    if relative.eq_ignore_ascii_case(installed::MANIFEST) {
        return None;
    }
    let top = relative.split('/').next()?;
    let kept = FILES.contains(&relative)
        || (relative.contains('/') && (FOLDERS.contains(&top) || top == "saves"));
    kept.then(|| home.join(path))
}

/// Makes a new instance from a shared one: `file` is a game folder `.zip`. Named after the
/// file, with "(2)" when the name is taken.
pub fn import_instance(file: &Path, progress: &dyn Fn(&str, f32)) -> Result<Instance, String> {
    import_into(file, progress, &|name| instances::create(name, None))
}

/// The most a shared instance may unpack to, all files together. A zip's sizes are its
/// own claim, so [`fill`] counts the bytes actually inflated: a few kilobytes of zip bomb can't fill the
/// disk.
pub const MAX_IMPORT: u64 = 8 << 30;

/// [`import_instance`] with the instance made by `create`.
pub fn import_into(
    file: &Path,
    progress: &dyn Fn(&str, f32),
    create: &dyn Fn(&str) -> Result<Instance, String>,
) -> Result<Instance, String> {
    if what(file)? != Dropped::Instance {
        return Err(trf(
            "{name} is not a shared profile.",
            &[("name", &name_of(file))],
        ));
    }
    import_any(file, progress, create, MAX_IMPORT)
}

/// [`import_into`] without asking what the file is, with at most `limit` bytes unpacked from the
/// archive.
fn import_any(
    file: &Path,
    progress: &dyn Fn(&str, f32),
    create: &dyn Fn(&str) -> Result<Instance, String>,
    limit: u64,
) -> Result<Instance, String> {
    let mut zip = open(file)?;
    let base = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Imported".into());
    let instance = (1..20)
        .map(|n| match n {
            1 => base.clone(),
            n => format!("{base} ({n})"),
        })
        .find_map(|name| create(&name).ok())
        .ok_or_else(|| {
            trf(
                "Could not make a profile called {name}.",
                &[("name", &base)],
            )
        })?;
    let home = instance.home.clone();
    let filled = fill(&mut zip, &home, progress, limit);
    if let Err(e) = filled {
        let _ = fs::remove_dir_all(&home);
        return Err(e);
    }
    Ok(instance)
}

/// `limit` as people read it: GiB from 1 GiB up, else KiB.
fn size_text(limit: u64) -> String {
    if limit >= 1 << 30 {
        format!("{} GiB", limit >> 30)
    } else {
        format!("{} KiB", limit.div_ceil(1024))
    }
}

fn fill(
    zip: &mut Archive<fs::File>,
    home: &Path,
    progress: &dyn Fn(&str, f32),
    limit: u64,
) -> Result<(), String> {
    let count = zip.len();
    let mut total = 0u64;
    for i in 0..count {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        let Some(to) = target(home, &name) else {
            continue;
        };
        fs::create_dir_all(to.parent().expect("inside home")).map_err(|e| e.to_string())?;
        let mut out = fs::File::create(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        // At most one byte past what is left: reading it means the archive holds more than `limit`.
        let room = limit.saturating_sub(total);
        let written = std::io::copy(&mut (&mut entry).take(room.saturating_add(1)), &mut out)
            .map_err(|e| format!("{}: {e}", to.display()))?;
        total = total.saturating_add(written);
        if written > room {
            return Err(crate::i18n::trf(
                "{file} unpacks to more than {limit}: it isn't imported.",
                &[("file", &name_of(&to)), ("limit", &size_text(limit))],
            ));
        }
        progress(
            &trf("Copying {name}", &[("name", &name)]),
            (i + 1) as f32 / count as f32,
        );
    }
    Ok(())
}

/// Whether a zip holds a Prism or MultiMC instance: an `mmc-pack.json` at its top or one folder down.
fn prism_instance<S: AsRef<str>>(names: &[S]) -> bool {
    names.iter().any(|name| {
        name.as_ref()
            .strip_suffix("mmc-pack.json")
            .is_some_and(|root| {
                root.is_empty()
                    || root
                        .strip_suffix('/')
                        .is_some_and(|folder| !folder.is_empty() && !folder.contains('/'))
            })
    })
}

#[cfg(test)]
mod tests;
