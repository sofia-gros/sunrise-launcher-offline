use super::*;
use crate::instances::Channel::Stable;
use std::cell::RefCell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn fixture() -> String {
    let mut list: Vec<_> = (1..=99)
        .map(|i| {
            serde_json::json!({
                "tag_name": format!("26.3-0.1.{i}"),
                "name": format!("Build {i}"), "body": "Release notes\r\n".repeat(256),
                "published_at": "2026-10-14T12:00:00Z", "draft": i == 99,
                "assets": [{"name": ASSET.unwrap_or("unsupported"),
                    "browser_download_url": "https://example.test/game", "size": 12345}]
            })
        })
        .collect();
    list.push(serde_json::json!({
        "tag_name": "launcher@99.0.0", "name": "Launcher", "body": "Notes",
        "assets": [{"name": LAUNCHER_ASSET.unwrap_or("unsupported"),
            "browser_download_url": "https://example.test/launcher", "size": 6789}]
    }));
    serde_json::to_string(&list).unwrap()
}

fn installed_fixture() -> Vec<String> {
    vec!["26.3-0.1.50".into()]
}

// The former startup paths: each decodes its own response; the update path parses games again.
fn baseline(load: impl Fn() -> Result<Vec<serde_json::Value>, String>) -> (Vec<Release>, Updates) {
    let releases = parse_releases(&load().unwrap(), &[ASSET], is_game_tag);
    let list = load().unwrap();
    let updates = Updates {
        sunrise: sunrise_update(
            &parse_releases(&list, &[ASSET], is_game_tag),
            &installed_fixture(),
            Stable,
        )
        .cloned(),
        ..Updates::default()
    };
    (releases, updates)
}

fn shared(
    load: impl FnOnce() -> Result<Vec<serde_json::Value>, String>,
) -> (Vec<Release>, Updates) {
    let mut releases = None;
    let updates = releases_and_updates_with(
        load,
        installed_fixture,
        Some(Stable),
        || Ok(None),
        |r| releases = Some(r.unwrap()),
    );
    (releases.unwrap(), updates)
}

#[test]
fn one_response_preserves_both_startup_results() {
    let text = fixture();
    let requests = AtomicUsize::new(0);
    let load = || {
        requests.fetch_add(1, Ordering::Relaxed);
        Ok(serde_json::from_str(&text).unwrap())
    };
    let old = baseline(load);
    assert_eq!(requests.swap(0, Ordering::Relaxed), 2);
    let new = shared(load);
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    assert_eq!(
        serde_json::to_value(old).unwrap(),
        serde_json::to_value(new).unwrap()
    );
}

fn newer_launcher() -> Option<Release> {
    launcher_update(&parse_launcher_releases(
        &[serde_json::json!({"tag_name": "v99.0.0", "assets": [{
            "name": LAUNCHER_ASSET.unwrap_or("unsupported"), "browser_download_url": "u", "size": 1
        }]})],
        LAUNCHER_ASSET,
    ))
}

#[test]
fn an_update_offer_requires_a_signed_manifest_for_its_platform() {
    use sunrise_launcher_release_verify::{launcher_message, sha256_hex, sums_file, SigningKey};
    let tag = "v99.0.0";
    let asset = "launcher-fixture.tar.gz";
    let list = [serde_json::json!({"tag_name": tag, "assets": [{"name": asset, "size": 1}]})];
    let (key, _) = SigningKey::generate().unwrap();
    let keys = [key.public()];
    let sums = sums_file(&[(asset.into(), sha256_hex(b"launcher"))]);
    let signature = key.sign_bytes(&launcher_message(tag, sums.as_bytes()));
    let signed = |url: &str| {
        Ok(Some(
            if url.ends_with(".sig") {
                signature.as_bytes()
            } else {
                sums.as_bytes()
            }
            .to_vec(),
        ))
    };
    assert_eq!(
        signed_launcher_offer(&list, asset, &keys, signed)
            .unwrap()
            .unwrap()
            .tag,
        tag
    );
    for invalid in [None, Some(sums.as_bytes().to_vec())] {
        let error = signed_launcher_offer(&list, asset, &keys, |url| {
            Ok(if url.ends_with(".sig") {
                invalid.clone()
            } else {
                Some(sums.as_bytes().to_vec())
            })
        })
        .unwrap_err();
        assert!(matches!(error, LauncherCheckError::Rejected { tag: ref seen, .. } if seen == tag));
        assert_eq!(serde_json::to_value(&error).unwrap(), error.message());
    }
    assert_eq!(
        signed_launcher_offer(&list, asset, &keys, |_| Err("offline".into())).unwrap_err(),
        LauncherCheckError::Unavailable("offline".into()),
    );
    assert!(
        signed_launcher_offer(&list, "different-platform.tar.gz", &keys, |_| panic!(
            "no manifest needed"
        ))
        .unwrap()
        .is_none()
    );
}

