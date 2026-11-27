//! Switch a played profile to another exact build, as Update game does: the build it lands on,
//! what it saves about following, and what a cancelled or failed switch leaves.
use super::*;
use crate::managed_play::Active;

const NEXT: &str = "developer-next";

/// Another installed and prepared build, beside the fixture's own.
fn next_build() {
    let base = backend::version_dir(NEXT);
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join(".installed"), []).unwrap();
    fs::write(base.join(backend::CLIENT), b"fixture next client").unwrap();
    fs::write(
        base.join("developer.json"),
        json!({"revision": "a".repeat(40)}).to_string(),
    )
    .unwrap();
    let resources = backend::resources(NEXT);
    fs::create_dir_all(&resources).unwrap();
    fs::write(resources.join("sunrise-resources.json"), b"{}").unwrap();
}

fn switch(
    fixture: &Fixture,
    choice: Option<BuildChoice>,
    cancellation: &native_mods::Cancellation,
    progress: &dyn Fn(InstallProgress),
    index: Result<Arc<index_v2::Index>, String>,
) -> Result<Switched, String> {
    v2::switch_profile_build_v2_with(
        &fixture.instance,
        install_target(&fixture.instance).unwrap(),
        NEXT,
        choice,
        v2::V2SwitchHooks {
            switch: SwitchHooks {
                cancellation,
                progress,
                approve: &|_| Ok(SwitchDecision::Cancel),
                at_cutoff: &|| Ok(()),
            },
            confirm_publisher_change: None,
            pin_dir: None,
        },
        || index,
        &fetch_fixture,
        &verify_fixture,
    )
}

fn played_base(fixture: &Fixture) -> String {
    Active::read_pointer(fixture.home())
        .unwrap()
        .expect("a played profile")
        .base()
        .to_owned()
}

fn saved_pin(fixture: &Fixture) -> Option<String> {
    crate::instances::list()
        .into_iter()
        .find(|instance| instance.id == fixture.instance.id)
        .unwrap()
        .version
}

#[test]
fn update_game_lands_on_the_exact_build_and_a_following_profile_keeps_following() {
    Fixture::run(
        concat!(
            module_path!(),
            "::update_game_lands_on_the_exact_build_and_a_following_profile_keeps_following"
        ),
        |mut fixture| {
            fixture.project("lantern", "Lanterns", V2Kind::Mod);
            fixture.release("lantern", "1.0.0", "", &[(AMBER, EARLIER_BLOCK)]);
            fixture.install(&["lantern"]);
            next_build();
            assert_eq!(played_base(&fixture), BUILD);
            assert_eq!(saved_pin(&fixture), None, "the profile follows its channel");
            let index = fixture.index_for(NEXT);
            let Switched::Changed(active) = switch(
                &fixture,
                None,
                &native_mods::Cancellation::default(),
                &|_| {},
                Ok(index),
            )
            .unwrap() else {
                panic!("expected the switch to change the profile")
            };
            assert_eq!(active.base(), NEXT);
            assert_eq!(
                played_base(&fixture),
                NEXT,
                "the exact build is the one played"
            );
            assert_eq!(
                saved_pin(&fixture),
                None,
                "still following: no pin was saved"
            );
            assert!(mod_list::is_enabled(fixture.home(), "lantern"));
        },
    );
}

#[test]
fn update_game_keeps_a_pinned_profile_on_the_new_build() {
    Fixture::run(
        concat!(
            module_path!(),
            "::update_game_keeps_a_pinned_profile_on_the_new_build"
        ),
        |mut fixture| {
            fixture.project("lantern", "Lanterns", V2Kind::Mod);
            fixture.release("lantern", "1.0.0", "", &[(AMBER, EARLIER_BLOCK)]);
            fixture.install(&["lantern"]);
            next_build();
            let index = fixture.index_for(NEXT);
            switch(
                &fixture,
                Some(BuildChoice::Keep),
                &native_mods::Cancellation::default(),
                &|_| {},
                Ok(index),
            )
            .unwrap();
            assert_eq!(played_base(&fixture), NEXT);
            assert_eq!(saved_pin(&fixture).as_deref(), Some(NEXT));
        },
    );
}

#[test]
fn a_cancelled_or_failed_update_leaves_the_old_build_played() {
    Fixture::run(
        concat!(
            module_path!(),
            "::a_cancelled_or_failed_update_leaves_the_old_build_played"
        ),
        |mut fixture| {
            fixture.project("lantern", "Lanterns", V2Kind::Mod);
            fixture.release("lantern", "1.0.0", "", &[(AMBER, EARLIER_BLOCK)]);
            fixture.install(&["lantern"]);
            next_build();
            let before = fixture.snapshot(&[LANTERN]);
            // Cancelled before it starts, and from its first step on.
            let stopped = native_mods::Cancellation::default();
            stopped.stop();
            let index = fixture.index_for(NEXT);
            assert!(switch(&fixture, None, &stopped, &|_| {}, Ok(index.clone())).is_err());
            let later = native_mods::Cancellation::default();
            let outcome = switch(&fixture, None, &later, &|_| later.stop(), Ok(index));
            assert!(
                outcome.is_err() || matches!(outcome, Ok(Switched::Stayed)),
                "{:?}",
                outcome.map(|_| ())
            );
            // Failed: the signed index can't be read.
            assert!(switch(
                &fixture,
                None,
                &native_mods::Cancellation::default(),
                &|_| {},
                Err("offline".into()),
            )
            .is_err());
            assert_eq!(
                played_base(&fixture),
                BUILD,
                "the old build is still played"
            );
            assert_eq!(saved_pin(&fixture), None);
            assert_eq!(
                fixture.snapshot(&[LANTERN]),
                before,
                "the profile is as it was"
            );
        },
    );
}

