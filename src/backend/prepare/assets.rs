//! The game's own files: assets, the vanilla jars downloaded for the account that owns the game,
//! the server jar and the sounds.
use super::{
    resources_at, resources_owner, server_profile, set_prepare_step, PrepareStep,
    RESOURCES_MANIFEST,
};
use crate::backend::{data_dir, get, prepare_lock, read_json, version_dir, versions_dir, Settings};
use crate::managed_play::Resources;
use crate::{account, mojang, sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Adds the extras a prepared version lacks (packs, panorama, fonts, languages, sounds), through its own
/// `sunrise-prepare extras`: a new version reuses the resources of the one before (`reuse_prepared`), which may
/// come from a preparer that left them out. Without them
/// in any Minecraft folder, one line each in the log, once per launcher run, and the version plays
/// as before. Nothing is prepared again.
pub fn complete_assets(tag: &str) {
    let Ok(preparation) = prepare_lock(tag) else {
        return;
    };
    complete_assets_staged(tag, &preparation.handoff(None));
}

pub(crate) fn complete_assets_staged(tag: &str, handoff: &crate::preparer::Handoff<'_>) {
    complete_assets_in(
        &versions_dir(),
        tag,
        minecraft_folders,
        &|line| eprintln!("{line}"),
        Some(handoff),
    );
}

/// Whether Sunrise's own store is asked to download again: it has an asset index, the version
/// lacks some extra (any kind: the download fetches the whole asset index, which holds the
/// sounds, languages, fonts, panorama and built-in packs) and it was not asked this run yet.
pub(in crate::backend) fn refill_wanted(
    has_index: bool,
    missing: Option<&crate::preparer::Extras>,
    asked: &std::sync::atomic::AtomicBool,
) -> bool {
    has_index
        && missing.is_some_and(|m| !m.is_empty())
        && !asked.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// Sunrise's own download (an owner's) may have stopped before the sounds and the languages,
/// and the copies of `complete_assets` read them from its store: asked again, once per run, through
/// the same owner gate as any download from Mojang (`download_for_owner`), which keeps the files
/// already whole. Called where a download is expected (`reuse_prepared_everywhere`), never on Play.
pub(in crate::backend) fn refill_own_store(tag: &str, handoff: &crate::preparer::Handoff<'_>) {
    static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let own = data_dir().join("minecraft");
    let root = versions_dir().join(tag);
    let resources = resources_at(&root);
    if read_json(&resources.join(RESOURCES_MANIFEST)).is_none_or(|m| server_profile(&m)) {
        return;
    }
    let missing = crate::preparer::binary(&root).and_then(|binary| {
        crate::preparer::missing_extras(&binary, &root, &resources, Some(handoff))
            .ok()
            .flatten()
    });
    if !refill_wanted(
        own.join("assets/indexes").is_dir(),
        missing.as_ref(),
        &ASKED,
    ) {
        return;
    }
    let Some(pins) = read_json(&version_dir(tag).join("research-lock.json"))
        .and_then(|l| sources::Pins::read(&l))
    else {
        return;
    };
    let fetch = |url: &str| -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        get(url)?
            .into_reader()
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
        Ok(body)
    };
    if let Err(e) = download_for_owner(&pins, &own, &fetch, None, &|line| eprintln!("{line}")) {
        eprintln!("{tag}: the missing assets were not downloaded: {e}");
    }
}

/// `folders`: where to look, asked only when the version lacks something.
pub(in crate::backend) fn complete_assets_in(
    versions: &Path,
    tag: &str,
    folders: impl Fn() -> Vec<PathBuf>,
    log: &dyn Fn(String),
    handoff: Option<&crate::preparer::Handoff<'_>>,
) {
    let stopped = || {
        handoff
            .and_then(crate::preparer::Handoff::stop)
            .is_some_and(crate::native_mods::Cancellation::is_stopped)
    };
    if stopped() {
        return;
    }
    // The versions tried this run. Held while adding: the start, an install and Play can complete one version at once.
    static TRIED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let root = versions.join(tag);
    let resources = resources_at(&root);
    if read_json(&resources.join(RESOURCES_MANIFEST)).is_none_or(|m| server_profile(&m)) {
        return;
    }
    // A version whose preparer has no `missing-extras` (from before it) keeps what it has.
    let Some(binary) = crate::preparer::binary(&root) else {
        return;
    };
    let lacking = match crate::preparer::missing_extras(&binary, &root, &resources, handoff) {
        Ok(Some(lacking)) if !lacking.is_empty() => lacking,
        Ok(_) => return,
        Err(e) => return log(format!("{tag}: {e}")),
    };
    if stopped() {
        return;
    }
    let mut tried = TRIED.lock().unwrap_or_else(|e| e.into_inner());
    if tried.contains(&root) {
        return;
    }
    if let Err(error) = Resources::invalidate(&root) {
        return log(format!("{tag}: {error}"));
    }
    let kinds: Vec<String> = lacking.iter().map(|(kind, _)| kind.clone()).collect();
    let result =
        crate::preparer::add_extras(&binary, &root, &resources, &kinds, &folders(), log, handoff);
    if !stopped() {
        tried.push(root.clone());
    }
    match result {
        Ok(Some(added)) => {
            for (kind, error) in added.failed {
                let what = lacking
                    .iter()
                    .find(|(k, _)| *k == kind)
                    .map_or(kind.as_str(), |(_, what)| what.as_str());
                log(crate::i18n::trf(
                    "{version}: {what} not added: {error}",
                    &[("version", tag), ("what", what), ("error", &error)],
                ));
            }
        }
        Ok(None) => {}
        Err(e) => log(format!("{tag}: {e}")),
    }
}

/// The Minecraft folders whose asset store can hold what a prepared version lacks: the one
/// chosen in the launcher, the official launcher's (or SUNRISE_MINECRAFT_DIR), the folder
/// other launchers' files are linked into and Sunrise's own download, then every launcher's
/// default folder.
fn minecraft_folders() -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = Settings::load().minecraft_dir.into_iter().collect();
    folders.extend([
        crate::preparer::minecraft_dir(None),
        data_dir().join("linked-minecraft"),
        data_dir().join("minecraft"),
    ]);
    folders.extend(
        sources::Folders::current()
            .map(|f| sources::known_folders(sources::Os::current(), &f))
            .unwrap_or_default()
            .into_iter()
            .map(|(_, dir)| dir),
    );
    let mut found: Vec<PathBuf> = Vec::new();
    for folder in folders {
        if folder.is_dir() && !found.contains(&folder) {
            found.push(folder);
        }
    }
    found
}

