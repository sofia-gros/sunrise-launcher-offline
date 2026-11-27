//! Play admission (game-install journals, the browser lock, registry takedowns) and the
//! registry's GitHub links.
use super::*;
use sunrise_launcher_smod::index;
use sunrise_launcher_smod::index_v2::{Index, Revoked};

/// The sample mod package, as an installed registry mod.
const HUD: &str = "crates/smod/tests/index/coords_hud-0.1.0.smod";

/// A signed index listing nothing.
fn sample() -> Index {
    Index {
        schema: index_v2::SCHEMA,
        serial: 1,
        generated: "2026-11-24T00:00:00Z".into(),
        builds: Vec::new(),
        categories: Vec::new(),
        projects: Vec::new(),
        revoked: Vec::new(),
        collections: Vec::new(),
    }
}

/// Seeds `coords_hud` as an installed registry mod in `home` for play-check and recovery tests.
fn seed_installed_hud(home: &Path) -> installed::Entry {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(HUD);
    let dest = home.join("sunrise-mods/coords_hud.smod");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::copy(&source, &dest).unwrap();
    let entry = installed::Entry {
        source: SourceId::Sunrise,
        project: "coords_hud".into(),
        mod_id: None,
        slug: "coords_hud".into(),
        title: "Coordinates HUD".into(),
        kind: crate::browser::Kind::Mod,
        version: "0.1.0".into(),
        number: "0.1.0".into(),
        file: "sunrise-mods/coords_hud.smod".into(),
        checksum: format!("SHA-256:{}", index::sha256_file(&source).unwrap()),
        page: String::new(),
        dependency: false,
        verdict: Some("Verified".into()),
        install_hold: None,
        repository: None,
    };
    let mut list = installed::Installed::load(home);
    list.entries.retain(|e| e.project != entry.project);
    list.entries.push(entry.clone());
    list.save(home).unwrap();
    mod_list::set_enabled(home, "coords_hud", true).unwrap();
    entry
}