#[test]
fn staging_distinguishes_a_missing_manifest_from_an_unavailable_feed() {
    let tag = "v9999.0.0";
    let release = Release {
        tag: tag.into(),
        name: tag.into(),
        body: String::new(),
        date: String::new(),
        time: String::new(),
        published_at: String::new(),
        asset: Some(("https://example.test/launcher".into(), 1)),
        prerelease: false,
        nightly: None,
    };
    let no_progress = |_| panic!("an unverified release must not begin downloading");
    let rejected = stage_update_with(&release, no_progress, |_| Ok(None)).unwrap_err();
    assert!(
        matches!(rejected, StageUpdateError::Check(LauncherCheckError::Rejected { tag: rejected, .. }) if rejected == tag)
    );
    let offline = stage_update_with(&release, no_progress, |_| Err("offline".into())).unwrap_err();
    assert!(matches!(
        &offline,
        StageUpdateError::Check(LauncherCheckError::Unavailable(_))
    ));
    assert_eq!(String::from(offline), "offline");
}

#[test]
fn a_game_list_error_still_checks_the_launcher() {
    for error in [
        "No connection: offline",
        "GitHub's rate limit was reached.",
        "invalid JSON",
        "first page answered 404",
        "first page didn't answer with a release list.",
    ] {
        let mut published = None;
        let updates = releases_and_updates_with(
            || Err(error.into()),
            || panic!("no installed scan after failure"),
            Some(Stable),
            || Ok(newer_launcher()),
            |r| published = Some(r),
        );
        assert_eq!(published.unwrap().unwrap_err(), error);
        assert_eq!(updates.sunrise_error.as_deref(), Some(error));
        assert!(updates.sunrise.is_none());
        assert_eq!(updates.launcher.is_some(), LAUNCHER_ASSET.is_some());
        assert!(updates.launcher_error.is_none());
    }
}

#[test]
fn publish_games_before_the_launcher_lookup() {
    let events = RefCell::new(Vec::new());
    let updates = releases_and_updates_with(
        || {
            Ok(vec![
                serde_json::json!({"tag_name": "26.3-0.1.14", "assets": []}),
            ])
        },
        Vec::new,
        Some(Stable),
        || {
            assert_eq!(events.borrow().first(), Some(&"releases"));
            events.borrow_mut().push("launcher");
            Ok(newer_launcher())
        },
        |r| {
            assert_eq!(r.unwrap()[0].tag, "26.3-0.1.14");
            events.borrow_mut().push("releases");
        },
    );
    assert_eq!(*events.borrow(), ["releases", "launcher"]);
    assert_eq!(updates.launcher.is_some(), LAUNCHER_ASSET.is_some());
}