/// The official launcher's game directory, else the first other launcher's found
/// (sources.rs); preparing looks in all of them anyway.
pub fn default_minecraft_dir() -> Option<PathBuf> {
    let folders = sources::Folders::current()?;
    sources::known_folders(sources::Os::current(), &folders)
        .into_iter()
        .map(|(_, dir)| dir)
        .find(|dir| dir.is_dir())
}

/// Downloads the pinned version from Mojang into `dir` when an account in the launcher owns
/// Minecraft (asked again now), else says what signing in would do, as the last line of the
/// not-found error.
pub(in crate::backend) fn download_for_owner(
    pins: &sources::Pins,
    dir: &Path,
    fetch: sources::Fetch,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    log: &dyn Fn(String),
) -> Result<(), String> {
    let mut accounts = account::Accounts::load();
    // Any account that owns the game may download it (`launch`), not only the one that plays.
    let pass = account::launch(
        &account::Endpoints::live(),
        account::client_id().as_deref(),
        account::store(),
        &mut accounts,
    );
    if let Err(e) = accounts.save_renewed() {
        log(crate::i18n::trf(
            "Sunrise: the accounts are not saved: {error}",
            &[("error", &e.to_string())],
        ));
    }
    download_for(
        pins,
        dir,
        pass,
        !accounts.accounts.is_empty(),
        fetch,
        cancel.unwrap_or(&mojang::CANCEL),
        log,
    )
}

/// Downloads Minecraft `id` (any version on Mojang's list, its version file's sha1 from it) into
/// Sunrise's own game folder, in the official launcher's layout, for an account that owns the
/// game (asked again now). Returns that folder.
pub fn download_version_for_owner(
    id: &str,
    manifest_sha1: &str,
    log: &dyn Fn(String),
) -> Result<PathBuf, String> {
    let dir = data_dir().join("minecraft");
    let fetch = |url: &str| -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        get(url)?
            .into_reader()
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
        Ok(body)
    };
    let pins = sources::Pins::for_version(id, manifest_sha1);
    download_for_owner(&pins, &dir, &fetch, None, log)
        .map_err(|e| e.trim_start_matches("\n  • ").to_string())?;
    Ok(dir)
}

/// What a player with no account signed in can do: a sentence that stands alone, and reads as one
/// more bullet after a list of places Sunrise looked.
fn sign_in_to_download(version: &str) -> String {
    crate::i18n::trf(
        "Sign in with the Microsoft account that owns Minecraft: Sunrise then downloads {version} from Mojang itself.",
        &[("version", version)],
    )
}

