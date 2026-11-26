use super::*;

#[test]
fn a_build_preparation_names_its_step_and_counts_mojangs_files() {
    use sunrise_launcher::backend::PrepareStep;
    let downloading = InstallProgress::Minecraft(PrepareStep::Files, Some((30, 120)));
    assert_eq!(
        stage_label(downloading),
        "Downloading Minecraft's files (30 of 120)"
    );
    assert_eq!(stage_fraction(downloading), Some(0.25));
    let preparing = InstallProgress::Minecraft(PrepareStep::Prepare, None);
    assert_eq!(stage_label(preparing), "Preparing Minecraft's files…");
    assert_eq!(stage_fraction(preparing), None);
}

#[test]
fn an_offline_name_is_a_player_for_installs_and_nothing_selected_is_not() {
    // No account at all, an offline name chosen: installs have a player.
    let offline = Accounts {
        offline: vec!["Builder_1".into()],
        offline_selected: Some("Builder_1".into()),
        ..Accounts::default()
    };
    assert!(install_has_player(&offline, false));
    // A selected account is a player, as before.
    let account = Accounts {
        accounts: vec![account::Account {
            uuid: "abc".into(),
            name: "Ludo".into(),
            skin: None,
            cape: None,
        }],
        selected: Some("abc".into()),
        ..Accounts::default()
    };
    assert!(install_has_player(&account, false));
    // Nothing selected: only the developer's offline mode lets installs start.
    assert!(!install_has_player(&Accounts::default(), false));
    assert!(install_has_player(&Accounts::default(), true));
}

#[test]
fn a_failed_install_says_why_in_a_dialog_not_on_the_card() {
    let root = homes();
    let installs = with_running(root.path());
    let error =
        || InstallError::Unproved("The mod registry index signature could not be verified.".into());
    let (profile, why) = installs
        .install_failure(&Worker::Done(Err(error())))
        .unwrap();
    assert_eq!(profile.id, "A");
    assert_eq!(
        why,
        "Couldn't check that the mods fit\n\nThe mod registry index signature could not be verified."
    );
    assert!(installs
        .install_failure(&Worker::Done(Err(InstallError::Cancelled)))
        .is_none());
    // Installed, but a dependency was left off: the dialog says so too.
    let left_off = InstalledBatch {
        added: Vec::new(),
        updated: Vec::new(),
        failure: Some(BuildNotice {
            changes: vec![sunrise_launcher::browser::sunrise::BuildChange {
                project: "lib".into(),
                title: "Lib".into(),
                reason: sunrise_launcher::browser::installed::InstallHold::Compile,
                predecessor: None,
            }],
            detail: String::new(),
            log: None,
        }),
    };
    let (_, why) = installs
        .install_failure(&Worker::Done(Ok(left_off)))
        .unwrap();
    assert_eq!(
        why,
        "Some mods stayed off or kept their old version\n\nLib: Keep installed, but off. Its code couldn't build on your computer."
    );
    let clean = InstalledBatch {
        added: Vec::new(),
        updated: Vec::new(),
        failure: None,
    };
    assert!(installs.install_failure(&Worker::Done(Ok(clean))).is_none());
    let failed = Outcome {
        failure: Some((failure_line(&error()).into(), error().to_string())),
        ..Outcome::default()
    };
    assert_eq!(failed.note(), None);
}

pub(super) fn request(home: &Path, id: &str, target: &str) -> Request {
    let slug = home.file_name().unwrap().to_str().unwrap();
    let profile = instances::Instance {
        id: slug.into(),
        name: slug.into(),
        kind: instances::Kind::Own,
        home: home.into(),
        work: home.into(),
        incarnation: None,
        icon: None,
        group: None,
        version: None,
        channel: Default::default(),
        native_profile: Default::default(),
        played: Default::default(),
        last_played: None,
        launch: Default::default(),
    };
    let identity = Identity::of(home).unwrap();
    Request {
        identity,
        profile_target: sunrise_launcher::managed_play::Target::resumed(&profile, identity),
        profile,
        target: Some(target.into()),
        release: None,
        displayed_names: BTreeMap::new(),
        github: None,
        local: None,
        sources: Vec::new(),
        picks: None,
        view: RequestView {
            kind: Kind::InstallV2,
            card: id.into(),
            ids: vec![id.into()],
        },
    }
}

