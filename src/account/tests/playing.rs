//! Who may play and as whom: the owner gate, sessions, and launching as an account.

use super::*;

#[test]
fn unread_session_files_go_once_the_game_would_refuse_them() {
    let dir = std::env::temp_dir().join(format!("hz-session-reap-{}", std::process::id()));
    let secrets = secrets("not-a-real-token", true);
    let waiting = write_session_file(&dir, &notch(), &secrets).unwrap();
    let stale = write_session_file(&dir, &notch(), &secrets).unwrap();
    let read = write_session_file(&dir, &notch(), &secrets).unwrap();
    // A game that never started: its file is past the age the game takes.
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(300))
        .unwrap();
    // A game that started took its file (and deleted it).
    std::fs::remove_file(&read).unwrap();
    let mut unread = [waiting.clone(), stale.clone(), read]
        .into_iter()
        .map(|path| UnreadSession {
            path,
            expire_only: true,
        })
        .collect();
    reap(&mut unread, SESSION_FILE_LIFE);
    assert!(waiting.exists(), "a game may still take it");
    assert!(!stale.exists(), "no token left behind");
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].path, waiting);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_owner_plays_with_a_session() {
    let (_, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    let pass = launch(&ep, Some("test-client"), &store, &mut accounts).unwrap();
    let (account, session) = pass.session.unwrap();
    assert_eq!(
        (account.name.as_str(), session.minecraft_token.as_str()),
        ("Notch", "mc-token")
    );
    assert!(pass.owner.is_some() && pass.offline.is_none());
    // The profile as the services have it now (the skin is new).
    assert!(accounts.selected().unwrap().skin.is_some());
}

#[test]
fn a_renewed_sign_in_the_keychain_refuses_still_plays_and_says_so() {
    let (_, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let stale = Secrets {
        minecraft_expires_at: now() + 60,
        ..secrets("old-token", true)
    };
    let mut accounts = signed_in(&store, &stale);
    let refusing = Refusing(store);
    let pass = launch(&ep, Some("test-client"), &refusing, &mut accounts).unwrap();
    assert_eq!(pass.session.unwrap().1.minecraft_token, "mc-token");
    let said = "Notch's renewed sign-in couldn't be saved in the keychain (the keyring is \
                locked): you may have to sign in again next time.";
    assert_eq!(pass.unsaved.as_deref(), Some(said));
    assert_eq!(take_unsaved().as_deref(), Some(said));
    assert_eq!(take_unsaved(), None, "said once");
    // Nothing to save, nothing to say.
    let mut accounts = signed_in(&refusing.0, &secrets("mc-token", true));
    let pass = launch(&ep, Some("test-client"), &refusing, &mut accounts).unwrap();
    assert!(pass.unsaved.is_none());
}

#[test]
fn a_no_from_the_services_blocks_at_once() {
    let (_, ep) = mock(|m| m.owns_game = false);
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    assert!(accounts.has_owner(&store));
    assert_eq!(
        launch(&ep, Some("test-client"), &store, &mut accounts).unwrap_err(),
        NoOwner::default()
    );
    // The answer is kept beside the tokens: offline now, it still says no.
    assert!(!kept(&store).owns && !accounts.has_owner(&store));
    assert_eq!(
        launch(&offline(), Some("test-client"), &store, &mut accounts).unwrap_err(),
        NoOwner::default()
    );
    assert_eq!(
        NoOwner::default().to_string(),
        "Sign in with a Microsoft account that owns Minecraft: Java Edition to play."
    );
    assert_eq!(
        NoOwner::default().in_window(),
        "Sign in with a Microsoft account that owns Minecraft: Java Edition to play. Open Settings and sign in under Account."
    );
}

#[test]
fn no_network_never_blocks_a_past_owner() {
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    let pass = launch(&offline(), Some("test-client"), &store, &mut accounts).unwrap();
    // Plays, offline, and says why.
    assert!(pass.session.is_none() && pass.owner.is_none());
    assert!(pass.offline.unwrap().contains("Could not reach"));
    // A token to renew with no network, or a build without sign-in: the same.
    let stale = Secrets {
        minecraft_expires_at: 0,
        ..secrets("mc-token", true)
    };
    let mut accounts = signed_in(&store, &stale);
    assert!(launch(&offline(), Some("test-client"), &store, &mut accounts).is_ok());
    assert!(launch(&offline(), None, &store, &mut accounts).is_ok());
    // An account that never passed doesn't pass offline.
    let mut accounts = signed_in(&store, &secrets("mc-token", false));
    assert!(launch(&offline(), Some("test-client"), &store, &mut accounts).is_err());
}

#[test]
fn accounts_from_before_the_gate_owned_the_game_at_sign_in() {
    let old = r#"{"refresh_token":"r","minecraft_token":"t","minecraft_expires_at":1,"xuid":null}"#;
    assert!(serde_json::from_str::<Secrets>(old).unwrap().owns);
}

#[test]
fn any_owner_opens_the_gate_and_the_one_who_plays_brings_the_session() {
    let (m, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    // A second account, no longer in the keychain, plays.
    accounts.accounts.push(Account {
        uuid: "other".into(),
        name: "Alex".into(),
        skin: None,
        cape: None,
    });
    accounts.selected = Some("other".into());
    let pass = launch(&ep, Some("test-client"), &store, &mut accounts).unwrap();
    assert!(pass.session.is_none());
    assert!(pass
        .offline
        .unwrap()
        .starts_with("Alex is no longer signed in"));
    assert_eq!(pass.owner.unwrap().0.name, "Notch");
    // Nobody selected (a download's gate): Notch is asked, and no session comes back.
    accounts.selected = None;
    m.lock().unwrap().log.clear();
    let pass = launch(&ep, Some("test-client"), &store, &mut accounts).unwrap();
    assert!(pass.session.is_none() && pass.offline.is_none());
    assert_eq!(m.lock().unwrap().log.len(), 2);
    // No account at all.
    assert!(launch(&ep, Some("test-client"), &store, &mut Accounts::default()).is_err());
}

#[test]
fn the_game_plays_only_as_an_account_that_owns_it() {
    let store = MemoryStore::default();
    let (_, ep) = mock(|_| {});
    // No account: no game.
    let refused = launch_for_game(
        &ep,
        Some("test-client"),
        &store,
        &mut Accounts::default(),
        false,
    );
    assert_eq!(refused.unwrap_err(), NoOwner::default());
    // An owner, none selected: it plays, with its session.
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    accounts.selected = None;
    let pass = launch_for_game(&ep, Some("test-client"), &store, &mut accounts, false).unwrap();
    assert_eq!(pass.session.unwrap().0.name, "Notch");
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    // Another account plays: Notch owning the game doesn't let it play under a name of its own.
    accounts.accounts.push(Account {
        uuid: "other".into(),
        name: "Alex".into(),
        skin: None,
        cape: None,
    });
    accounts.play_as("other").unwrap();
    let refused =
        launch_for_game(&ep, Some("test-client"), &store, &mut accounts, false).unwrap_err();
    assert!(refused
        .to_string()
        .starts_with("Alex is no longer signed in"));
    // The services say the account that plays doesn't own the game.
    let (_, ep) = mock(|m| m.owns_game = false);
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    let refused = launch_for_game(&ep, Some("test-client"), &store, &mut accounts, false);
    assert_eq!(refused.unwrap_err(), NoOwner::default());
}

#[test]
fn an_offline_name_plays_without_an_owner_only_with_the_forks_pass() {
    let store = MemoryStore::default();
    let (_, ep) = mock(|_| {});
    // A kept offline name, no account at all: the strict gate refuses, the pass starts the
    // game with no session and no account picked for it.
    let mut accounts = Accounts::default();
    accounts
        .play_as_player(&store, Player::Offline("Builder_1".into()), true)
        .unwrap();
    assert_eq!(accounts.player(), Player::Offline("Builder_1".into()));
    let refused = launch_for_game(
        &ep,
        Some("test-client"),
        &store,
        &mut accounts.clone(),
        false,
    );
    assert_eq!(refused.unwrap_err(), NoOwner::default());
    let pass = launch_for_game(&ep, Some("test-client"), &store, &mut accounts, true).unwrap();
    assert!(pass.session.is_none() && pass.owner.is_none());
    assert_eq!(
        accounts.selected, None,
        "no account is picked for an offline name"
    );
    assert_eq!(accounts.name_arg(), Some("Builder_1"));
    // Without the pass, choosing an offline name beside no owner is refused at the switch.
    let mut fresh = Accounts::default();
    assert!(fresh
        .play_as_player(&store, Player::Offline("Builder_1".into()), false)
        .is_err());
    assert!(fresh.offline.is_empty());
}

#[test]
fn refusals_have_messages() {
    let tokens = || MsTokens {
        access: "ms-access-1".into(),
        refresh: "r".into(),
    };
    let (_, ep) = mock(|m| m.xerr = Some(2148916233));
    assert!(finish_sign_in(&ep, tokens())
        .unwrap_err()
        .to_string()
        .contains("no Xbox profile"));
    let (_, ep) = mock(|m| m.xerr = Some(2148916238));
    assert!(finish_sign_in(&ep, tokens())
        .unwrap_err()
        .to_string()
        .contains("child account (error 2148916238)"));
    let (_, ep) = mock(|m| m.xerr = Some(2148916238));
    assert!(finish_sign_in(&ep, tokens())
        .unwrap_err()
        .to_string()
        .contains("https://account.microsoft.com/family"));
    let (_, ep) = mock(|m| m.owns_game = false);
    assert_eq!(
        finish_sign_in(&ep, tokens()).unwrap_err().to_string(),
        DOES_NOT_OWN
    );
    let (_, ep) = mock(|m| m.no_profile = true);
    assert!(finish_sign_in(&ep, tokens())
        .unwrap_err()
        .to_string()
        .contains("no Minecraft profile"));
    assert_eq!(
        oauth_error(&json!({"error": "authorization_declined"})).to_string(),
        "Sign-in was declined in the browser."
    );
    // Nothing listening: a message, not a URL.
    assert_eq!(
        device_code(&offline(), "x").unwrap_err().to_string(),
        "Could not reach Microsoft. Check your internet connection."
    );
}

#[test]
fn has_owner_asks_the_selected_account_first() {
    let store = MemoryStore::default();
    // The owner is second in the list but selected: its item alone is read.
    let mut accounts = Accounts::default();
    accounts
        .add(
            &store,
            Account {
                uuid: "other".into(),
                name: "Other".into(),
                skin: None,
                cape: None,
            },
            &secrets("x-token", false),
        )
        .unwrap();
    accounts
        .add(&store, notch(), &secrets("mc-token", true))
        .unwrap();
    let reading = Reading(&store, Mutex::default());
    assert!(accounts.has_owner(&reading));
    assert_eq!(*reading.1.lock().unwrap(), vec![UUID]);
}

#[test]
fn session_files_are_private_and_complete() {
    let dir = std::env::temp_dir().join(format!("sunrise-launcher-session-{}", std::process::id()));
    let account = Account {
        uuid: UUID.into(),
        name: "Notch".into(),
        skin: None,
        cape: None,
    };
    let secrets = Secrets {
        refresh_token: "r".into(),
        sign_in_required: false,
        minecraft_token: "mc-token".into(),
        minecraft_expires_at: 1,
        xuid: Some("25".into()),
        owns: true,
    };
    let path = write_session_file(&dir, &account, &secrets).unwrap();
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        v,
        json!({"name": "Notch", "uuid": UUID, "access_token": "mc-token", "user_type": "msa", "xuid": "25"})
    );
    // The refresh token never leaves the keychain.
    assert!(!v.to_string().contains("\"r\""));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let other = write_session_file(&dir, &account, &secrets).unwrap();
    assert_ne!(path, other);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_running_game_gets_a_renewed_session_only_when_ready() {
    let store = MemoryStore::default();
    signed_in(&store, &secrets("old-minecraft", true));
    let (_, ep) = mock(|_| {});
    let dir = std::env::temp_dir().join(format!("sunrise-launcher-renew-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let file =
        session::renew_for_game_with(&ep, Some("test-client"), &store, &notch(), &nowhere(), &dir)
            .unwrap();
    let handed: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(handed["uuid"], UUID);
    assert_eq!(handed["access_token"], "mc-token");
    // A refused refresh hands over nothing and says to sign in again.
    signed_in(
        &store,
        &Secrets {
            refresh_token: "revoked".into(),
            ..secrets("old-minecraft", true)
        },
    );
    let refused =
        session::renew_for_game_with(&ep, Some("test-client"), &store, &notch(), &nowhere(), &dir)
            .unwrap_err();
    assert_eq!(refused.state, session::State::SignInRequired);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_launch_as_another_account_leaves_the_selected_one() {
    let store = MemoryStore::default();
    // Notch is signed in, owns the game and is selected; Alex is listed but no longer has
    // tokens in the keychain.
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    let alex = "0f22b7ed5e3d4a2c9bfe2f6f8f1f6a01";
    accounts.accounts.push(Account {
        uuid: alex.into(),
        name: "Alex".into(),
        skin: None,
        cape: None,
    });
    // This launch plays as Alex: it is Alex who is told to sign in again (Notch owning the game
    // doesn't let Alex play), and Notch is still the one `account select` picked.
    let refused = launch_as(&offline(), Some("test-client"), &store, &mut accounts, alex);
    assert!(refused
        .unwrap_err()
        .starts_with("Alex is no longer signed in"));
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    // Notch plays with the last answer while the services can't be reached.
    let pass = launch_as(&offline(), Some("test-client"), &store, &mut accounts, UUID).unwrap();
    assert!(pass.session.is_none() && pass.offline.is_some());
    // An account that isn't in the launcher is refused, and changes nothing either.
    let gone = launch_as(
        &offline(),
        Some("test-client"),
        &store,
        &mut accounts,
        "not-signed-in",
    );
    assert_eq!(gone.unwrap_err(), "That account isn't signed in.");
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
}

#[test]
fn play_starts_as_the_account_it_was_pressed_for_whoever_is_selected_since() {
    let store = MemoryStore::default();
    // Notch pressed Play. While the game installed, the switcher moved to Alex, who is listed
    // but has no tokens in the keychain.
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    let alex = "0f22b7ed5e3d4a2c9bfe2f6f8f1f6a01";
    accounts.accounts.push(Account {
        uuid: alex.into(),
        name: "Alex".into(),
        skin: None,
        cape: None,
    });
    accounts.selected = Some(alex.into());
    // Reading whoever is selected at sign-in, as Play did, answers for Alex.
    let mut as_selected = accounts.clone();
    assert!(
        launch_for_game(
            &offline(),
            Some("test-client"),
            &store,
            &mut as_selected,
            false
        )
        .is_err(),
        "the selected account is Alex, who can't play"
    );
    // The accepted account plays, and the switcher's choice stays.
    let pass = launch_accepted(
        &offline(),
        Some("test-client"),
        &store,
        &mut accounts,
        UUID,
        false,
    )
    .unwrap();
    assert!(pass.offline.is_some());
    assert_eq!(accounts.selected.as_deref(), Some(alex));
    // Signed out before the game started: nobody plays in its place.
    accounts.remove(&store, UUID).unwrap();
    let gone = launch_accepted(
        &offline(),
        Some("test-client"),
        &store,
        &mut accounts,
        UUID,
        false,
    );
    assert!(gone.unwrap_err().keyring.unwrap().contains("signed out"));
    assert_eq!(accounts.selected.as_deref(), Some(alex));
}

#[test]
fn sign_out_after_preparing_a_session_refuses_spawn_and_drops_only_that_session() {
    let store = MemoryStore::default();
    let dir = tempfile::tempdir().unwrap();
    let accounts_file = dir.path().join("accounts.json");
    let mut accounts = signed_in(&store, &secrets("mc-token", true));
    accounts.accounts.push(Account {
        uuid: "other-account".into(),
        name: "Other".into(),
        skin: None,
        cape: None,
    });
    accounts.selected = Some("other-account".into());
    accounts.save_to(&accounts_file).unwrap();
    assert!(check_launch_account(&Accounts::load_from(&accounts_file), UUID).is_ok());
    let prepared = dir.path().join("pending-session");
    let other = dir.path().join("another-session");
    std::fs::write(&prepared, "prepared for the accepted account").unwrap();
    std::fs::write(&other, "unrelated session").unwrap();

    // Another process signs the accepted account out while jars are being prepared.
    accounts.remove(&store, UUID).unwrap();
    accounts.save_to(&accounts_file).unwrap();
    let now = Accounts::load_from(&accounts_file);
    assert_eq!(now.selected().unwrap().uuid, "other-account");
    assert!(check_launch_account(&now, UUID)
        .unwrap_err()
        .in_window()
        .contains("signed out"));
    discard_launch_session(Some(prepared.clone()));
    assert!(!prepared.exists());
    assert_eq!(std::fs::read_to_string(other).unwrap(), "unrelated session");
}

/// A launch loads the accounts, asks the services for a while, then saves: a sign-in or sign-out
/// made meanwhile stays, and only the names and skins it renewed are written.
#[test]
fn a_launch_saving_late_keeps_sign_ins_and_outs_made_meanwhile() {
    let store = MemoryStore::default();
    let dir = std::env::temp_dir().join(format!("sunrise-launcher-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let file = dir.join("accounts.json");
    let other = |uuid: &str, name: &str| Account {
        uuid: uuid.into(),
        name: name.into(),
        skin: None,
        cape: None,
    };
    let mut before = signed_in(&store, &secrets("mc-token", true));
    before.accounts.push(other("gone", "Leaving"));
    before.save_to(&file).unwrap();
    // The launch's copy, with Notch's new skin.
    let mut renewed = Accounts::load_from(&file);
    renewed.accounts[0].skin = Some(("https://textures.example/notch".into(), "classic".into()));
    // Meanwhile: one account signs out, another signs in.
    let mut now = Accounts::load_from(&file);
    now.accounts.retain(|a| a.uuid != "gone");
    now.accounts.push(other("new", "Arriving"));
    now.save_to(&file).unwrap();
    renewed.save_renewed_to(&store, &file).unwrap();
    let saved = Accounts::load_from(&file);
    let uuids: Vec<&str> = saved.accounts.iter().map(|a| a.uuid.as_str()).collect();
    assert_eq!(uuids, [UUID, "new"]);
    assert!(
        saved.find(UUID).unwrap().skin.is_some(),
        "the renewed skin is kept"
    );
    assert_eq!(saved.selected.as_deref(), Some(UUID));
    // Written whole through a new file: nothing else is left beside it.
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from("accounts.json")]);
    let _ = std::fs::remove_dir_all(&dir);
}
