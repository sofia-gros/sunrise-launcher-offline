//! accounts.json: loading old fields, switching, ordering, a damaged sign-in, and secrets from an earlier launcher.

use super::*;

#[test]
fn offline_names_survive_a_load_and_retired_fields_stay_retired() {
    let dir = std::env::temp_dir().join(format!("sunrise-retired-fields-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("skins")).unwrap();
    let store = MemoryStore::default();
    let mut old = serde_json::to_value(signed_in(&store, &secrets("mc-token", true))).unwrap();
    old["selected"] = serde_json::Value::Null;
    old["offline"] = serde_json::json!(["Builder_1"]);
    old["offline_selected"] = serde_json::json!("Builder_1");
    old["looks"] =
        serde_json::json!({ "builder_1": { "skin": true, "cape": true, "model": "slim" } });
    let path = dir.join("accounts.json");
    std::fs::write(&path, old.to_string()).unwrap();
    let skin_file = dir.join("skins/builder_1.png");
    std::fs::write(&skin_file, b"png").unwrap();
    let mut accounts = Accounts::load_from(&path);
    assert_eq!(accounts.accounts, vec![notch()]);
    assert!(accounts.selected().is_none());
    // This fork keeps offline names: they load, and the one chosen plays.
    assert_eq!(accounts.offline, ["Builder_1"]);
    assert_eq!(accounts.player(), Player::Offline("Builder_1".into()));
    let (_, ep) = mock(|_| {});
    let pass = launch_for_game(&ep, Some("test-client"), &store, &mut accounts, false).unwrap();
    assert_eq!(pass.session.unwrap().0.uuid, UUID);
    accounts.save_to(&path).unwrap();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    // The names are written back; `looks` stays retired (offline names keep no look yet),
    // and no file is deleted either way.
    assert_eq!(
        saved,
        serde_json::json!({
            "accounts": accounts.accounts,
            "selected": UUID,
            "offline": ["Builder_1"],
            "offline_selected": "Builder_1",
        })
    );
    assert!(skin_file.is_file());
    // Retired settings fields are ignored on load and not written back — except
    // `dev_offline`, which this fork's OFFLINE_PLAY switches on at every load.
    let settings = crate::backend::Settings::parse(
        r#"{"dev_offline": true, "welcomed": true, "corners": 12, "keep_versions": 3}"#,
    );
    let saved = serde_json::to_value(&settings).unwrap();
    assert_eq!(
        saved.get("dev_offline"),
        Some(&serde_json::json!(crate::backend::OFFLINE_PLAY)),
        "{saved}"
    );
    assert!(saved.get("welcomed").is_none(), "{saved}");
    assert!(saved.get("corners").is_none(), "{saved}");
    assert_eq!(saved["keep_versions"], 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The account switcher's one click: who plays is on disk at once, and is still who plays
/// after a restart; an account that isn't signed in changes nothing.
#[test]
fn a_switch_is_saved_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("accounts.json");
    let named = |uuid: &str, name: &str| Account {
        uuid: uuid.into(),
        name: name.into(),
        skin: None,
        cape: None,
    };
    Accounts {
        accounts: vec![
            named("a1", "Juniper"),
            named("b2", "Mossbyte"),
            named("c3", "Cinder"),
        ],
        selected: Some("a1".into()),
        ..Accounts::default()
    }
    .save_to(&path)
    .unwrap();

    let kept = Accounts::switch_in(&path, "c3").unwrap();
    assert_eq!(kept.selected().unwrap().name, "Cinder");
    let restarted = Accounts::load_from(&path);
    assert_eq!(restarted, kept);
    assert_eq!(restarted.selected.as_deref(), Some("c3"));
    assert_eq!(restarted.accounts.len(), 3, "switching signs nobody out");

    assert!(Accounts::switch_in(&path, "zz").is_err());
    assert_eq!(
        Accounts::load_from(&path).selected.as_deref(),
        Some("c3"),
        "a refused switch leaves who plays"
    );
    // An account signed in since the window read the list is not lost by a switch.
    let mut grown = Accounts::load_from(&path);
    grown.accounts.push(named("d4", "Fennel"));
    grown.save_to(&path).unwrap();
    let after = Accounts::switch_in(&path, "b2").unwrap();
    assert_eq!(after.accounts.len(), 4);
    assert_eq!(after.selected.as_deref(), Some("b2"));
}

#[test]
fn play_as_picks_a_signed_in_account() {
    let mut a = Accounts {
        accounts: vec![Account {
            uuid: "abc".into(),
            name: "Ludo".into(),
            skin: None,
            cape: None,
        }],
        ..Accounts::default()
    };
    a.play_as("abc").unwrap();
    assert_eq!(a.selected().unwrap().name, "Ludo");
    assert!(a.play_as("zzz").is_err());
    assert_eq!(a.selected.as_deref(), Some("abc"));
    // An accounts.json written now has nothing but the accounts and who plays.
    assert_eq!(
        serde_json::to_string(&Accounts::default()).unwrap(),
        r#"{"accounts":[],"selected":null}"#
    );
}

#[test]
fn a_damaged_saved_sign_in_is_not_overwritten_or_exposed() {
    let store = MemoryStore::default();
    store.set(UUID, "damaged-secret-contents").unwrap();
    let status = session::refresh_with(
        &offline(),
        Some("test-client"),
        &store,
        &notch(),
        &nowhere(),
    );
    assert_eq!(status.state, session::State::SignInRequired);
    assert_eq!(
        store.get(UUID).unwrap().as_deref(),
        Some("damaged-secret-contents")
    );
    assert!(!serde_json::to_string(&status)
        .unwrap()
        .contains("damaged-secret-contents"));
}

/// A sign-in the launcher before this one saved is read once from its service, never written
/// there, and kept under this launcher's own; this launcher's own secret always wins.
#[test]
fn an_earlier_launchers_secret_is_copied_and_the_own_one_wins() {
    let Some(&earlier) = crate::identity::EARLIER_KEYCHAIN_SERVICES.first() else {
        return;
    };
    let kept = std::cell::RefCell::new(Vec::new());
    let read = |service: &'static str| Ok((service == earlier).then(|| "old".to_string()));
    let got = get_carried(None, read, |secret| {
        kept.borrow_mut().push(secret.to_string());
        Ok(())
    });
    assert_eq!(got, Ok(Some("old".into())));
    assert_eq!(*kept.borrow(), ["old"]);
    let got = get_carried(Some("own".into()), read, |_| panic!("nothing to copy"));
    assert_eq!(got, Ok(Some("own".into())));
    // Nothing anywhere: no secret, nothing written.
    assert_eq!(
        get_carried(None, |_| Ok(None), |_| panic!("nothing to copy")),
        Ok(None)
    );
}

#[test]
fn accounts_can_be_reordered_and_persist_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("accounts.json");
    let accounts = Accounts {
        accounts: vec![
            Account {
                uuid: "a".into(),
                name: "Alpha".into(),
                skin: None,
                cape: None,
            },
            Account {
                uuid: "b".into(),
                name: "Beta".into(),
                skin: None,
                cape: None,
            },
            Account {
                uuid: "c".into(),
                name: "Gamma".into(),
                skin: None,
                cape: None,
            },
        ],
        selected: Some("b".into()),
        ..Accounts::default()
    };
    accounts.save_to(&path).unwrap();

    // Moving the first up or last down is a no-op.
    let same = Accounts::reorder_in(&path, "a", -1).unwrap();
    assert_eq!(
        same.accounts
            .iter()
            .map(|a| a.uuid.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    let moved_up = Accounts::reorder_in(&path, "c", -1).unwrap();
    assert_eq!(
        moved_up
            .accounts
            .iter()
            .map(|a| a.uuid.as_str())
            .collect::<Vec<_>>(),
        ["a", "c", "b"]
    );
    let moved_down = Accounts::reorder_in(&path, "a", 1).unwrap();
    assert_eq!(
        moved_down
            .accounts
            .iter()
            .map(|a| a.uuid.as_str())
            .collect::<Vec<_>>(),
        ["c", "a", "b"]
    );
    assert_eq!(Accounts::load_from(&path).selected.as_deref(), Some("b"));
}