fn with_running(root: &Path) -> Installs {
    let first = request(&root.join("A"), "one", "nightly");
    Installs {
        running: Some(Running {
            profile: first.profile,
            identity: first.identity,
            target: first.target,
            requests: vec![first.view],
            stage: InstallProgress::Checking,
            eta: crate::eta::StageEta::default(),
            cancellation: Cancellation::default(),
            github: None,
            local: None,
            source: None,
        }),
        ..Installs::default()
    }
}

#[test]
fn a_registry_install_keeps_its_profile_build_while_it_runs() {
    let root = homes();
    let installs = with_running(root.path());
    let run = installs.running.as_ref().unwrap();
    assert!(run.source.is_none(), "a registry install, not a source one");
    assert_eq!(installs.base(&run.profile), Some("nightly"));
    let other = request(&root.path().join("B"), "two", "dev").profile;
    assert_eq!(installs.base(&other), None);
}

#[test]
fn failed_github_add_retries_only_the_checked_subset_and_never_a_version_refusal() {
    let root = homes();
    let mut installs = with_running(root.path());
    let run = installs.running.as_mut().unwrap();
    let target = GithubTarget {
        profile: run.profile.clone(),
        identity: run.identity,
        profile_target: Target::resumed(&run.profile, run.identity),
        target: run.target.clone(),
        release: None,
    };
    let exact = crate::github_mods_page::tests::checked_subset(target);
    run.github = Some(exact.clone());
    let failed = Worker::Done(Err(InstallError::GithubBuild(
        "fixture build stopped".into(),
    )));
    let retry = installs.github_feedback(&failed).unwrap().retry.unwrap();
    assert!(installs.handle(failed));
    // The retry survives losing the worker and owns the exact files and revision it used.
    assert!(Arc::ptr_eq(&retry, &exact));
    assert_eq!(retry.source.commit, "a".repeat(40));
    let GithubSelection::Add(selected) = &retry.selection else {
        panic!("expected a fresh Add");
    };
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "two");
    assert!(selected[0].file.is_file());

    let mut installs = with_running(root.path());
    installs.running.as_mut().unwrap().github = Some(retry);
    let wrong = Worker::Done(Err(InstallError::NotForVersion(
        "fixture version refusal".into(),
    )));
    assert!(installs.github_feedback(&wrong).unwrap().retry.is_none());
}

#[test]
fn a_refused_source_add_opens_one_modal_naming_each_refused_member() {
    let root = homes();
    let mut installs = with_running(root.path());
    let member = |id: &str, name: &str| (id.to_owned(), name.to_owned(), format!("someone/{id}"));
    installs.running.as_mut().unwrap().source = Some(RunningSource {
        ids: vec!["jade".into()],
        names: vec!["Jade".into()],
        members: vec![member("jade", "Jade")],
        update: false,
        cancellation: Default::default(),
        rejected: BTreeMap::new(),
    });
    let refused = Worker::Done(Err(InstallError::Source(SourceError::DoesNotFit(
        "jade pins build ed3ac19".into(),
    ))));
    let (_, modal) = installs.refused(&refused).unwrap();
    assert_eq!(modal.mods.len(), 1);
    assert_eq!(modal.mods[0].name, "Jade");
    assert_eq!(modal.mods[0].repo.as_deref(), Some("someone/jade"));
    assert!(modal.mods[0].refusal.said.starts_with("Jade isn't ready"));
    assert_eq!(modal.mods[0].refusal.details, "jade pins build ed3ac19");
    let cancelled = Worker::Done(Err(InstallError::Source(SourceError::Cancelled)));
    assert_eq!(installs.refused(&cancelled), None);

    // A collection that added the rest names only the member it skipped.
    let source = installs.running.as_mut().unwrap().source.as_mut().unwrap();
    source.ids = vec!["jade".into(), "minimap".into()];
    source.names = vec!["Jade".into(), "Minimap".into()];
    source.members = vec![member("jade", "Jade"), member("minimap", "Minimap")];
    source
        .rejected
        .insert("minimap".into(), SourceError::Changed(String::new()));
    let (_, modal) = installs
        .refused(&Worker::Done(Ok(InstalledBatch::default())))
        .unwrap();
    assert_eq!(modal.names, ["Minimap"]);
    assert_eq!(modal.mods[0].name, "Minimap");
    assert!(installs
        .refused(&Worker::Progress(InstallProgress::Checking))
        .is_none());

    // The rest then failing for its own reason is said once more, after the member.
    let failed = Worker::Done(Err(InstallError::Source(SourceError::DoesNotFit(
        "jade pins build ed3ac19".into(),
    ))));
    let (_, modal) = installs.refused(&failed).unwrap();
    let said: Vec<_> = modal.mods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(said, ["Minimap", "Jade"]);
    assert!(modal.mods[1].refusal.said.starts_with("Jade isn't ready"));
    // Stopping it opens nothing, whatever a member said before.
    let stopped = Worker::Done(Err(InstallError::Source(SourceError::Cancelled)));
    assert_eq!(installs.refused(&stopped), None);
}

