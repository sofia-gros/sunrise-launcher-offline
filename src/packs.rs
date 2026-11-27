//! An instance's resource packs and shader packs, as the game finds and picks them.
//!
//! - Resource packs: `resourcepacks/` in the game folder, each a `.zip` or a folder with a
//!   `pack.mcmeta`. The ones turned on are `options.txt`'s `resourcePacks` list (vanilla's
//!   format, `"file/<name>"`, the bottom first), so the game's Resource Packs screen shows the
//!   same choice. A pack turned on here goes on top, as the screen puts a newly picked one.
//! - Shader packs: `shaderpacks/`, Sunrise's own (a folder with `sunrise-shaders.toml`, or a
//!   `.sshader`) and those in the `shaders/` format (a `.zip`, or a folder with `shaders/`), as
//!   the game's Shader Packs page lists them (`shader_pack_list`). One at most is on:
//!   `pickedShaderPack` in `sunrise-graphics.txt`.
//!
//! Every other line of both files is written back as it was.
use crate::i18n::{tr, trf};
use crate::paths::OPTIONS;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const GRAPHICS: &str = "sunrise-graphics.txt";
const SHADER_MANIFEST: &str = "sunrise-shaders.toml";
const SHADER_PACKAGE: &str = "sshader";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Resource,
    Shader,
}

impl Kind {
    pub fn folder(self) -> &'static str {
        match self {
            Kind::Resource => "resourcepacks",
            Kind::Shader => "shaderpacks",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pack {
    pub kind: Kind,
    /// Its file or folder name in the pack folder: the id commands take.
    pub file: String,
    pub path: PathBuf,
    /// A Sunrise shader pack's own name, else the file's.
    pub title: String,
    /// `pack.mcmeta`'s description as plain text; a shader pack's format.
    pub description: String,
    /// Its picture: a resource pack's `pack.png`, a Sunrise shader pack's `icon.png`.
    pub icon: Option<Vec<u8>>,
    pub enabled: bool,
}

fn shader_key(path: &Path) -> Option<String> {
    if !sunrise_launcher_shader_api::package::is_pack(path) {
        return None;
    }
    let manifest = if path.is_dir() {
        sunrise_launcher_shader_api::Pack::read_manifest(path)
    } else {
        let mut zip = crate::zip_intake::open(regular(path)?).ok()?;
        sunrise_launcher_shader_api::Pack::read_zip_manifest(&mut zip)
    }
    .ok()?;
    Some(format!("sunrise:{}", manifest.id))
}

/// Refuses a Sunrise shader pack whose id an installed one already has: two would show as one
/// pack, and which one the game ran would depend on their file names.
fn refuse_installed_shader(dir: &Path, incoming: &Path) -> Result<(), String> {
    let Some(key) = shader_key(incoming) else {
        return Ok(());
    };
    let installed = fs::read_dir(dir).into_iter().flatten().flatten().find(|e| {
        !e.file_name().to_string_lossy().starts_with('.')
            && shader_key(&e.path()).as_ref() == Some(&key)
    });
    match installed {
        Some(e) => Err(trf(
            "A shader pack with the ID {id} is already installed ({file}). Remove it first to add this one.",
            &[
                ("id", key.strip_prefix("sunrise:").unwrap_or(&key)),
                ("file", &e.file_name().to_string_lossy()),
            ],
        )),
        None => Ok(()),
    }
}

fn migrate_shader_values(home: &Path, pack: &Path, key: &str) -> std::io::Result<()> {
    let Some(id) = key.strip_prefix("sunrise:") else {
        return Ok(());
    };
    let dir = home.join(Kind::Shader.folder());
    let target = dir.join(".sunrise-options").join(format!("{id}.txt"));
    if target.exists() {
        return Ok(());
    }
    let previous = picked_shader(home).filter(|picked| {
        !picked.starts_with("sunrise:")
            && (shader_key(&dir.join(picked)).as_deref() == Some(key)
                || (!dir.join(picked).exists()
                    && picked
                        .strip_prefix(&format!("{id}-"))
                        .is_some_and(|version| {
                            version.len() > ".sshader".len() && version.ends_with(".sshader")
                        })))
    });
    // Changing the selection would discard the old filename needed to import its sidecar.
    for source in previous
        .map(|name| dir.join(name))
        .into_iter()
        .chain(std::iter::once(pack.to_path_buf()))
    {
        let mut sidecar = source.into_os_string();
        sidecar.push(".txt");
        let mut source = match fs::File::open(sidecar) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        fs::create_dir_all(
            target
                .parent()
                .expect("the target is a file in .sunrise-options/"),
        )?;
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e) => return Err(e),
        };
        std::io::copy(&mut source, &mut file)?;
        return Ok(());
    }
    Ok(())
}

