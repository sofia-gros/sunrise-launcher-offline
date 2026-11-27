//! Sharing an instance: its settings, packs and Sunrise mods, and the worlds the player picks,
//! as a plain `.zip` of the game folder's layout.
//!
//! Only what makes the instance goes in, from a fixed list ([`FILES`], [`FOLDERS`]): never logs,
//! screenshots, backups, caches, or anything tied to the player's account. A world goes without
//! its per-player files (`playerdata/`, `players/`, `stats/`, `advancements/`) and its level.dat without the
//! player's UUID. The player's server list (`servers.dat`) stays out too.
use crate::i18n::trf;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use sunrise_launcher_nbt::{self as nbt, Limits, Tag};

/// The game folder's own files that are the instance's settings.
pub const FILES: [&str; 6] = [
    crate::paths::OPTIONS,
    "sunrise-graphics.txt",
    "sunrise-extras.txt",
    "sunrise-controller.txt",
    "sunrise-mods.txt",
    "servers.dat",
];
/// Its folders of packs, mods and their settings.
pub const FOLDERS: [&str; 4] = ["resourcepacks", "shaderpacks", "sunrise-mods", "config"];
/// What a world keeps per player: left out.
const PER_PLAYER: [&str; 5] = [
    "playerdata",
    "players",
    "stats",
    "advancements",
    "session.lock",
];
/// Read when a shared zip is imported, but never written: the servers the player plays on are
/// theirs, not part of the instance they share.
const NOT_EXPORTED: [&str; 1] = ["servers.dat"];

/// What an export wrote.
#[derive(Debug, PartialEq)]
pub struct Exported {
    pub path: PathBuf,
    /// Files copied in.
    pub copied: usize,
    /// The worlds in it, by folder.
    pub worlds: Vec<String>,
}

/// Writes the instance whose game folder is `home` to `out`. `worlds` are folders of its
/// `saves/`.
pub fn export(home: &Path, name: &str, worlds: &[String], out: &Path) -> Result<Exported, String> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for f in FILES {
        if !NOT_EXPORTED.contains(&f) && home.join(f).is_file() {
            files.push((f.to_string(), home.join(f)));
        }
    }
    for folder in FOLDERS {
        walk(home, &home.join(folder), &[], &mut files);
    }
    for world in worlds {
        let dir = home.join("saves").join(world);
        if world.contains(['/', '\\']) || !dir.join("level.dat").is_file() {
            return Err(trf(
                "{name} has no world {world}.",
                &[("name", name), ("world", world)],
            ));
        }
        walk(home, &dir, &PER_PLAYER, &mut files);
    }
    if files.is_empty() {
        return Err(trf(
            "{name} has nothing to share yet: no settings, packs, mods or worlds.",
            &[("name", name)],
        ));
    }
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let part = out.with_extension("part");
    let written = write(&part, &files);
    let done = written.and_then(|()| fs::rename(&part, out).map_err(|e| e.to_string()));
    if let Err(e) = done {
        let _ = fs::remove_file(&part);
        return Err(trf(
            "Could not export {name}: {error}",
            &[("name", name), ("error", &e)],
        ));
    }
    Ok(Exported {
        path: out.to_path_buf(),
        copied: files.len(),
        worlds: worlds.to_vec(),
    })
}

fn write(path: &Path, files: &[(String, PathBuf)]) -> Result<(), String> {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).map_err(|e| e.to_string())?);
    let options = zip::write::SimpleFileOptions::default();
    for (rel, file) in files {
        let bytes = match rel.as_str() {
            rel if rel.ends_with("/level.dat") || rel.ends_with("/level.dat_old") => {
                shareable_level(file)?
            }
            crate::paths::OPTIONS => shareable_options(file)?,
            _ => fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?,
        };
        zip.start_file(rel, options).map_err(|e| e.to_string())?;
        zip.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Every file under `dir`, relative to `home` with `/`, leaving out the names in `skip` at
/// `dir`'s top and dot files.
fn walk(home: &Path, dir: &Path, skip: &[&str], out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || skip.contains(&name.as_str()) {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            walk(home, &path, &[], out);
        } else if let Ok(rel) = path.strip_prefix(home) {
            out.push((rel.to_string_lossy().replace('\\', "/"), path));
        }
    }
}

/// `options.txt` without its `lastServer:` line: the address the player last connected to is theirs,
/// not part of the instance they share. Filtered as bytes, so a file that is not UTF-8 loses it too;
/// every other line keeps its bytes and its ending.
fn shareable_options(file: &Path) -> Result<Vec<u8>, String> {
    let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = &bytes[..];
    while !rest.is_empty() {
        // The game ends a line at LF, CRLF or a bare CR, as `BufferedReader.lines()` does.
        let end = match rest.iter().position(|b| matches!(b, b'\r' | b'\n')) {
            Some(i) if rest[i] == b'\r' && rest.get(i + 1) == Some(&b'\n') => i + 2,
            Some(i) => i + 1,
            None => rest.len(),
        };
        let (line, tail) = rest.split_at(end);
        if !line.starts_with(b"lastServer:") {
            out.extend_from_slice(line);
        }
        rest = tail;
    }
    Ok(out)
}

/// A world's level.dat (or its level.dat_old) without the player's UUID, in the player's own
/// entry and as the world's single-player owner; unreadable, it goes as it is.
fn shareable_level(file: &Path) -> Result<Vec<u8>, String> {
    let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut raw = Vec::new();
    if flate2::read::GzDecoder::new(&bytes[..])
        .take(64 << 20)
        .read_to_end(&mut raw)
        .is_err()
    {
        return Ok(bytes);
    }
    let Ok((mut root, _)) = nbt::read_named(&raw, Limits::default()) else {
        return Ok(bytes);
    };
    if let Some(Tag::Compound(data)) = root.tag.get_mut("Data") {
        data.remove(&"singleplayer_uuid".into());
        if let Some(Tag::Compound(player)) = data.get_mut(&"Player".into()) {
            player.remove(&"UUID".into());
        }
    }
    let raw = nbt::write_named(&root, Limits::default()).map_err(|e| e.to_string())?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&raw).map_err(|e| e.to_string())?;
    gz.finish().map_err(|e| e.to_string())
}

/// A free path for `name.zip` in `dir`: "Name (2).zip" when taken.
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    let clean: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c => c,
        })
        .collect();
    (1..)
        .map(|n| match n {
            1 => dir.join(format!("{clean}.zip")),
            n => dir.join(format!("{clean} ({n}).zip")),
        })
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(test)]
mod tests;