/// What [`download_for_owner`] does with the owner gate's answer: Mojang's files only for its
/// `owner`, an account the services said owns the game just now.
pub(in crate::backend) fn download_for(
    pins: &sources::Pins,
    dir: &Path,
    pass: Result<account::Launch, account::NoOwner>,
    signed_in: bool,
    fetch: sources::Fetch,
    cancel: &std::sync::atomic::AtomicBool,
    log: &dyn Fn(String),
) -> Result<(), String> {
    let v = &pins.version;
    match pass {
        Ok(account::Launch {
            owner: Some((owner, _)),
            ..
        }) => {
            let name = owner.name;
            log(crate::i18n::trf(
                "{name} owns Minecraft: downloading {version} from Mojang",
                &[("name", &name), ("version", &v.to_string())],
            ));
            let os = match sources::Os::current() {
                sources::Os::Linux => "linux",
                sources::Os::MacOs => "osx",
                sources::Os::Windows => "windows",
            };
            mojang::download(pins, dir, os, 8, fetch, cancel, log).map_err(|e| {
                format!("\n  • Sunrise tried to download {v} from Mojang for {name}, but: {e}")
            })
        }
        // An owner by its last answer, who can't be asked now.
        Ok(pass) => Err(format!(
            "\n  • Downloading {v} from Mojang needs your Microsoft account, which could not be \
             checked: {}",
            pass.offline.unwrap_or_default()
        )),
        Err(_) if signed_in => Err(
            "\n  • No signed-in account owns Minecraft: Java Edition, so Sunrise can't download it."
                .into(),
        ),
        Err(_) => Err(format!("\n  • {}", sign_in_to_download(v))),
    }
}

/// Makes sure `jar` holds the server jar `manifest` names: one already cached whose sha1 is not
/// the manifest's (a cut-off or captive-portal download) is fetched again, and a download is
/// kept only when it matches. A manifest without a sha1 can't check either.
pub(in crate::backend) fn ensure_server_jar(
    jar: &Path,
    manifest: &serde_json::Value,
    fetch: sources::Fetch,
    log: &dyn Fn(String),
) -> Result<(), String> {
    use crate::i18n::{tr, trf};
    let server = &manifest["downloads"]["server"];
    let sha1 = server["sha1"].as_str();
    let whole =
        |body: &[u8]| sha1.is_none_or(|want| sources::sha1_hex(body).eq_ignore_ascii_case(want));
    if fs::read(jar).is_ok_and(|body| whole(&body)) {
        return Ok(());
    }
    let url = server["url"]
        .as_str()
        .ok_or_else(|| tr("The version file has no server download").to_string())?;
    log(trf(
        "Downloading the {version} server jar from Mojang",
        &[(
            "version",
            &jar.file_stem().unwrap_or_default().to_string_lossy(),
        )],
    ));
    let body = fetch(url)?;
    if !whole(&body) {
        return Err(trf(
            "The server jar from Mojang is not the one its version file names (sha1 {sha1})",
            &[("sha1", &sources::sha1_hex(&body))],
        ));
    }
    sources::write_file(jar, &body)
}

/// Copies the pinned version's sounds from the player's own launcher install into the version's resources with its own `sunrise-prepare sounds`; sound is optional, so a failure is a line in `log`.
pub fn sounds(
    tag: &str,
    minecraft: &Path,
    log: &dyn Fn(String),
    preparation: &crate::preparer::Preparation,
) {
    sounds_staged(tag, minecraft, log, &preparation.handoff(None));
}

pub fn sounds_cancellable(
    tag: &str,
    minecraft: &Path,
    log: &dyn Fn(String),
    preparation: &crate::preparer::Preparation,
    stop: &crate::native_mods::Cancellation,
) {
    if stop.is_stopped() {
        return;
    }
    sounds_staged(
        tag,
        minecraft,
        log,
        &preparation.handoff_cancellable(None, stop),
    );
}

pub(crate) fn sounds_staged(
    tag: &str,
    minecraft: &Path,
    log: &dyn Fn(String),
    handoff: &crate::preparer::Handoff<'_>,
) {
    use crate::i18n::{tr, trf};
    let tag = resources_owner(tag);
    let tag = tag.as_str();
    let dir = version_dir(tag);
    set_prepare_step(PrepareStep::Sounds);
    log(tr("Copying the sounds from your Minecraft install").into());
    let result = match crate::preparer::binary(&dir) {
        Some(binary) => crate::preparer::sounds(&binary, &dir, minecraft, log, Some(handoff))
            .and_then(|done| done.ok_or_else(|| crate::preparer::too_old(tag))),
        None => Err(crate::preparer::too_old(tag)),
    };
    if let Err(why) = result {
        log(trf(
            "No sound (the game will be silent): {why}",
            &[("why", &why)],
        ));
    }
}