/// What [`list`] depends on, read without opening a pack: the two saved choices and each pack
/// folder's entries with their sizes and times. A list is read again when this changes.
pub fn stamp(home: &Path) -> String {
    let saved = |file: &str, key: &str| {
        line_value(
            &fs::read_to_string(home.join(file)).unwrap_or_default(),
            key,
        )
    };
    let mut out = format!(
        "{:?}\n{:?}\n",
        saved(OPTIONS, "resourcePacks"),
        saved(GRAPHICS, "pickedShaderPack")
    );
    for kind in [Kind::Resource, Kind::Shader] {
        let mut entries: Vec<String> = fs::read_dir(home.join(kind.folder()))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| {
                let meta = e.metadata().ok();
                let time = meta.as_ref().and_then(|m| m.modified().ok());
                format!("{:?} {:?} {time:?}", e.file_name(), meta.map(|m| m.len()))
            })
            .collect();
        entries.sort();
        out += &entries.join("\n");
        out.push('\n');
    }
    out
}

/// Which packs' pictures a listing reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pictures {
    All,
    /// Only list metadata. Home asks for artwork separately, for the rows near the view.
    None,
}

/// The packs of `kind` in the game folder `home`, by name: turning one on or off leaves it where
/// it is.
pub fn list(home: &Path, kind: Kind) -> Vec<Pack> {
    list_with(home, kind, Pictures::All)
}

/// [`list`], reading the pictures of `pictures` only.
pub fn list_with(home: &Path, kind: Kind, pictures: Pictures) -> Vec<Pack> {
    let on = enabled(home, kind);
    let mut packs: Vec<Pack> = fs::read_dir(home.join(kind.folder()))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let file = path.file_name()?.to_str()?.to_string();
            read_named(&path, file, kind, &on, pictures)
        })
        .collect();
    packs.sort_by(|a, b| a.file.cmp(&b.file));
    let mut keys = std::collections::HashSet::new();
    packs.retain(|p| {
        kind != Kind::Shader || keys.insert(shader_key(&p.path).unwrap_or_else(|| p.file.clone()))
    });
    packs.sort_by_key(|p| p.title.to_lowercase());
    packs
}

/// The shader pack `sunrise-graphics.txt` picks: a Sunrise pack's `sunrise:<id>`, or the name of
/// one file or folder in `shaderpacks/`. Anything else (a path, `..`, nothing) picks no pack, so
/// what is read from that file never leads out of the pack folder.
fn picked_shader(home: &Path) -> Option<String> {
    let picked = line_value(
        &fs::read_to_string(home.join(GRAPHICS)).unwrap_or_default(),
        "pickedShaderPack",
    )?;
    let one_name = || {
        let mut parts = Path::new(&picked).components();
        matches!(
            (parts.next(), parts.next()),
            (Some(std::path::Component::Normal(name)), None) if name == picked.as_str()
        )
    };
    let id = picked.strip_prefix("sunrise:");
    match id {
        Some(id) => !id.is_empty() && !id.contains(['/', '\\']),
        None => one_name() && !picked.contains('\\'),
    }
    .then_some(picked)
}