/// Another installed build whose prepared files the next build can reuse.
const DONOR: &str = "developer-donor";

/// Gives a donor build and the next one what `reuse_prepared` needs to find them compatible (as
/// `backend`'s own reuse test does): the same Minecraft pins and preparer, and prepared output
/// on the donor only. The next build is installed, not prepared. The build the profile plays is
/// left as it was played.
fn compatible_unprepared_next() {
    let oracle = "mods/sunrise/src/test/java/dev/sunriselab/parity/VanillaHeightmapOracle.java";
    let collision = "mods/sunrise/src/test/java/dev/sunriselab/parity/VanillaCollisionOracle.java";
    let base = backend::version_dir(NEXT);
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join(".installed"), []).unwrap();
    fs::write(base.join(backend::CLIENT), b"fixture next client").unwrap();
    fs::write(
        base.join("developer.json"),
        json!({"revision": "a".repeat(40)}).to_string(),
    )
    .unwrap();
    let donor = backend::version_dir(DONOR);
    fs::create_dir_all(&donor).unwrap();
    fs::write(donor.join(".installed"), []).unwrap();
    fs::write(donor.join(backend::CLIENT), b"fixture donor client").unwrap();
    for tag in [DONOR, NEXT] {
        let root = backend::version_dir(tag);
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(
            "research-lock.json",
            &json!({"minecraft": {"version": backend::PINNED_GAME, "client_sha1": "c", "server_sha1": "s"}})
                .to_string(),
        );
        write("tools/prepare_native_client.py", "schema 7");
        write(oracle, "heightmap");
        write(collision, "collision");
    }
    let resources = backend::resources(DONOR);
    fs::create_dir_all(&resources).unwrap();
    fs::write(
        resources.join("sunrise-resources.json"),
        json!({"schema": 7, "collision_exporters_sha256": {collision: "x"}}).to_string(),
    )
    .unwrap();
    fs::create_dir_all(resources.join("worldgen")).unwrap();
    fs::write(resources.join("worldgen/biomes.json"), "{}").unwrap();
}

/// The next build is prepared by the install's own step (another build's prepared files,
/// copied), not by the fixture, and Update game's switch then lands on it (review of #615).
#[test]
fn an_installs_own_steps_prepare_the_build_the_switch_then_lands_on() {
    Fixture::run(
        concat!(
            module_path!(),
            "::an_installs_own_steps_prepare_the_build_the_switch_then_lands_on"
        ),
        |mut fixture| {
            fixture.project("lantern", "Lanterns", V2Kind::Mod);
            fixture.release("lantern", "1.0.0", "", &[(AMBER, EARLIER_BLOCK)]);
            fixture.install(&["lantern"]);
            compatible_unprepared_next();
            assert!(
                !backend::check_prepared(NEXT),
                "the next build starts unprepared"
            );
            backend::reuse_prepared_for(NEXT);
            // The answer above is cached; ask again as the window does after an install.
            backend::forget_prepared();
            assert!(
                backend::check_prepared(NEXT),
                "the install's step prepared it from the donor's files"
            );
            let index = fixture.index_for(NEXT);
            let switched = switch(
                &fixture,
                None,
                &native_mods::Cancellation::default(),
                &|_| {},
                Ok(index),
            )
            .unwrap_or_else(|error| panic!("the switch to the prepared build failed: {error}"));
            let Switched::Changed(active) = switched else {
                panic!("expected the switch to change the profile")
            };
            assert_eq!(active.base(), NEXT);
            assert_eq!(played_base(&fixture), NEXT);
        },
    );
}

/// Two nightly builds the window's index offers (the registry offers only nightly, dev and
/// stable ids), installed and prepared beside the fixture's own: the newer first.
const OFFERED: [(&str, char); 2] = [
    ("nightly-2026-11-22-bbbbbbb", 'b'),
    ("nightly-2026-11-20-aaaaaaa", 'a'),
];

fn offered_build(tag: &str) {
    let base = backend::version_dir(tag);
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join(".installed"), []).unwrap();
    fs::write(base.join(backend::CLIENT), b"fixture offered client").unwrap();
    let resources = backend::resources(tag);
    fs::create_dir_all(&resources).unwrap();
    fs::write(resources.join("sunrise-resources.json"), b"{}").unwrap();
}