#[test]
fn later_clicks_wait_fifo_and_a_repeated_click_queues_once() {
    let root = homes();
    let a = root.path().join("A");
    let b = root.path().join("B");
    let mut installs = with_running(root.path());
    installs.submit(request(&b, "two", "nightly"));
    installs.submit(request(&a, "three", "nightly"));
    installs.handle(Worker::Progress(InstallProgress::Building(Some(0.5))));
    installs.submit(request(&a, "four", "nightly"));
    installs.submit(request(&a, "four", "nightly"));
    assert!(matches!(
        installs.card(&a, "one"),
        Some(Card::Progress(InstallProgress::Building(Some(0.5))))
    ));
    assert!(matches!(installs.card(&a, "three"), Some(Card::Waiting)));
    assert_eq!(
        installs
            .waiting
            .iter()
            .map(|q| q.view.card.as_str())
            .collect::<Vec<_>>(),
        ["two", "three", "four"]
    );
}

fn homes() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for id in ["A", "B"] {
        std::fs::create_dir(root.path().join(id)).unwrap();
    }
    root
}

#[test]
fn queued_target_cannot_open_the_replacement_home() {
    let root = homes();
    let a = root.path().join("A");
    let b = root.path().join("B");
    let mut installs = with_running(root.path());
    installs.submit(request(&b, "two", "nightly"));
    installs.submit(request(&a, "three", "stable"));
    assert!(matches!(installs.card(&b, "two"), Some(Card::Waiting)));

    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mods_page::install::tests::replace_home_worker"])
        .env("FIXTURE_REPLACE_QUEUED_HOME", &b)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read(b.join("replacement")).unwrap(), b"new owner");
    installs.handle(Worker::Done(Ok(InstalledBatch::default())));
    let queued = installs.waiting.pop_front().unwrap();
    let error = match sunrise_launcher::managed_play::Profile::open_for(
        &queued.profile.home,
        None,
        Some(queued.identity),
    ) {
        Ok(_) => panic!("admitted a replacement"),
        Err(error) => String::from(error),
    };
    assert!(
        error.contains("profile was replaced while the request was waiting"),
        "{error}"
    );
    assert!(!b.join(".install-txn.lock").exists());
    assert!(!b.join("mod-browser").exists());
    let next = installs.waiting.pop_front().unwrap();
    assert_eq!(next.view.card, "three");
    assert_eq!(next.identity, Identity::of(&next.profile.home).unwrap());
}

#[test]
fn replace_home_worker() {
    let Some(home) = std::env::var_os("FIXTURE_REPLACE_QUEUED_HOME").map(PathBuf::from) else {
        return;
    };
    let owner = home.with_extension("owner.lock");
    let _admitted = sunrise_launcher::profile_txn::Admission::open(&home, Some(&owner)).unwrap();
    std::fs::rename(&home, home.with_extension("detached")).unwrap();
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join("replacement"), b"new owner").unwrap();
}