/// The file names turned on, bottom first.
fn enabled(home: &Path, kind: Kind) -> Vec<String> {
    match kind {
        Kind::Resource => option_list(&fs::read_to_string(home.join(OPTIONS)).unwrap_or_default())
            .into_iter()
            .filter_map(|id| id.strip_prefix("file/").map(str::to_string))
            .collect(),
        Kind::Shader => {
            let picked = picked_shader(home);
            let dir = home.join(Kind::Shader.folder());
            picked
                .map(|p| {
                    shader_key(&dir.join(&p))
                        .or_else(|| {
                            if dir.join(&p).exists() {
                                return None;
                            }
                            let stem = p.strip_suffix(".sshader")?;
                            let mut paths: Vec<_> = fs::read_dir(&dir)
                                .ok()?
                                .flatten()
                                .map(|e| e.path())
                                .collect();
                            paths.sort();
                            paths
                                .iter()
                                .filter_map(|path| shader_key(path))
                                .find(|key| {
                                    stem.strip_prefix(
                                        key.strip_prefix("sunrise:")
                                            .expect("shader_key always says sunrise:<id>"),
                                    )
                                    .is_some_and(|v| v.starts_with('-') && v.len() > 1)
                                })
                        })
                        .unwrap_or(p)
                })
                .into_iter()
                .collect()
        }
    }
}

/// A pack at `path` that is shown under the name `file` (a copy not yet put in place has
/// another).
fn read_named(
    path: &Path,
    file: String,
    kind: Kind,
    on: &[String],
    pictures: Pictures,
) -> Option<Pack> {
    if file.starts_with('.') {
        return None;
    }
    let ext =
        |x: &str| path.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case(x));
    let enabled = on.contains(&file)
        || (kind == Kind::Shader && shader_key(path).is_some_and(|k| on.contains(&k)));
    // A picture nobody will see is not read.
    let pictured = pictures == Pictures::All;
    let (title, description, icon) = match kind {
        Kind::Resource => {
            // The game reads a folder, or a file whose name ends in `.zip`, exactly so.
            if !(path.is_dir() || file.ends_with(".zip")) {
                return None;
            }
            let [meta, icon] = entries(path, [Some("pack.mcmeta"), pictured.then_some("pack.png")]);
            let description = pack_description(&pack_meta(&meta?)?)?;
            (file.clone(), description, icon)
        }
        Kind::Shader if path.join(SHADER_MANIFEST).is_file() || ext(SHADER_PACKAGE) => {
            // A Sunrise shader pack's picture is its `icon.png`.
            let [manifest, icon] = entries(
                path,
                [Some(SHADER_MANIFEST), pictured.then_some("icon.png")],
            );
            let name = manifest
                .and_then(|t| toml::from_slice::<toml::Table>(&t).ok())
                .and_then(|t| t.get("name")?.as_str().map(str::to_string));
            (
                name.unwrap_or_else(|| file.clone()),
                tr("Sunrise shader pack").into(),
                icon,
            )
        }
        Kind::Shader if ext("zip") || path.join("shaders").is_dir() => {
            (file.clone(), "shaders/ format".into(), None)
        }
        Kind::Shader => return None,
    };
    Some(Pack {
        kind,
        enabled,
        path: path.to_path_buf(),
        file,
        title,
        description,
        icon,
    })
}