/// The catalogue the window is shown: only the offered builds, each mod's releases fitting the
/// builds `fits` names for it, marked as not fitting those `no` names, and neither for the rest.
fn window_index(
    fixture: &Fixture,
    fits: &[(&str, &[&str])],
    no: &[(&str, &[&str])],
) -> index_v2::Index {
    let mut index = fixture.catalogue.clone();
    index.builds = OFFERED
        .iter()
        .map(|(tag, commit)| index_v2::Build {
            id: (*tag).to_owned(),
            commit: commit.to_string().repeat(40),
        })
        .collect();
    let named = |list: &[(&str, &[&str])], id: &str| {
        list.iter()
            .find(|(mod_id, _)| *mod_id == id)
            .map(|(_, builds)| builds.iter().map(|b| (*b).to_owned()).collect())
            .unwrap_or_default()
    };
    // What the registry requires of a published entry (`Index::check`) that the in-memory
    // fixture leaves out: the window never downloads these, as the archives are installed.
    for project in &mut index.projects {
        project.description = format!("{} for the window's captures.", project.name);
        project.screenshots = vec![index_v2::Screenshot {
            path: format!("pages/{}/{}.png", project.id, "1".repeat(64)),
            caption: String::new(),
        }];
        for release in &mut project.releases {
            release.fits = named(fits, &project.mod_id);
            release.does_not_fit = named(no, &project.mod_id);
            release.sunrise = OFFERED[1].0.to_owned();
            release.published = "2026-11-20".into();
            release.file.url = format!(
                "https://fixture.invalid/{}-{}.smod",
                project.mod_id, release.version
            );
            release.file.attestation = format!("{}.sigstore.json", release.file.url);
        }
    }
    index.serial += 1;
    index.generated = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    index
}

/// Writes a played profile for the window's captures (`SUNRISE_CAPTURE_CASE`), with the index
/// the window is shown as `index.json` beside it: the capture job signs that with a disposable
/// key and builds the window with `capture-registry-key`. The profile plays the fixture's build
/// with Wayfinder, Lanterns and Minimap; two offered nightlies are installed beside it.
/// - `fresh`: nothing more.
/// - `needs` and `blocked`: then it switched to another build that has no release of Wayfinder,
///   which held it off (a finished switch, against `fresh`). In `needs` the Nov 20 nightly has
///   releases for all three, the Nov 22 one none of Wayfinder; in `blocked` Nov 22 has none of
///   Wayfinder and Nov 20 none of Lanterns, with Minimap not marked for it.
#[test]
#[ignore = "writes a window capture fixture; the capture job runs it"]
fn capture_fixture() {
    let Ok(case) = std::env::var("SUNRISE_CAPTURE_CASE") else {
        return;
    };
    Fixture::run(
        concat!(module_path!(), "::capture_fixture"),
        |mut fixture| {
            for (id, name) in [
                ("wayfinder", "Wayfinder"),
                ("lantern", "Lanterns"),
                ("minimap", "Minimap"),
            ] {
                fixture.project(id, name, V2Kind::Mod);
                fixture.release(id, "1.0.0", "", &[]);
            }
            fixture.install(&["wayfinder", "lantern", "minimap"]);
            for (tag, _) in OFFERED {
                offered_build(tag);
            }
            let [(n22, _), (n20, _)] = OFFERED;
            let all: &[&str] = &[n22, n20];
            let shown = match case.as_str() {
                "fresh" | "needs" => window_index(
                    &fixture,
                    &[
                        ("wayfinder", &[n20][..]),
                        ("lantern", all),
                        ("minimap", all),
                    ],
                    &[("wayfinder", &[n22][..])],
                ),
                "blocked" => window_index(
                    &fixture,
                    &[("lantern", &[n22][..]), ("minimap", &[n22][..])],
                    &[("wayfinder", all), ("lantern", &[n20][..])],
                ),
                other => panic!("unknown capture case {other}"),
            };
            if case != "fresh" {
                next_build();
                let mut held = (*fixture.index_for(NEXT)).clone();
                for project in held.projects.iter_mut().filter(|p| p.mod_id == "wayfinder") {
                    for release in &mut project.releases {
                        release.fits.retain(|b| b != NEXT);
                        release.does_not_fit.push(NEXT.to_owned());
                    }
                }
                let switched = v2::switch_profile_build_v2_with(
                    &fixture.instance,
                    install_target(&fixture.instance).unwrap(),
                    NEXT,
                    Some(BuildChoice::Keep),
                    v2::V2SwitchHooks {
                        switch: SwitchHooks {
                            cancellation: &native_mods::Cancellation::default(),
                            progress: &|_| {},
                            approve: &|_| Ok(SwitchDecision::TurnOff),
                            at_cutoff: &|| Ok(()),
                        },
                        confirm_publisher_change: None,
                        pin_dir: None,
                    },
                    || Ok(Arc::new(held.clone())),
                    &fetch_fixture,
                    &verify_fixture,
                )
                .unwrap();
                assert!(matches!(switched, Switched::Changed(_)));
            }
            shown.check().unwrap();
            fs::write(
                fixture.root.join("index.json"),
                serde_json::to_vec_pretty(&shown).unwrap(),
            )
            .unwrap();
        },
    );
}