fn interrupted_game_install(home: &Path, title: String) -> (installed::Entry, installed::Entry) {
    let old = seed_installed_hud(home);
    let bytes = sunrise_launcher_smod::Package::write_files(
        "id='coords_hud'\nname='Coordinates HUD'\nversion='1.0.0'\napi='0.2'\ncapabilities=['registries']\n",
        &[],
    )
    .unwrap();
    let mut new = old.clone();
    new.title = title;
    new.version = "1.0.0".into();
    new.number = new.version.clone();
    new.checksum = format!("SHA-256:{}", index::sha256(&bytes));
    fs::write(
        home.join(index::browser_state::PENDING),
        serde_json::to_vec(&serde_json::json!({
            "id": "coords_hud", "file": old.file, "old": old, "new": new,
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(home.join(&old.file), bytes).unwrap();
    mod_list::set_enabled(home, "coords_hud", true).unwrap();
    (old, new)
}

#[test]
fn registry_play_recovers_a_large_game_journal_before_checking_revocations() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    let mut registry = sample();
    let (old, new) = interrupted_game_install(home, "an allowed title ".repeat(20_000));
    let pending = home.join(index::browser_state::PENDING);
    assert!(fs::metadata(&pending).unwrap().len() > 256 << 10);
    let file = home.join(&old.file);
    let bytes = fs::read(&file).unwrap();
    registry.revoked.push(Revoked {
        project: "coords_hud".into(),
        sha256: Some(old.checksum.strip_prefix("SHA-256:").unwrap().into()),
        reason: "Old file taken down.".into(),
        date: String::new(),
    });

    let notices = check_before_play(home, &[], || Some(Arc::new(registry))).unwrap();
    assert!(notices.is_empty(), "{notices:?}");
    assert!(mod_list::is_enabled(home, "coords_hud"));
    assert!(!pending.exists());
    let rows: installed::Installed =
        serde_json::from_slice(&fs::read(home.join(installed::MANIFEST)).unwrap()).unwrap();
    assert_eq!(rows.entries, [new]);
    assert_eq!(fs::read(file).unwrap(), bytes);
}

#[test]
fn play_waits_for_a_writer_before_the_journal_is_visible_and_unlocks_before_fetching() {
    use std::{sync::mpsc, thread, time::Duration};
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    let registry = sample();
    let writer = crate::browser::lock_home(home).unwrap();
    assert!(!home.join(index::browser_state::PENDING).exists());
    assert!(!home.join(installed::MANIFEST).exists());
    thread::scope(|scope| {
        let (enter, entering) = mpsc::channel();
        let (done, completion) = mpsc::channel();
        let (released, release) = mpsc::channel();
        let index = Arc::new(registry.clone());
        scope.spawn(move || {
            enter.send(()).unwrap();
            let result = check_before_play(home, &[], || {
                // A blocking lock returns only once Play has let go. The wait for it is bounded
                // so a Play that still held the lock fails here, and then lets go as it unwinds.
                scope.spawn(move || {
                    drop(crate::browser::lock_home(home).unwrap());
                    released.send(()).unwrap();
                });
                release
                    .recv_timeout(Duration::from_secs(30))
                    .expect("Play must not hold the browser lock during a fetch");
                Some(index)
            });
            done.send(result).unwrap();
        });
        entering.recv().unwrap();
        assert_eq!(
            completion.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let (_, new) = interrupted_game_install(home, "Published during Play".into());
        drop(writer);
        assert!(completion.recv().unwrap().unwrap().is_empty());
        assert!(!home.join(index::browser_state::PENDING).exists());
        assert_eq!(installed::Installed::read(home).unwrap().entries, [new]);
    });
}

#[test]
fn play_admission_names_the_game_journal_and_leaves_other_profiles_playable() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let home = first.path();
    let (old, _) = interrupted_game_install(home, "Coordinates HUD".into());
    let pending = home.join(index::browser_state::PENDING);
    let manifest = home.join(installed::MANIFEST);
    let package = home.join(&old.file);
    let package_bytes = fs::read(&package).unwrap();
    let valid_journal = fs::read(&pending).unwrap();
    let valid_manifest = fs::read(&manifest).unwrap();
    let duplicate = serde_json::to_vec(&serde_json::json!({"entries": [old, old]})).unwrap();
    for (journal, rows, expected) in [
        (
            b"invalid json".as_slice(),
            &valid_manifest,
            index::browser_state::PENDING,
        ),
        (&valid_journal, &duplicate, "several entries"),
    ] {
        fs::write(&pending, journal).unwrap();
        fs::write(&manifest, rows).unwrap();
        let detail = installed::recover_game_install(home).unwrap_err().details();
        assert!(
            !detail.contains(&home.display().to_string()),
            "local path leaked: {detail}"
        );
        assert!(
            check_before_play(home, &[], || panic!("no registry call before recovery")).is_err()
        );
        assert!(detail.contains(expected), "{detail}");
        assert_eq!(fs::read(&pending).unwrap(), journal);
        assert_eq!(fs::read(&manifest).unwrap(), *rows);
        assert_eq!(fs::read(&package).unwrap(), package_bytes);
        assert!(check_before_play(second.path(), &[], || None).is_ok());
    }
    fs::write(&manifest, valid_manifest).unwrap();
    assert!(installed::recover_game_install(home).is_ok());
    assert!(!pending.exists());
}

#[test]
fn registry_play_preserves_metadata_edited_after_a_game_journal_was_written() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    let registry = sample();
    let (mut current, _) = interrupted_game_install(home, "Coordinates HUD".into());
    current.dependency = !current.dependency;
    let manifest = home.join(installed::MANIFEST);
    let edited = serde_json::to_vec(&serde_json::json!({ "entries": [current] })).unwrap();
    fs::write(&manifest, &edited).unwrap();

    let error = check_before_play(home, &[], || Some(Arc::new(registry))).unwrap_err();
    assert!(error.contains("changed since the install began"));
    assert!(mod_list::is_enabled(home, "coords_hud"));
    assert_eq!(fs::read(manifest).unwrap(), edited);
    assert!(home.join(index::browser_state::PENDING).exists());
}

#[test]
fn registry_play_refuses_a_taken_down_mod_baked_into_a_build() {
    let root = tempfile::tempdir().unwrap();
    let mut index = sample();
    index.revoked.push(Revoked {
        project: "coords_hud".into(),
        sha256: Some("a".repeat(64)),
        reason: "Test.".into(),
        date: String::new(),
    });
    let built = [crate::native_status::Included {
        id: "coords_hud".into(),
        version: "0.1.0".into(),
        sha256: "a".repeat(64),
    }];
    // Even if the installed-list entry and the enable flag are already gone, the build holds it.
    let error = check_before_play(root.path(), &built, || Some(Arc::new(index))).unwrap_err();
    assert!(
        error.contains("includes a mod the registry took down"),
        "{error}"
    );
}

#[test]
fn owner_links_only_take_github_logins() {
    for ok in ["Stink-O", "ludovic111", "a"] {
        assert!(github_login(ok), "{ok}");
    }
    for bad in [
        "",
        "-x",
        "x-",
        "a--b",
        "bad login",
        "a/b",
        "x?y=1",
        &"a".repeat(40),
    ] {
        assert!(!github_login(bad), "{bad}");
    }
}

#[test]
fn a_project_is_starred_on_the_repository_its_source_is_in() {
    let repo = |s: &str| github_repo(s);
    let daybreak = Some("https://github.com/hildlabs/sunrise-shaders".to_owned());
    // A folder of it, a trailing slash, `.git`, a query or an anchor: the same repository.
    for source in [
        "https://github.com/hildlabs/sunrise-shaders",
        "https://github.com/hildlabs/sunrise-shaders/tree/main/pack",
        "https://github.com/hildlabs/sunrise-shaders/",
        "https://github.com/hildlabs/sunrise-shaders.git",
        "https://github.com/hildlabs/sunrise-shaders?tab=readme",
        "https://github.com/hildlabs/sunrise-shaders#readme",
    ] {
        assert_eq!(repo(source), daybreak, "{source:?}");
    }
    assert_eq!(
        repo("https://github.com/horizon-rust/horizon.v2_mods"),
        Some("https://github.com/horizon-rust/horizon.v2_mods".to_owned())
    );
    // Not GitHub, no repository, or a name GitHub doesn't allow: no star button.
    for source in [
        "https://gitlab.com/hildlabs/sunrise-shaders",
        "http://github.com/hildlabs/sunrise-shaders",
        "https://github.com.evil.example/hildlabs/sunrise-shaders",
        "https://github.com/hildlabs",
        "https://github.com/hildlabs/",
        "https://github.com/-hildlabs/sunrise-shaders",
        "https://github.com/hildlabs/..",
        "https://github.com/hildlabs/sunrise shaders",
        "",
    ] {
        assert_eq!(repo(source), None, "{source:?}");
    }
}