/// `path` opened to read, when it is a regular file: a pipe or a device under a pack's name is
/// never opened, so listing packs can't wait on one.
fn regular(path: &Path) -> Option<fs::File> {
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

/// The files called `names` at the root of a pack folder or zip (a name not asked for is not
/// read), each at most 4 MB, with the pack opened once. In a folder only a regular file that is itself in the folder is read: a
/// link is not followed out of the pack.
fn entries<const N: usize>(path: &Path, names: [Option<&str>; N]) -> [Option<Vec<u8>>; N] {
    const MAX: u64 = 4 << 20;
    let read = |from: &mut dyn Read| {
        let mut out = Vec::new();
        from.take(MAX).read_to_end(&mut out).ok()?;
        Some(out)
    };
    if path.is_dir() {
        return names.map(|name| {
            let file = path.join(name?);
            if !fs::symlink_metadata(&file).ok()?.is_file() {
                return None;
            }
            read(&mut regular(&file)?)
        });
    }
    let mut zip = regular(path).and_then(|file| crate::zip_intake::open(file).ok());
    names.map(|name| read(&mut zip.as_mut()?.by_name(name?).ok()?))
}

/// A file at the root of a pack folder or zip, at most 4 MB.
pub(crate) fn entry(path: &Path, name: &str) -> Option<Vec<u8>> {
    let [file] = entries(path, [Some(name)]);
    file
}

/// Read the image at the root of a pack, independently of listing its metadata.
pub fn artwork(path: &Path, kind: Kind) -> Option<Vec<u8>> {
    let name = match kind {
        Kind::Resource => "pack.png",
        Kind::Shader if sunrise_launcher_shader_api::package::is_pack(path) => "icon.png",
        Kind::Shader => return None,
    };
    entry(path, name)
}

/// A `pack.mcmeta` read the way the game's JSON reader does: a leading byte order mark is
/// skipped and `//` and `/* */` comments are ignored. Anything unreadable gives `None`.
pub(crate) fn pack_meta(bytes: &[u8]) -> Option<serde_json::Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_string = !in_string;
                clean.push(c);
            }
            '\\' if in_string => {
                clean.push(c);
                clean.extend(chars.next());
            }
            '/' if !in_string && chars.peek() == Some(&'/') => {
                while chars.next_if(|&n| n != '\n').is_some() {}
            }
            '/' if !in_string && chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for n in chars.by_ref() {
                    if last == '*' && n == '/' {
                        break;
                    }
                    last = n;
                }
                clean.push(' ');
            }
            c => clean.push(c),
        }
    }
    serde_json::from_str(&clean).ok()
}

/// The description of a parsed `pack.mcmeta` as plain text. The game refuses a pack that gives
/// none, so a missing one gives `None`.
pub(crate) fn pack_description(meta: &serde_json::Value) -> Option<String> {
    let description = &meta["pack"]["description"];
    (!description.is_null()).then(|| plain(description))
}

/// A text component as plain text: its strings, without `§` formatting codes.
pub(crate) fn plain(component: &serde_json::Value) -> String {
    fn walk(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::String(s) => out.push_str(s),
            serde_json::Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            serde_json::Value::Object(o) => {
                if let Some(t) = o.get("text").or_else(|| o.get("translate")) {
                    walk(t, out);
                }
                if let Some(extra) = o.get("extra") {
                    walk(extra, out);
                }
            }
            _ => {}
        }
    }
    let mut raw = String::new();
    walk(component, &mut raw);
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '§' => {
                chars.next();
            }
            '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// `key:value` in a `key:value` per line file.
fn line_value(text: &str, key: &str) -> Option<String> {
    // The game keeps the last line of a repeated key.
    text.lines()
        .filter_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .next_back()
        .map(str::to_string)
}