#[test]
fn a_later_click_cannot_change_the_destination_of_a_queued_retry() {
    use launcher_play::{PlayDestination, PlayIntent};
    let root = homes();
    let mut installs = with_running(root.path());
    let profile = installs.running.as_ref().unwrap().profile.clone();
    let retry = PlayIntent {
        instance: profile.id.clone(),
        incarnation: profile.incarnation,
        account: Some("accepted".into()),
        destination: PlayDestination::Quick("--world", "original world".into()),
    };
    assert!(installs.defer_play(&profile, retry.clone()));
    assert!(!installs.defer_play(
        &profile,
        PlayIntent {
            destination: PlayDestination::Quick("--world", "other world".into()),
            ..retry
        }
    ));
    installs.handle(Worker::Done(Ok(InstalledBatch::default())));
    let (_, accepted) = installs.take_play().unwrap();
    let mut cmd = std::process::Command::new("sunrise-client");
    accepted.destination.apply(&mut cmd).unwrap();
    assert_eq!(
        cmd.get_args().collect::<Vec<_>>(),
        ["--world", "original world"]
    );
    assert_eq!(accepted.account.as_deref(), Some("accepted"));
}

#[test]
fn deferred_play_is_kept_only_for_success_and_failure_is_scoped_to_its_profile() {
    let root = homes();
    for (home, pass, kept) in [("A", true, true), ("A", false, false), ("B", false, true)] {
        let mut installs = with_running(root.path());
        installs.play = Some((
            root.path().join(home),
            launcher_play::PlayIntent {
                instance: home.into(),
                incarnation: None,
                account: Some("accepted".into()),
                destination: launcher_play::PlayDestination::Home,
            },
        ));
        installs.handle(Worker::Done(if pass {
            Ok(InstalledBatch::default())
        } else {
            Err(InstallError::Download("timeout".into()))
        }));
        assert_eq!(
            installs.take_play().and_then(|(_, intent)| intent.account),
            kept.then(|| "accepted".into())
        );
    }
}

#[test]
fn local_install_stays_tracked_and_honours_cancellation() {
    let root = homes();
    let smod = root.path().join("local-tweaks.smod");
    let bytes = sunrise_launcher_smod::Package::write_files(
        "id = 'local_tweaks'\nname = 'Local Tweaks'\nversion = '0.1.0'\napi = '0.2'\nside = 'both'\n",
        &[],
    )
    .unwrap();
    std::fs::write(&smod, &bytes).unwrap();
    let pkg = sunrise_launcher::github_mods::read_local_smod(&smod).unwrap();

    let mut installs = with_running(root.path());
    let run = installs.running.as_mut().unwrap();
    let target = GithubTarget {
        profile: run.profile.clone(),
        identity: run.identity,
        profile_target: Target::resumed(&run.profile, run.identity),
        target: run.target.clone(),
        release: None,
    };
    run.requests = vec![RequestView {
        kind: Kind::Local,
        card: smod.display().to_string(),
        ids: vec![pkg.id.clone()],
    }];
    run.local = Some(Arc::new(LocalInput {
        target,
        label: smod.display().to_string(),
        names: vec![pkg.name.clone()],
        packages: vec![pkg],
    }));

    // Stopping before publication rejects any subsequent world review prompt and suppresses error banners.
    run.cancellation.stop();
    let (reply, answer) = std::sync::mpsc::channel();
    let question = WorldQuestion {
        removal: false,
        cancel_default: true,
        heading: "Review".into(),
        detail: "Detail".into(),
    };
    assert!(!installs.handle(Worker::World(question, reply)));
    assert!(!answer.recv().unwrap());
    assert!(installs
        .github_feedback(&Worker::Done(Err(InstallError::Cancelled)))
        .is_none());

    // Successful completion reports the installed mod name and clears running state.
    let ok = installs
        .github_feedback(&Worker::Done(Ok(InstalledBatch::default())))
        .unwrap();
    assert!(ok.local);
    assert_eq!(ok.result.unwrap(), "Installed Local Tweaks.");
    assert!(installs.handle(Worker::Done(Ok(InstalledBatch::default()))));
    assert!(installs.running.is_none());
}