#[test]
fn a_failed_launcher_list_is_reported_not_up_to_date_and_keeps_the_game_result() {
    let requests = AtomicUsize::new(0);
    for day in [14, 15] {
        let mut published = None;
        let updates = releases_and_updates_with(
            || {
                requests.fetch_add(1, Ordering::Relaxed);
                Ok(vec![
                    serde_json::json!({"tag_name": format!("26.3-0.1.{day}")}),
                ])
            },
            Vec::new,
            Some(Stable),
            || Err(LauncherCheckError::Unavailable("offline".into())),
            |r| published = Some(r.unwrap()),
        );
        assert_eq!(published.unwrap()[0].tag, format!("26.3-0.1.{day}"));
        assert!(updates.launcher.is_none());
        assert_eq!(
            updates
                .launcher_error
                .as_ref()
                .map(LauncherCheckError::message),
            Some("offline")
        );
        assert!(updates.sunrise_error.is_none());
    }
    assert_eq!(requests.load(Ordering::Relaxed), 2);
}

#[test]
fn empty_or_prebuilt_only_fork_has_no_game_or_launcher_update() {
    for list in [
        vec![],
        vec![serde_json::json!({
            "tag_name": "prebuilt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "prerelease": true,
            "assets": [{"name": "sunrise-prebuilt-v3-x86_64-unknown-linux-gnu.tar.gz",
                        "browser_download_url": "https://example.test/sdk", "size": 9}]
        })],
    ] {
        let mut games = None;
        let updates = releases_and_updates_with(
            || Ok(list),
            Vec::new,
            Some(Stable),
            || Ok(None),
            |result| games = Some(result.unwrap()),
        );
        assert!(games.unwrap().is_empty());
        assert!(updates.sunrise.is_none());
        assert!(updates.launcher.is_none());
        assert!(updates.sunrise_error.is_none() && updates.launcher_error.is_none());
    }
}

#[test]
fn each_platform_download_is_bound_to_the_fork_not_the_provider_url() {
    let platforms = [
        (
            "linux",
            "x86_64",
            "sunrise-linux-x86_64.tar.gz",
            "sunrise-launcher-linux-x86_64.tar.gz",
        ),
        (
            "windows",
            "x86_64",
            "sunrise-windows-x64.zip",
            "sunrise-launcher-windows-x64.zip",
        ),
        (
            "macos",
            "aarch64",
            "sunrise-macos-arm64.zip",
            "sunrise-launcher-macos-arm64.zip",
        ),
    ];
    let assets: Vec<_> = platforms.iter().flat_map(|(_, _, game, launcher)| [game, launcher])
        .map(|name| serde_json::json!({"name": name, "size": 9,
            "browser_download_url": "https://github.com/horizon-rust/horizon/releases/download/wrong/file"}))
        .collect();
    // The game's list is the game repository's, the launcher's is its own repository's.
    let game_list = [serde_json::json!({"tag_name": "26.3-0.1.0", "assets": assets})];
    let launcher_list = [serde_json::json!({"tag_name": "v1.0.9", "assets": assets})];
    for (os, arch, game, launcher) in platforms {
        let (game_asset, launcher_asset) = platform_assets(os, arch).unwrap();
        let games = parse_releases(&game_list, &[Some(game_asset)], is_game_tag);
        let launchers = parse_launcher_releases(&launcher_list, Some(launcher_asset));
        for (parsed, repository, tag, expected) in [
            (games, "sunrise-org/sunrise", "26.3-0.1.0", game),
            (
                launchers,
                "sofia-gros/sunrise-launcher-offline",
                "v1.0.9",
                launcher,
            ),
        ] {
            assert_eq!(parsed.len(), 1);
            assert_eq!(
                parsed[0].asset,
                Some((
                    format!("https://github.com/{repository}/releases/download/{tag}/{expected}"),
                    9
                ))
            );
        }
    }
}