/// `resourcePacks:["vanilla","file/x.zip"]` in options.txt.
fn option_list(options: &str) -> Vec<String> {
    line_value(options, "resourcePacks")
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// The file's text; a file that is not there is empty, and any other failure to read it is an
/// error: an options file that could not be read is never written over as if it were empty.
fn read_settings(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Puts `text` in place of the file whole: a temporary file in the same folder, then a rename.
fn write_settings(path: &Path, text: &str) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".sunrise-options-")
        .tempfile_in(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    std::io::Write::write_all(&mut temp, text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    temp.persist(path)
        .map_err(|e| format!("{}: {}", path.display(), e.error))?;
    Ok(())
}

/// Sets `key:value` in the file, or adds the line; every other line stays.
fn set_line(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let text = read_settings(path)?;
    write_settings(path, &with_line(&text, key, value))
}

/// `text` with `key:value` set, or added as a line; every other line stays.
fn with_line(text: &str, key: &str, value: &str) -> String {
    let is_key = |l: &str| l.strip_prefix(key).is_some_and(|r| r.starts_with(':'));
    // The game keeps the last line of a repeated key, so that is the line to set.
    let last = text
        .lines()
        .rev()
        .position(is_key)
        .map(|n| text.lines().count() - 1 - n);
    let mut out: Vec<String> = text
        .lines()
        .enumerate()
        .map(|(i, l)| match last {
            Some(n) if n == i => format!("{key}:{value}"),
            _ => l.to_string(),
        })
        .collect();
    if last.is_none() {
        out.push(format!("{key}:{value}"));
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Turns a pack on or off. A resource pack turned on goes on top; a shader pack turned on
/// replaces the one that was.
pub fn set_enabled(home: &Path, pack: &Pack, on: bool) -> Result<(), String> {
    match pack.kind {
        Kind::Resource => {
            let options = home.join(OPTIONS);
            let text = fs::read_to_string(&options).unwrap_or_default();
            let id = format!("file/{}", pack.file);
            let mut list = option_list(&text);
            list.retain(|i| *i != id);
            if list.is_empty() {
                list.push("vanilla".into());
            }
            if on {
                list.push(id.clone());
            } else {
                // The game's "use it anyway" answer goes with it.
                let mut incompatible: Vec<String> = line_value(&text, "incompatibleResourcePacks")
                    .and_then(|v| serde_json::from_str(&v).ok())
                    .unwrap_or_default();
                if incompatible.contains(&id) {
                    incompatible.retain(|i| *i != id);
                    let value = serde_json::to_string(&incompatible)
                        .expect("a list of names always serialises");
                    set_line(&options, "incompatibleResourcePacks", &value)?;
                }
            }
            set_line(
                &options,
                "resourcePacks",
                &serde_json::to_string(&list).expect("a list of names always serialises"),
            )
        }
        Kind::Shader => {
            if let Some(previous) = picked_shader(home).filter(|p| !p.starts_with("sunrise:")) {
                if let Some(key) = enabled(home, Kind::Shader).first() {
                    let path = home.join(Kind::Shader.folder()).join(previous);
                    migrate_shader_values(home, &path, key).map_err(|e| e.to_string())?;
                }
            }
            let key = shader_key(&pack.path).unwrap_or_else(|| pack.file.clone());
            migrate_shader_values(home, &pack.path, &key).map_err(|e| e.to_string())?;
            let value = if on { key.as_str() } else { "" };
            set_line(&home.join(GRAPHICS), "pickedShaderPack", value)
        }
    }
}

/// The pack a command names: its file name, or its title (any case).
pub fn find(home: &Path, kind: Kind, query: &str) -> Result<Pack, String> {
    let all = list_with(home, kind, Pictures::None);
    all.iter()
        .find(|p| p.file == query)
        .or_else(|| all.iter().find(|p| p.title.eq_ignore_ascii_case(query)))
        .cloned()
        .ok_or_else(|| {
            trf(
                "No pack {query} in {folder}",
                &[
                    ("query", query),
                    ("folder", &home.join(kind.folder()).display().to_string()),
                ],
            )
        })
}

/// What a dropped or picked file is: a resource pack (a zip with `pack.mcmeta`), a Sunrise
/// shader pack (`.sshader`), or a shader pack in the `shaders/` format (a zip with `shaders/`).
pub fn kind_of(file: &Path) -> Result<Kind, String> {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    match ext.as_deref() {
        Some(SHADER_PACKAGE | "zip") => {}
        _ => {
            return Err(trf(
                "{name} is not a pack: packs are .zip or .sshader files.",
                &[("name", &name)],
            ))
        }
    }
    let archive = fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let zip = crate::zip_intake::open(archive).map_err(|e| format!("{}: {e}", file.display()))?;
    if ext.as_deref() == Some(SHADER_PACKAGE) {
        Ok(Kind::Shader)
    } else if zip.index_for_name("pack.mcmeta").is_some() {
        // The game loads a resource pack file only when its name ends in `.zip`.
        if !name.ends_with(".zip") {
            return Err(trf(
                "{name} must be named with a lowercase .zip ending for the game to load it",
                &[("name", &name)],
            ));
        }
        Ok(Kind::Resource)
    } else if zip
        .file_names()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            trf(
                "{name} has an entry whose name can't be read: {error}",
                &[("name", &name), ("error", &e.to_string())],
            )
        })?
        .iter()
        .any(|n| n.starts_with("shaders/"))
    {
        Ok(Kind::Shader)
    } else {
        Err(trf(
            "{name} has neither a pack.mcmeta (a resource pack) nor a shaders folder (a shader pack).",
            &[("name", &name)],
        ))
    }
}

/// `name` in `dir`, or `stem (2).ext`, `stem (3).ext`, ... : the first path not taken.
fn free_target(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let stem = Path::new(name)
        .file_stem()
        .unwrap_or(name)
        .to_string_lossy();
    let ext = Path::new(name)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| match n {
            1 => dir.join(name),
            n => dir.join(format!("{stem} ({n}){ext}")),
        })
        .find(|p| !p.exists())
        .expect("a folder can't hold a pack under every numbered name")
}

/// Copies a pack file into the right folder of `home`, under a name not taken yet. Returns it,
/// turned off.
pub fn add(home: &Path, file: &Path) -> Result<Pack, String> {
    let kind = kind_of(file)?;
    let dir = home.join(kind.folder());
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = file
        .file_name()
        .ok_or_else(|| tr("A pack file has a name").to_string())?;
    if kind == Kind::Shader {
        refuse_installed_shader(&dir, file)?;
    }
    let target = free_target(&dir, name);
    fs::copy(file, &target).map_err(|e| {
        trf(
            "Could not copy {file}: {error}",
            &[
                ("file", &file.display().to_string()),
                ("error", &e.to_string()),
            ],
        )
    })?;
    let placed = target
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);
    placed
        .and_then(|file| read_named(&target, file, kind, &[], Pictures::All))
        .ok_or_else(|| {
            trf(
                "{file} is not a pack the game reads",
                &[("file", &target.display().to_string())],
            )
        })
}

