//! A world's data packs: `<world>/datapacks/`, each a `.zip` or a folder with a `pack.mcmeta`, as
//! the game discovers them (`datapack::discover`). The game turns on every pack it finds there
//! that the world's level.dat (`Data.DataPacks.Disabled`) doesn't list as disabled, when the world
//! opens or on `/reload`, so adding one is copying it in. While a `<world>/datapacks.json` still
//! exists it is read instead of level.dat; the launcher never creates one.
use crate::i18n::{tr, trf};
use crate::packs::{entry, pack_description, pack_meta};
use std::fs;
use std::path::{Path, PathBuf};
use sunrise_launcher_nbt::{JavaString, Tag};

// Removal: the first release after 26.3-0.1.0 drops every `datapacks.json` use below (the read in
// `disabled` and the json half of `forget`, with this constant), together with the game's
// migration of that file into level.dat.
const SETTINGS: &str = "datapacks.json";

#[derive(Clone, Debug, PartialEq)]
pub struct DataPack {
    /// Its file or folder name in `datapacks/`; the game's id is `file/<name>`.
    pub file: String,
    pub path: PathBuf,
    /// `pack.mcmeta`'s description as plain text.
    pub description: String,
    /// Off when the world's level.dat lists it in `Data.DataPacks.Disabled` (the game's
    /// `/datapack disable`).
    pub enabled: bool,
}