#[test]
fn decoy_asset_names_never_replace_the_exact_platform_asset() {
    let asset = "sunrise-linux-x86_64.tar.gz";
    for decoy in [
        format!("../{asset}"),
        format!("prefix-{asset}"),
        format!("{asset}.bak"),
    ] {
        for (tag, encoded) in [
            ("v1/../../x", "v1%2F..%2F..%2Fx"),
            ("v1%?# é", "v1%25%3F%23%20%C3%A9"),
        ] {
            let list = [
                serde_json::json!({ "tag_name": tag, "html_url": "https://evil.test", "assets": [
                    {"name": decoy, "size": 1, "browser_download_url": "https://evil.test/first"},
                    {"name": asset, "size": 2, "browser_download_url": "https://evil.test/second"}
                ]}),
            ];
            let got = parse_releases(&list, &[Some(asset)], |_| true);
            assert_eq!(
                got[0].asset,
                Some((
                    format!(
                        "https://github.com/sunrise-org/sunrise/releases/download/{encoded}/{asset}"
                    ),
                    2
                ))
            );
        }
    }
    for tag in ["../x", "prebuilt-1"] {
        let got = game_releases_with(
            &[
                serde_json::json!({"tag_name": tag, "prerelease": true, "assets": [{"name": asset, "size": 1}]}),
            ],
            &[Some(asset)],
        );
        assert!(got.is_empty());
    }
    let draft = parse_releases(
        &[
            serde_json::json!({"tag_name": "26.3-0.1.0", "draft": true, "assets": [{"name": asset, "size": 1}]}),
        ],
        &[Some(asset)],
        is_game_tag,
    );
    assert!(draft.is_empty());
    assert_eq!(
        crate::signed::asset_url("../../x", "../evil.zip"),
        "https://github.com/sunrise-org/sunrise/releases/download/..%2F..%2Fx/..%2Fevil.zip"
    );
}

#[test]
#[ignore = "local release-loading benchmark; run explicitly with --ignored --nocapture"]
fn benchmark_shared_startup_response() {
    let text = fixture();
    let requests = AtomicUsize::new(0);
    let load = |delay: Duration| {
        requests.fetch_add(1, Ordering::Relaxed);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        serde_json::from_str(black_box(&text)).map_err(|e| e.to_string())
    };
    let old = || baseline(|| load(Duration::ZERO));
    let new = || shared(|| load(Duration::ZERO));
    assert_eq!(
        serde_json::to_value(old()).unwrap(),
        serde_json::to_value(new()).unwrap()
    );
    let concurrent_old = || {
        std::thread::scope(|scope| {
            let releases = scope.spawn(|| {
                parse_releases(
                    &load(Duration::from_millis(5)).unwrap(),
                    &[ASSET],
                    is_game_tag,
                )
            });
            let updates = scope.spawn(|| {
                let list = load(Duration::from_millis(5)).unwrap();
                Updates {
                    sunrise: sunrise_update(
                        &parse_releases(&list, &[ASSET], is_game_tag),
                        &installed_fixture(),
                        Stable,
                    )
                    .cloned(),
                    ..Updates::default()
                }
            });
            (releases.join().unwrap(), updates.join().unwrap())
        })
    };
    let threaded_new = || {
        std::thread::scope(|scope| {
            scope
                .spawn(|| shared(|| load(Duration::from_millis(5))))
                .join()
                .unwrap()
        })
    };
    assert_eq!(
        serde_json::to_value(concurrent_old()).unwrap(),
        serde_json::to_value(threaded_new()).unwrap()
    );
    fn measure<T>(run: impl Fn() -> T, requests: &AtomicUsize) -> (u128, usize) {
        for _ in 0..20 {
            black_box(run());
        }
        requests.store(0, Ordering::Relaxed);
        let mut times: Vec<_> = (0..201)
            .map(|_| {
                let start = Instant::now();
                black_box(run());
                start.elapsed().as_nanos()
            })
            .collect();
        times.sort_unstable();
        (times[100], requests.load(Ordering::Relaxed))
    }
    println!(
        "fixture={} bytes; 100 releases; 20 warmups, 201 samples; median ns",
        text.len()
    );
    for (label, before, after) in [
        (
            "serial zero-delay work",
            measure(old, &requests),
            measure(new, &requests),
        ),
        (
            "concurrent startup, 5ms/request",
            measure(concurrent_old, &requests),
            measure(threaded_new, &requests),
        ),
    ] {
        assert_eq!((before.1, after.1), (402, 201));
        println!(
            "{label}: before={} after={} ratio={:.3}x requests={}->{}",
            before.0,
            after.0,
            before.0 as f64 / after.0 as f64,
            before.1,
            after.1
        );
    }
}