/// The Installed page's add: like `add`, for a file the person said is a `kind` pack. A file of
/// another kind, or a `.sshader` that fails the shader package's own checks, is refused before
/// anything is written. The copy goes through a hidden temporary file in the folder and is put
/// in place only whole, never over a pack already there (a same name gets `(2)`), so an error
/// leaves the folder as it was.
pub fn add_local(home: &Path, kind: Kind, file: &Path) -> Result<Pack, String> {
    let name = file
        .file_name()
        .ok_or_else(|| tr("A pack file has a name").to_string())?;
    if !fs::metadata(file).is_ok_and(|m| m.is_file()) {
        return Err(trf(
            "{file} is not a file.",
            &[("file", &file.display().to_string())],
        ));
    }
    let dir = home.join(kind.folder());
    let made = !dir.exists();
    let put = || -> Result<Pack, String> {
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let ext = Path::new(name)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let mut temp = tempfile::Builder::new()
            .prefix(".sunrise-add-")
            .suffix(&ext)
            .tempfile_in(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut source = fs::File::open(file).map_err(|e| {
            trf(
                "Could not read {file}: {error}",
                &[
                    ("file", &file.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        })?;
        std::io::copy(&mut source, temp.as_file_mut()).map_err(|e| {
            trf(
                "Could not copy {file}: {error}",
                &[
                    ("file", &file.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        })?;
        // What is checked is the copy that gets put in place.
        let found = kind_of(temp.path()).map_err(|e| {
            let shown = name.to_string_lossy();
            let temp_name = temp.path().file_name().map(|n| n.to_string_lossy());
            let e = e.replace(&temp.path().display().to_string(), &shown);
            match temp_name {
                Some(t) => e.replace(&*t, &shown),
                None => e,
            }
        })?;
        if found != kind {
            return Err(trf(
                "{file} is a {found} pack, not a {kind} pack.",
                &[
                    ("file", &name.to_string_lossy()),
                    ("found", found.folder()),
                    ("kind", kind.folder()),
                ],
            ));
        }
        if ext.eq_ignore_ascii_case(".sshader") {
            refuse_installed_shader(&dir, temp.path())?;
            let pack = sunrise_launcher_shader_api::Pack::read(temp.path())?;
            if let Err(problems) = sunrise_launcher_shader_api::check(&pack) {
                let lines: Vec<String> = problems.iter().map(ToString::to_string).collect();
                return Err(trf(
                    "{file} fails the shader pack check:\n{problems}",
                    &[
                        ("file", &name.to_string_lossy()),
                        ("problems", &lines.join("\n")),
                    ],
                ));
            }
        }
        if kind == Kind::Resource {
            // A pack with data but no assets is a data pack; one with both stays a resource pack.
            let zip = crate::zip_intake::open(temp.reopen().map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let names: Vec<_> = zip.file_names().flatten().collect();
            if names.iter().any(|n| n.starts_with("data/"))
                && !names.iter().any(|n| n.starts_with("assets/"))
            {
                return Err(trf(
                    "{file} is a data pack (data/ and no assets/): add it as a data pack.",
                    &[("file", &name.to_string_lossy())],
                ));
            }
        }
        // What the game reads, under the name it will have, before anything is published.
        let target = free_target(&dir, name);
        let shown = target
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string);
        let mut pack = shown
            .and_then(|shown| read_named(temp.path(), shown, kind, &[], Pictures::All))
            .ok_or_else(|| {
                trf(
                    "{file} is not a pack the game reads",
                    &[("file", &name.to_string_lossy())],
                )
            })?;
        temp.persist_noclobber(&target).map_err(|e| {
            trf(
                "Could not add {file}: {error}",
                &[
                    ("file", &target.display().to_string()),
                    ("error", &e.error.to_string()),
                ],
            )
        })?;
        pack.path = target;
        Ok(pack)
    };
    let pack = put();
    if pack.is_err() && made {
        let _ = fs::remove_dir(&dir);
    }
    pack
}
/// The enabled resource packs the folder shows, as `resourcePacks` holds them: each one's index
/// in the raw list (bottom first) and its file name. Builtins and references to packs that are
/// not there are left out.
fn visible_enabled(home: &Path, options: &str) -> (Vec<String>, Vec<(usize, String)>) {
    let raw = option_list(options);
    let shown: Vec<String> = list(home, Kind::Resource)
        .into_iter()
        .map(|p| p.file)
        .collect();
    let slots = raw
        .iter()
        .enumerate()
        .filter_map(|(i, id)| Some((i, id.strip_prefix("file/")?.to_string())))
        .filter(|(_, name)| shown.contains(name))
        .collect();
    (raw, slots)
}

/// The file names in `resourcePacks`, highest priority (the game's top) first. Packs that are
/// not there are included: a page ranks the ones it shows.
pub fn resource_order(home: &Path) -> Vec<String> {
    enabled(home, Kind::Resource).into_iter().rev().collect()
}

/// Swaps `file` with `neighbor`, two enabled resource packs next to each other among the
/// ones the folder shows (`resource_order` without the missing ones). Everything else in `resourcePacks` stays where it is, builtins and
/// references to missing packs included. A pair that is not enabled, not adjacent (the list
/// changed since the page read it), or a pack listed twice is refused and nothing is written.
pub fn move_resource(home: &Path, file: &str, neighbor: &str) -> Result<(), String> {
    let options = home.join(OPTIONS);
    let text = read_settings(&options)?;
    let (mut raw, slots) = visible_enabled(home, &text);
    let at = |name: &str| {
        let mut found = slots.iter().filter(|(_, n)| n == name).map(|(i, _)| *i);
        (found.next(), found.next().is_some())
    };
    let stale = || {
        trf(
            "{file} and {neighbor} are not next to each other any more.",
            &[("file", file), ("neighbor", neighbor)],
        )
    };
    let ((Some(a), false), (Some(b), false)) = (at(file), at(neighbor)) else {
        return Err(stale());
    };
    let rank = |i: usize| {
        slots
            .iter()
            .position(|(s, _)| *s == i)
            .expect("both packs were just found among the slots")
    };
    if rank(a).abs_diff(rank(b)) != 1 {
        return Err(stale());
    }
    raw.swap(a, b);
    let text = with_line(
        &text,
        "resourcePacks",
        &serde_json::to_string(&raw).expect("a list of names always serialises"),
    );
    write_settings(&options, &text)
}

/// Turns the pack off and moves it to the system's trash.
pub fn remove(home: &Path, pack: &Pack) -> Result<(), String> {
    if pack.enabled {
        set_enabled(home, pack, false)?;
    }
    crate::desktop_seams::to_trash(&pack.path).map_err(|e| {
        trf(
            "Could not move {file} to the trash: {error}",
            &[("file", &pack.file), ("error", &e.to_string())],
        )
    })
}

#[cfg(test)]
mod tests;