/// `datapacks.json`, as the game's migration will read it.
fn settings(world: &Path) -> serde_json::Value {
    fs::read(world.join(SETTINGS))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// The ids the world has turned off. A `datapacks.json` wins while it exists (the fallback to
/// remove with the file); otherwise level.dat's `Data.DataPacks.Disabled`, and a world without
/// `DataPacks` has every pack on.
fn disabled(world: &Path) -> Vec<String> {
    if world.join(SETTINGS).exists() {
        return settings(world)["disabled"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    let root = crate::worlds::level_dat(&world.join("level.dat"));
    let Some(Tag::List(ids)) = root
        .as_ref()
        .and_then(|r| r.get("Data")?.get("DataPacks")?.get("Disabled"))
    else {
        return Vec::new();
    };
    ids.iter()
        .filter_map(|t| match t {
            Tag::String(s) => Some(s.to_string_lossy()),
            _ => None,
        })
        .collect()
}

/// The data packs of the world folder `world`, by name.
pub fn list(world: &Path) -> Vec<DataPack> {
    let off = disabled(world);
    let mut packs: Vec<DataPack> = fs::read_dir(world.join("datapacks"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let file = e.file_name().to_str()?.to_string();
            let zip = path.is_file() && file.to_ascii_lowercase().ends_with(".zip");
            if !(zip || path.is_dir()) {
                return None;
            }
            let description = pack_description(&pack_meta(&entry(&path, "pack.mcmeta")?)?)?;
            Some(DataPack {
                enabled: !off.contains(&format!("file/{file}")),
                description,
                file,
                path,
            })
        })
        .collect();
    packs.sort_by_key(|p| p.file.to_lowercase());
    packs
}

/// Whether a zip is a data pack: a `pack.mcmeta` and a `data/` folder.
fn check(file: &Path) -> Result<(), String> {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let archive = fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let zip = crate::zip_intake::open(archive).map_err(|e| format!("{}: {e}", file.display()))?;
    if zip.index_for_name("pack.mcmeta").is_none() {
        return Err(trf(
            "{name} is not a pack: it has no pack.mcmeta.",
            &[("name", &name)],
        ));
    }
    // What `list` needs to show it, checked before the pack is copied.
    if entry(file, "pack.mcmeta")
        .and_then(|bytes| pack_meta(&bytes))
        .and_then(|meta| pack_description(&meta))
        .is_none()
    {
        return Err(trf(
            "{name} is not a data pack the game reads.",
            &[("name", &name)],
        ));
    }
    let names: Vec<_> = zip.file_names().collect::<Result<_, _>>().map_err(|e| {
        trf(
            "{name} has an entry whose name can't be read: {error}",
            &[("name", &name), ("error", &e.to_string())],
        )
    })?;
    if !names.iter().any(|n| n.starts_with("data/")) {
        return Err(trf(
            "{name} has no data folder: it is a resource pack, not a data pack.",
            &[("name", &name)],
        ));
    }
    Ok(())
}

/// Copies a data pack zip into the world's `datapacks/` under a free name. The game turns it on
/// the next time the world opens (or on `/reload`).
pub fn add(world: &Path, file: &Path) -> Result<DataPack, String> {
    check(file)?;
    let _claim = crate::worlds::lock_saved_dir(world)?;
    let dir = world.join("datapacks");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = file.file_name().ok_or("A pack file has a name")?;
    let stem = Path::new(name)
        .file_stem()
        .unwrap_or(name)
        .to_string_lossy();
    let target = (1..)
        .map(|n| match n {
            1 => dir.join(name),
            n => dir.join(format!("{stem} ({n}).zip")),
        })
        .find(|p| !p.exists())
        .expect("a folder can't hold a pack under every numbered name");
    fs::copy(file, &target).map_err(|e| {
        trf(
            "Could not copy {file}: {error}",
            &[
                ("file", &file.display().to_string()),
                ("error", &e.to_string()),
            ],
        )
    })?;
    let file_name = target
        .file_name()
        .expect("the target is a name joined onto datapacks/")
        .to_string_lossy()
        .into_owned();
    // A pack of the same name removed while it was disabled would stay off.
    forget(world, &file_name)?;
    list(world)
        .into_iter()
        .find(|p| p.file == file_name)
        .ok_or_else(|| {
            trf(
                "{file} is not a data pack the game reads",
                &[("file", &target.display().to_string())],
            )
        })
}

/// Drops `file/<name>` from `Data.DataPacks.Enabled` and `Disabled` in level.dat, and changes no
/// other tag. Called with the world's lock held, so the game is not running it. A world without
/// a readable level.dat or without `DataPacks` has nothing to drop.
fn forget(world: &Path, file: &str) -> Result<(), String> {
    let id = format!("file/{file}");
    let java_id = JavaString::from(id.as_str());
    if let Some(mut root) = crate::worlds::level_dat(&world.join("level.dat")) {
        let mut changed = false;
        if let Some(packs) = root.get_mut("Data").and_then(|d| d.get_mut("DataPacks")) {
            for key in ["Enabled", "Disabled"] {
                if let Some(Tag::List(list)) = packs.get_mut(key) {
                    let before = list.len();
                    list.retain(|t| !matches!(t, Tag::String(s) if *s == java_id));
                    changed |= list.len() != before;
                }
            }
        }
        if changed {
            crate::worlds::save_level_dat(world, root).map_err(|e| format!("level.dat: {e}"))?;
        }
    }
    // Removal: the json half goes with the fallback in `disabled`, in the first release after
    // 26.3-0.1.0. It only edits a file that is already there, so the game's migration carries the
    // right selection; it never creates one.
    if !world.join(SETTINGS).exists() {
        return Ok(());
    }
    let mut s = settings(world);
    let mut changed = false;
    for key in ["enabled", "disabled"] {
        if let Some(list) = s[key].as_array_mut() {
            let before = list.len();
            list.retain(|v| v.as_str() != Some(id.as_str()));
            changed |= list.len() != before;
        }
    }
    if !changed {
        return Ok(());
    }
    let text = serde_json::to_string_pretty(&s).expect("a JSON value always serialises");
    fs::write(world.join(SETTINGS), text).map_err(|e| format!("{SETTINGS}: {e}"))
}

/// The data pack a command names: its file name, with or without `.zip`.
pub fn find(world: &Path, query: &str) -> Result<DataPack, String> {
    list(world)
        .into_iter()
        .find(|p| p.file == query || p.file.strip_suffix(".zip") == Some(query))
        .ok_or_else(|| {
            trf(
                "No data pack {query} in {folder}",
                &[
                    ("query", query),
                    ("folder", &world.join("datapacks").display().to_string()),
                ],
            )
        })
}

/// Moves the data pack to the system's trash, or into `trash` when one is given (a test's
/// scratch folder), and forgets it in the world's level.dat.
pub fn remove(world: &Path, pack: &DataPack, trash: Option<&Path>) -> Result<(), String> {
    let _claim = crate::worlds::lock_saved_dir(world)?;
    match trash {
        Some(folder) => crate::desktop_seams::to_folder(&pack.path, folder),
        None => crate::desktop_seams::to_trash(&pack.path),
    }
    .map_err(|e| {
        trf(
            "Could not move {file} to the trash: {error}",
            &[("file", &pack.file), ("error", &e.to_string())],
        )
    })?;
    forget(world, &pack.file)
}

/// What of a data pack the game reads today (`DataStore::load`, `worldgen_overrides`):
/// `data/<namespace>/<kind>/`. A tag is `<registry> tag`; worldgen is `worldgen` for the folders
/// in `WORLDGEN_READ` and `worldgen <folder>` for the rest.
pub const READ: [&str; 25] = [
    "function",
    "function tag",
    "predicate",
    "item_modifier",
    "recipe",
    "recipe_filter",
    "loot_table",
    "advancement",
    "villager_trade",
    "trade_set",
    "damage_type",
    "dimension",
    "dimension_type",
    "structure",
    "worldgen",
    "item tag",
    "block tag",
    "fluid tag",
    "enchantment tag",
    "instrument tag",
    "entity_type tag",
    "damage_type tag",
    "dialog tag",
    "biome tag",
    "dialog",
];

/// The `worldgen/` folders the chunk generator reads (`overrides::KINDS`).
const WORLDGEN_READ: [&str; 14] = [
    "biome",
    "block_state_provider",
    "carver",
    "feature",
    "density_function",
    "material_condition",
    "material_rule",
    "noise",
    "noise_settings",
    "placed_feature",
    "processor_list",
    "structure",
    "structure_set",
    "template_pool",
];

/// A data pack's files by kind (`function`, `worldgen`, `item tag`...), with how many each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Coverage {
    /// What Sunrise runs.
    pub read: Vec<(String, usize)>,
    /// What it doesn't read yet.
    pub unread: Vec<(String, usize)>,
}

impl Coverage {
    /// "Runs its 12 functions, 1 predicate. Not read yet: 40 worldgen, 3 recipes."
    pub fn summary(&self) -> String {
        let runs = match self.read.is_empty() {
            true => tr("Nothing in it runs in Sunrise yet.").to_string(),
            false => trf("Runs its {counts}.", &[("counts", &counts(&self.read))]),
        };
        match self.unread.is_empty() {
            true => runs,
            false => trf(
                "{runs} Not read yet: {unread}.",
                &[("runs", &runs), ("unread", &self.not_read())],
            ),
        }
    }

    /// "1077 worldgen, 653 loot tables, 16 recipes": what Sunrise doesn't read yet, the six
    /// biggest kinds and how many other kinds.
    pub fn not_read(&self) -> String {
        let mut biggest = self.unread.clone();
        biggest.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let others = biggest.len().saturating_sub(6);
        biggest.truncate(6);
        match others {
            0 => counts(&biggest),
            1 => trf("{kinds} and 1 other kind", &[("kinds", &counts(&biggest))]),
            n => trf(
                "{kinds} and {n} other kinds",
                &[("kinds", &counts(&biggest)), ("n", &n.to_string())],
            ),
        }
    }
}

fn counts(list: &[(String, usize)]) -> String {
    list.iter()
        .map(|(k, n)| {
            let k = k.replace('_', " ");
            match *n == 1 || k == "worldgen" {
                true => format!("{n} {k}"),
                false => format!("{n} {k}s"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Counts a data pack zip's files by kind, and splits them into what the game reads and what
/// it doesn't yet.
pub fn coverage(file: &Path) -> Result<Coverage, String> {
    let archive = fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let zip = crate::zip_intake::open(archive).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    let names: Vec<_> = zip
        .file_names()
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", file.display()))?;
    for name in names.iter().filter(|n| !n.ends_with('/')) {
        let parts: Vec<&str> = name.split('/').collect();
        let kind = match parts.as_slice() {
            ["data", _, "tags", "worldgen", registry, _, ..] => format!("{registry} tag"),
            ["data", _, "tags", tag, _, ..] => format!("{tag} tag"),
            ["data", _, "worldgen", folder, _, ..] if WORLDGEN_READ.contains(folder) => {
                "worldgen".to_string()
            }
            ["data", _, "worldgen", folder, _, ..] => format!("worldgen {folder}"),
            ["data", _, kind, _, ..] => kind.to_string(),
            _ => continue,
        };
        *kinds.entry(kind).or_default() += 1;
    }
    let (read, unread) = kinds
        .into_iter()
        .partition(|(k, _)| READ.contains(&k.as_str()));
    Ok(Coverage { read, unread })
}

#[cfg(test)]
mod tests;