#[test]
fn static_offers_and_launcher_update_do_not_wait_on_sdk_lookups() {
    let Some(static_asset) = ASSET else {
        return;
    };
    if installation::sdk_target(selected_game_architecture(&Settings::default())).is_none() {
        return;
    }
    let events = RefCell::new(Vec::new());
    let list = vec![
        serde_json::json!({
            "tag_name": "26.3-0.1.9",
            "assets": [{"name": sunrise_launcher_sdk::INDEX, "size": 1}]
        }),
        serde_json::json!({
            "tag_name": "26.3-0.1.8",
            "assets": [{"name": static_asset, "size": 1}]
        }),
    ];
    let updates = releases_and_updates_with_sdk(
        || Ok(list),
        Vec::new,
        Some(Stable),
        || {
            assert_eq!(*events.borrow(), ["releases:static"]);
            events.borrow_mut().push("launcher");
            Ok(newer_launcher())
        },
        |r| {
            let releases = r.unwrap();
            let sdk_offered = releases
                .iter()
                .find(|rel| rel.tag == "26.3-0.1.9")
                .and_then(|rel| rel.asset.as_ref())
                .is_some();
            let static_offered = releases
                .iter()
                .find(|rel| rel.tag == "26.3-0.1.8")
                .and_then(|rel| rel.asset.as_ref())
                .is_some();
            assert!(static_offered, "known static build is offered immediately");
            if sdk_offered {
                events.borrow_mut().push("releases:sdk");
            } else {
                events.borrow_mut().push("releases:static");
            }
        },
        |candidates, _target| {
            assert_eq!(*events.borrow(), ["releases:static", "launcher"]);
            events.borrow_mut().push("sdk");
            candidates
                .iter()
                .any(|rel| rel.tag == "26.3-0.1.9" && rel.asset.is_some())
        },
    );
    assert_eq!(
        *events.borrow(),
        ["releases:static", "launcher", "sdk", "releases:sdk"]
    );
    assert_eq!(updates.sunrise.unwrap().tag, "26.3-0.1.9");
    assert_eq!(updates.launcher.is_some(), LAUNCHER_ASSET.is_some());
}

#[test]
fn nightly_bookkeeping_uses_final_catalogue_when_newest_nightly_is_sdk_only() {
    let Some(static_asset) = ASSET else {
        return;
    };
    if installation::sdk_target(selected_game_architecture(&Settings::default())).is_none() {
        return;
    }
    let list = vec![
        serde_json::json!({
            "tag_name": "nightly",
            "name": "Nightly 2026-11-20 · abc1234",
            "prerelease": true,
            "assets": [{"name": sunrise_launcher_sdk::INDEX, "size": 1}]
        }),
        serde_json::json!({
            "tag_name": "26.3-0.1.8",
            "assets": [{"name": static_asset, "size": 1}]
        }),
    ];
    let mut seen = Vec::new();
    let mut final_catalogue = None;
    let _ = releases_and_updates_with_sdk(
        || Ok(list),
        Vec::new,
        Some(crate::instances::Channel::Nightly),
        || Ok(None),
        |r| {
            let releases = r.unwrap();
            let top_nightly = crate::instances::Channel::Nightly
                .builds(&releases)
                .next()
                .map(|rel| rel.tag.clone());
            seen.push(top_nightly);
            final_catalogue = Some(releases);
        },
        |_candidates, _target| true,
    );
    // Pre-verification catalogue omits the unverified SDK-only Nightly, so bookkeeping must wait
    // for the final catalogue (as startup does after `releases_and_updates` returns).
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], None);
    assert_eq!(seen[1].as_deref(), Some("nightly-2026-11-20-abc1234"));
    let final_top = crate::instances::Channel::Nightly
        .builds(final_catalogue.as_ref().unwrap())
        .next()
        .map(|rel| rel.tag.as_str());
    assert_eq!(final_top, Some("nightly-2026-11-20-abc1234"));
}
