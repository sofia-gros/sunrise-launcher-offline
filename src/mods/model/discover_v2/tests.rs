use super::*;
use sunrise_launcher_smod::index_v2::{
    Build, Category, Collection, File, Member, Repository, Revoked, Screenshot,
};

fn hex64(ch: char) -> String {
    std::iter::repeat_n(ch, 64).collect()
}

fn label_build(id: &str) -> String {
    match id {
        "26.3-0.1.0" => "Stable · 0.1.0".to_owned(),
        "nightly-2026-11-20-abcdef0" => "Nightly · Nov 20".to_owned(),
        "dev-2026-11-22-7cfdf81" => "Dev · Nov 22".to_owned(),
        other => other.to_owned(),
    }
}

fn make_release(
    version: &str,
    published: &str,
    sunrise: &str,
    fits: &[&str],
    does_not_fit: &[&str],
    sha_ch: char,
) -> Release {
    Release {
        version: version.to_owned(),
        published: published.to_owned(),
        sunrise: sunrise.to_owned(),
        workflow: format!(
            "sunrise-org/sunrise-mods/.github/workflows/release.yml@refs/tags/release-v{version}"
        ),
        repository: None,
        fits: fits.iter().map(|s| (*s).to_owned()).collect(),
        does_not_fit: does_not_fit.iter().map(|s| (*s).to_owned()).collect(),
        jar_unavailable: Vec::new(),
        dependency_unavailable: Vec::new(),
        file: File {
            url: format!("https://github.com/example/mod/releases/download/v{version}/m.smod"),
            size: 120_000,
            sha256: hex64(sha_ch),
            attestation: format!(
                "https://github.com/example/mod/releases/download/v{version}/m.smod.sigstore.json"
            ),
        },
        dependencies: Vec::new(),
    }
}

fn make_project(
    id: &str,
    slug: &str,
    name: &str,
    summary: &str,
    categories: &[&str],
    releases: Vec<Release>,
) -> Project {
    Project {
        id: id.to_owned(),
        slug: slug.to_owned(),
        mod_id: slug.replace('-', "_"),
        kind: Kind::NativeMod,
        name: name.to_owned(),
        summary: summary.to_owned(),
        description: format!("# {name}\n\nDetailed description of {name}."),
        authors: vec!["Author".to_owned()],
        owners: vec!["author".to_owned()],
        license: "MIT".to_owned(),
        homepage: Some("https://example.com/mod".to_owned()),
        issues: Some("https://github.com/author/mod/issues".to_owned()),
        source: Some("https://github.com/author/mod".to_owned()),
        icon: format!("pages/{id}/{}.png", hex64('a')),
        screenshots: vec![
            Screenshot {
                path: format!("pages/{id}/{}.png", hex64('b')),
                caption: "Overworld minimap and waypoints".to_owned(),
            },
            Screenshot {
                path: format!("pages/{id}/not-hashed.png"),
                caption: "Ignored non-content-addressed shot".to_owned(),
            },
        ],
        categories: categories.iter().map(|s| (*s).to_owned()).collect(),
        repository: Repository {
            id: 42,
            name: format!("author/{slug}"),
            path: ".".to_owned(),
        },
        workflow: "sunrise-org/sunrise-mods/.github/workflows/release.yml".to_owned(),
        releases,
    }
}

fn sample_index(projects: Vec<Project>, revoked: Vec<Revoked>) -> Index {
    Index {
        schema: index_v2::SCHEMA,
        serial: 1,
        generated: "2026-11-22T12:00:00Z".to_owned(),
        builds: vec![
            Build {
                id: "26.3-0.1.0".to_owned(),
                commit: "a".repeat(40),
            },
            Build {
                id: "nightly-2026-11-20-abcdef0".to_owned(),
                commit: format!("abcdef0{}", "0".repeat(33)),
            },
        ],
        categories: vec![
            Category {
                id: "utility".to_owned(),
                icon: "wrench".to_owned(),
                kinds: vec![Kind::Mod, Kind::NativeMod],
            },
            Category {
                id: "worldgen".to_owned(),
                icon: "compass".to_owned(),
                kinds: vec![Kind::Mod, Kind::NativeMod],
            },
        ],
        projects,
        revoked,
        collections: Vec::new(),
    }
}

#[test]
fn exact_fit_never_infers_from_sunrise_or_semver_and_picks_newest_fitting() {
    let build = "26.3-0.1.0";
    // Project A: 2.0.0 does NOT list build in fits even though sunrise == build;
    // 1.2.0 and 1.0.0 list build in fits -> newest fitting must be 1.2.0.
    let p_a = make_project(
        "Ab3dE9xQ",
        "wayfinder",
        "Wayfinder",
        "Minimap with waypoints",
        &["utility"],
        vec![
            make_release("1.0.0", "2026-11-10", build, &[build], &[], '1'),
            make_release("2.0.0", "2026-11-22", build, &[], &[build], '2'),
            make_release("1.2.0", "2026-11-20", build, &[build], &[], '3'),
        ],
    );
    // Project B: sunrise == build, but fits is empty and does_not_fit is empty -> Unknown, NOT Fits!
    let p_b = make_project(
        "Cd5fG7hJ",
        "unchecked",
        "Unchecked Mod",
        "Waiting for fit check",
        &["utility"],
        vec![make_release("1.0.0", "2026-11-21", build, &[], &[], '4')],
    );
    // Project C: explicitly listed in does_not_fit -> DoesNotFit.
    let p_c = make_project(
        "Ef7gH9iK",
        "incompatible",
        "Incompatible Mod",
        "Needs newer API",
        &["worldgen"],
        vec![make_release(
            "1.0.0",
            "2026-11-21",
            "nightly-2026-11-20-abcdef0",
            &["nightly-2026-11-20-abcdef0"],
            &[build],
            '5',
        )],
    );

    let index = sample_index(vec![p_a, p_b, p_c], Vec::new());
    let list = cards(&index, Some(build), &label_build, &LocalFits::default());
    assert_eq!(list.len(), 3);

    // Wayfinder fits at newest fitting release 1.2.0 (skipping non-fitting 2.0.0).
    assert_eq!(list[0].fit.fitting_version(), Some("1.2.0"));
    assert!(list[0].fit.fits());
    assert_eq!(list[0].fit.reason(), None);

    // Unchecked Mod is NOT inferred from sunrise; shows "Not checked for ... yet".
    assert!(!list[1].fit.fits());
    assert_eq!(
        list[1].fit.reason().as_deref(),
        Some("Not checked for Stable · 0.1.0 yet")
    );

    // Incompatible Mod is in does_not_fit; shows "Doesn't fit ...".
    assert!(!list[2].fit.fits());
    assert_eq!(
        list[2].fit.reason().as_deref(),
        Some("Doesn't fit Stable · 0.1.0")
    );

    // Default Discover (show_all = false) shows ONLY fitting mods.
    let default_filters = V2Filters::default();
    let shown = discover(&list, &default_filters);
    assert_eq!(shown, vec![0]);
    assert_eq!(hidden_other_build_count(&list, &default_filters), 2);

    // Show all includes the non-fitting mods with their exact reasons.
    let all_filters = V2Filters {
        show_all: true,
        ..V2Filters::default()
    };
    let shown_all = discover(&list, &all_filters);
    assert_eq!(shown_all.len(), 3);
}

#[test]
fn a_release_lists_every_build_with_the_index_verdict_first_and_local_answers_in_gaps() {
    let night = "nightly-2026-11-20-abcdef0";
    let stable = "26.3-0.1.0";
    let gone = "dev-2026-11-01-1111111";
    let project = make_project(
        "Ab3dE9xQ",
        "enchanting",
        "Dis-Enchanting Table",
        "Takes enchantments off",
        &["utility"],
        vec![
            make_release("5.1.1", "2026-11-24", night, &[night, gone], &[], '1'),
            make_release("5.1.0", "2026-11-22", night, &[], &[], '2'),
        ],
    );
    let mut index = sample_index(vec![project], Vec::new());
    let stable_commit = index.build(stable).unwrap().commit.clone();
    let night_commit = index.build(night).unwrap().commit.clone();
    // This computer found both releases not to fit either build.
    let kept = |commit: &str| {
        if commit != stable_commit && commit != night_commit {
            return local_fit::Found::new();
        }
        local_fit::Found::from([
            (hex64('1'), local_fit::Local::DoesNotFit),
            (hex64('2'), local_fit::Local::DoesNotFit),
        ])
    };

    let newest = release_builds(&index, "Ab3dE9xQ", "5.1.1", &kept).unwrap();
    assert_eq!(newest.made_for, night);
    assert_eq!(
        newest.builds,
        [
            (stable.to_owned(), BuildFit::DoesNotFitHere),
            (night.to_owned(), BuildFit::Fits),
            (gone.to_owned(), BuildFit::Fits),
        ],
        "the index's own verdict wins over this computer's"
    );
    let older = release_builds(&index, "Ab3dE9xQ", "5.1.0", &|_| local_fit::Found::new());
    assert_eq!(
        older.unwrap().builds,
        [
            (stable.to_owned(), BuildFit::NotChecked),
            (night.to_owned(), BuildFit::NotChecked),
        ]
    );

    index.projects[0].releases[1]
        .does_not_fit
        .push(stable.into());
    let judged = release_builds(&index, "Ab3dE9xQ", "5.1.0", &kept).unwrap();
    assert_eq!(judged.builds[0], (stable.to_owned(), BuildFit::DoesNotFit));
    assert!(release_builds(&index, "Ab3dE9xQ", "9.9.9", &kept).is_none());
}

#[test]
fn relevance_ranks_exact_name_ahead_of_summary_and_description() {
    let build = "26.3-0.1.0";
    let summary_hit = make_project(
        "Aa11Bb22",
        "compass-plus",
        "Compass Plus",
        "Adds a wayfinder overlay to the compass",
        &["utility"],
        vec![make_release(
            "1.0.0",
            "2026-11-22",
            build,
            &[build],
            &[],
            '1',
        )],
    );
    let name_hit = make_project(
        "Cc33Dd44",
        "wayfinder",
        "Wayfinder",
        "Minimap with waypoints",
        &["utility"],
        vec![make_release(
            "1.0.0",
            "2026-11-18",
            build,
            &[build],
            &[],
            '2',
        )],
    );
    let index = sample_index(vec![summary_hit, name_hit], Vec::new());
    let list = cards(&index, Some(build), &label_build, &LocalFits::default());
    let shown = discover(
        &list,
        &V2Filters {
            text: "wayfinder".to_owned(),
            ..V2Filters::default()
        },
    );
    assert_eq!(shown.len(), 2);
    assert_eq!(list[shown[0]].name, "Wayfinder");
    assert_eq!(list[shown[1]].name, "Compass Plus");
}

#[test]
fn takedowns_block_whole_project_and_single_release_digests() {
    let build = "26.3-0.1.0";
    let whole_revoked = make_project(
        "Rr11Rr22",
        "bad-mod",
        "Bad Mod",
        "Taken down",
        &["utility"],
        vec![make_release(
            "1.0.0",
            "2026-11-20",
            build,
            &[build],
            &[],
            '9',
        )],
    );
    let one_release_revoked = make_project(
        "Ss33Ss44",
        "good-mod",
        "Good Mod",
        "Has a clean older release",
        &["utility"],
        vec![
            make_release("2.0.0", "2026-11-22", build, &[build], &[], '7'),
            make_release("1.0.0", "2026-11-18", build, &[build], &[], '8'),
        ],
    );
    let index = sample_index(
        vec![whole_revoked, one_release_revoked],
        vec![
            Revoked {
                project: "Rr11Rr22".to_owned(),
                sha256: None,
                reason: "Malware".to_owned(),
                date: "2026-11-22".to_owned(),
            },
            Revoked {
                project: "Ss33Ss44".to_owned(),
                sha256: Some(hex64('7')),
                reason: "Broken build".to_owned(),
                date: "2026-11-22".to_owned(),
            },
        ],
    );
    let list = cards(&index, Some(build), &label_build, &LocalFits::default());
    assert!(!list[0].fit.fits());
    assert_eq!(list[0].fit.reason().as_deref(), Some("Taken down: Malware"));
    // 2.0.0 is revoked by sha256, so Good Mod falls back to fitting 1.0.0.
    assert_eq!(list[1].fit.fitting_version(), Some("1.0.0"));
}

#[test]
fn v2_page_keeps_captioned_content_addressed_screenshots_and_publisher() {
    let build = "26.3-0.1.0";
    let proj = make_project(
        "Ab3dE9xQ",
        "wayfinder",
        "Wayfinder",
        "Clean minimap with waypoints",
        &["utility", "worldgen"],
        vec![
            make_release("1.2.0", "2026-11-22", build, &[build], &[], '1'),
            make_release(
                "1.1.0",
                "2026-11-18",
                "nightly-2026-11-20-abcdef0",
                &["nightly-2026-11-20-abcdef0"],
                &[build],
                '2',
            ),
        ],
    );
    let index = sample_index(vec![proj], Vec::new());
    let p = page(
        &index,
        "Ab3dE9xQ",
        Some(build),
        &label_build,
        &LocalFits::default(),
    )
    .unwrap();
    assert_eq!(p.repository_name, "author/wayfinder");
    assert_eq!(p.categories, vec!["utility", "worldgen"]);
    assert_eq!(p.summary, "Clean minimap with waypoints");
    assert!(p.icon.is_some());
    // Non-content-addressed screenshot is dropped; content-addressed one keeps its caption.
    assert_eq!(p.screenshots.len(), 1);
    assert_eq!(p.screenshots[0].caption, "Overworld minimap and waypoints");
    assert_eq!(p.fitting_versions.len(), 1);
    assert_eq!(p.fitting_versions[0].version, "1.2.0");
    assert_eq!(p.other_versions.len(), 1);
    assert_eq!(
        p.other_versions[0].verdict.reason().as_deref(),
        Some("Doesn't fit Stable · 0.1.0")
    );
}

#[test]
fn v2_page_lists_the_installed_release_dependencies_with_their_providers() {
    let build = "26.3-0.1.0";
    let dep =
        |mod_id: &str, version: &str, project: Option<&str>, required: bool| index_v2::Dependency {
            mod_id: mod_id.to_owned(),
            version: semver::VersionReq::parse(version).unwrap(),
            project: project.map(str::to_owned),
            required,
        };
    let mut fitting = make_release("1.0.0", "2026-11-10", build, &[build], &[], '1');
    fitting.dependencies = vec![
        dep("map_icons", "*", None, false),
        dep("core_lib", "^2.1", Some("CoreLib1"), true),
        dep("render_api", "^1", None, true),
        dep("ghost", "^1", None, true),
        dep("other_lib", "^1", Some("CoreLib1"), true),
    ];
    let mut newer = make_release("2.0.0", "2026-11-22", build, &[], &[build], '2');
    newer.dependencies = vec![dep("only_in_newer", "^1", None, true)];
    let index = sample_index(
        vec![
            make_project(
                "Ab3dE9xQ",
                "wayfinder",
                "Wayfinder",
                "",
                &[],
                vec![fitting, newer],
            ),
            make_project("CoreLib1", "core-lib", "Core Lib", "", &[], Vec::new()),
            make_project("Render01", "render-api", "Render A", "", &[], Vec::new()),
            make_project("Render02", "render-api", "Render B", "", &[], Vec::new()),
            make_project("MapIcon1", "map-icons", "Map Icons", "", &[], Vec::new()),
        ],
        Vec::new(),
    );
    let p = page(
        &index,
        "Ab3dE9xQ",
        Some(build),
        &label_build,
        &LocalFits::default(),
    )
    .unwrap();
    let rows: Vec<_> = p
        .dependencies
        .iter()
        .map(|d| {
            (
                d.mod_id.as_str(),
                d.range.as_deref(),
                d.required,
                &d.provider,
            )
        })
        .collect();
    let listed = |id: &str, name: &str| V2Provider::Listed {
        id: id.to_owned(),
        name: name.to_owned(),
        icon: Some(format!("pages/{id}/{}.png", hex64('a'))),
    };
    assert_eq!(
        rows,
        vec![
            (
                "core_lib",
                Some("^2.1"),
                true,
                &listed("CoreLib1", "Core Lib")
            ),
            ("render_api", Some("^1"), true, &V2Provider::Several(2)),
            ("ghost", Some("^1"), true, &V2Provider::Missing),
            // Pinned to a project that provides a different mod: nothing to link to.
            ("other_lib", Some("^1"), true, &V2Provider::Missing),
            ("map_icons", None, false, &listed("MapIcon1", "Map Icons")),
        ]
    );
}

#[test]
fn a_build_outside_the_index_builds_is_unlisted_and_nothing_fits_it() {
    let listed = "26.3-0.1.0";
    let unlisted = "nightly-2026-10-01-0000000";
    let project = make_project(
        "Ab3dE9xQ",
        "wayfinder",
        "Wayfinder",
        "Minimap with waypoints",
        &["utility"],
        vec![make_release(
            "1.0.0",
            "2026-11-10",
            listed,
            &[listed],
            &[],
            '1',
        )],
    );
    let index = sample_index(vec![project], Vec::new());
    assert!(build_listed(&index, Some(listed)));
    assert!(build_listed(&index, None));
    assert!(!build_listed(&index, Some(unlisted)));
    let cards = cards(&index, Some(unlisted), &label_build, &LocalFits::default());
    assert!(discover(&cards, &V2Filters::default()).is_empty());
    let all = V2Filters {
        show_all: true,
        ..V2Filters::default()
    };
    assert_eq!(discover(&cards, &all).len(), 1);
}

#[test]
fn a_collection_picks_its_default_members_that_fit_and_installs_only_the_rest() {
    let build = "26.3-0.1.0";
    let project = |id: &str, slug: &str, category: &str, fits: &[&str], ch| {
        let not = if fits.is_empty() { vec![build] } else { vec![] };
        make_project(
            id,
            slug,
            slug,
            "A block set.",
            &[category],
            vec![make_release("1.0.0", "2026-11-10", build, fits, &not, ch)],
        )
    };
    let mut index = sample_index(
        vec![
            project("Ab3dE9xQ", "fences", "utility", &[build], '1'),
            project("Bc4eF0yR", "doors", "utility", &[build], '2'),
            project("Cd5fG1zS", "roofs", "decoration", &[], '3'),
        ],
        vec![],
    );
    let member = |project: &str, default| Member {
        project: project.to_owned(),
        default,
    };
    index.collections = vec![Collection {
        id: "Qm4tZ81c".to_owned(),
        name: "Macaw's Builds".to_owned(),
        summary: "Three block sets.".to_owned(),
        description: String::new(),
        publisher: "author".to_owned(),
        icon: format!("pages/Qm4tZ81c/{}.png", hex64('9')),
        members: vec![
            member("Ab3dE9xQ", true),
            member("Bc4eF0yR", false),
            member("Cd5fG1zS", true),
        ],
    }];
    let cards = cards(&index, Some(build), &label_build, &LocalFits::default());
    let found = collections(&index, &cards);
    let collection = &found[0];
    assert_eq!(collection.fitting(&cards), 2);
    // Doors is off by default, and Roofs has no release for this build.
    let picks = collection.initial_picks(&cards);
    assert_eq!(picks, BTreeSet::from(["Ab3dE9xQ".to_owned()]));
    let all = ["Ab3dE9xQ", "Bc4eF0yR", "Cd5fG1zS"]
        .map(str::to_owned)
        .into();
    assert_eq!(
        collection.to_install(&cards, &all, &|id| id == "Ab3dE9xQ"),
        ["Bc4eF0yR"]
    );
    // Install names how many it takes, and why the rest wait: Roofs doesn't fit, and Doors is
    // optional until it is installed.
    let (label, why) = collection.install_label(&cards, 1, &|_| false);
    assert_eq!(label, "Install 1");
    assert_eq!(
        why.as_deref(),
        Some("Installs 1 of its 3 mods: some don't fit this build yet and some are optional. Open it to choose.")
    );
    let (_, why) = collection.install_label(&cards, 1, &|id| id == "Bc4eF0yR");
    assert_eq!(
        why.as_deref(),
        Some("Installs 1 of its 3 mods: the rest don't fit this build yet.")
    );

    let filters = |text: &str, category: Option<&str>, show_all| V2Filters {
        text: text.to_owned(),
        category: category.map(str::to_owned),
        show_all,
        ..V2Filters::default()
    };
    let shown_with = |text, category, show_all| {
        discover_collections(&found, &cards, &filters(text, category, show_all))
    };
    let shown = |text, category| shown_with(text, category, false);
    assert_eq!(shown("macaw", None), [0]);
    assert_eq!(
        shown("doors", None),
        [0],
        "a member's name finds its collection"
    );
    assert!(shown("lantern", None).is_empty());
    // Under a category the collection stands in for its members in it, as when browsing.
    assert_eq!(shown("", Some("utility")), [0]);
    // Roofs, its only member in Decoration, doesn't fit: there it shows only with Show all.
    assert!(shown("", Some("decoration")).is_empty());
    assert_eq!(shown_with("", Some("decoration"), true), [0]);
    assert!(shown("", Some("mobs")).is_empty());
    // A member's name finds the collection only where that member would be listed.
    assert!(shown_with("doors", Some("decoration"), true).is_empty());
    assert!(shown("roofs", None).is_empty());
    assert_eq!(shown_with("roofs", None, true), [0]);
    assert_eq!(shown_with("roofs", Some("decoration"), true), [0]);

    // Browsing shows the collection's card in place of its members, Show all included; a
    // search finds a member.
    let mods = |text: &str, show_all| {
        let filters = V2Filters {
            text: text.to_owned(),
            show_all,
            ..V2Filters::default()
        };
        let mut shown = discover(&cards, &filters);
        fold_members(&mut shown, &found, &filters);
        shown.len()
    };
    assert_eq!(mods("", false), 0);
    assert_eq!(mods("", true), 0);
    assert_eq!(mods("doors", false), 1);
    // Members stay folded under a category too: only their collection shows there.
    let in_category = V2Filters {
        category: Some("utility".to_owned()),
        ..V2Filters::default()
    };
    let mut shown = discover(&cards, &in_category);
    fold_members(&mut shown, &found, &in_category);
    assert!(shown.is_empty());
}

#[test]
fn a_local_check_fills_only_cards_the_index_says_nothing_of() {
    let stable = "26.3-0.1.0";
    let nightly = "nightly-2026-11-20-abcdef0";
    let project = |id: &str, fits: &[&str], not: &[&str], ch| {
        make_project(
            id,
            id,
            id,
            "summary",
            &["utility"],
            vec![make_release("1.0.0", "2026-11-10", stable, fits, not, ch)],
        )
    };
    let index = sample_index(
        vec![
            project("AAAAAAAA", &[], &[], '1'),
            project("BBBBBBBB", &[], &[], '2'),
            project("CCCCCCCC", &[], &[nightly], '3'),
            project("DDDDDDDD", &[], &[], '4'),
        ],
        Vec::new(),
    );
    let local = LocalFits {
        found: local_fit::Found::from([
            (hex64('1'), local_fit::Local::Fits),
            (hex64('2'), local_fit::Local::DoesNotFit),
            // The index's own verdict wins over this.
            (hex64('3'), local_fit::Local::Fits),
        ]),
        checking: BTreeSet::from([hex64('4')]),
        jar_cache: None,
    };
    let fit = |id: &str| {
        cards(&index, Some(nightly), &label_build, &local)
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
            .fit
    };
    let a = fit("AAAAAAAA");
    assert_eq!(
        a,
        CardFit::FitsHere {
            version: "1.0.0".into()
        }
    );
    assert!(a.fits() && !a.faded());
    assert!(matches!(fit("BBBBBBBB"), CardFit::DoesNotFitHere { .. }));
    assert!(matches!(fit("CCCCCCCC"), CardFit::DoesNotFit { .. }));
    let d = fit("DDDDDDDD");
    assert!(matches!(d, CardFit::Checking { .. }));
    assert!(
        !d.fits() && !d.faded(),
        "a card being checked isn't drawn as unfit"
    );

    // On a build the index doesn't list, every card takes this computer's answer, the one
    // the index judged only for another build included.
    let unlisted = |id: &str| {
        cards(&index, Some("dev-2026-11-30-1234567"), &label_build, &local)
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
            .fit
    };
    assert!(matches!(unlisted("AAAAAAAA"), CardFit::FitsHere { .. }));
    assert!(matches!(
        unlisted("BBBBBBBB"),
        CardFit::DoesNotFitHere { .. }
    ));
    assert!(matches!(unlisted("CCCCCCCC"), CardFit::FitsHere { .. }));
    assert!(matches!(unlisted("DDDDDDDD"), CardFit::Checking { .. }));
}

#[test]
fn v2_a_release_whose_original_jar_modrinth_no_longer_serves_is_not_installable_unless_cached() {
    let build = "26.3-0.1.0";
    let gone = JarRef::new("macaws_bridges".into(), "3.0.0".into(), "ab".repeat(64)).unwrap();
    let other = JarRef::new("other_lib".into(), "1.2.0".into(), "cd".repeat(64)).unwrap();
    let mut marked = make_release("1.0.0", "2026-11-22", build, &[build], &[], '1');
    marked.jar_unavailable = vec![gone.clone(), other];
    let index = sample_index(
        vec![make_project(
            "Ab3dE9xQ",
            "wayfinder",
            "Wayfinder",
            "summary",
            &["utility"],
            vec![marked],
        )],
        Vec::new(),
    );
    let lines = "The original file macaws_bridges 3.0.0 is no longer on Modrinth.\n\
                 The original file other_lib 1.2.0 is no longer on Modrinth.";

    let empty = tempfile::tempdir().unwrap();
    let none = LocalFits {
        jar_cache: Some(empty.path().to_owned()),
        ..LocalFits::default()
    };
    let card = cards(&index, Some(build), &label_build, &none).remove(0);
    assert!(!card.fit.fits() && card.fit.faded());
    assert_eq!(card.fit.reason().as_deref(), Some(lines));
    let p = page(&index, "Ab3dE9xQ", Some(build), &label_build, &none).unwrap();
    assert!(p.will_install.is_none() && p.fitting_versions.is_empty());
    assert_eq!(p.other_versions[0].verdict.reason().as_deref(), Some(lines));
    assert_eq!(p.fit.reason().as_deref(), Some(lines));

    // Once every jar is cached, the release shows nothing and installs.
    let cached = tempfile::tempdir().unwrap();
    for jar in &index.projects[0].releases[0].jar_unavailable {
        std::fs::write(jar.cache_path(cached.path()), b"jar").unwrap();
    }
    let have = LocalFits {
        jar_cache: Some(cached.path().to_owned()),
        ..LocalFits::default()
    };
    let card = cards(&index, Some(build), &label_build, &have).remove(0);
    assert_eq!(
        card.fit,
        CardFit::Fits {
            version: "1.0.0".into()
        }
    );
    let p = page(&index, "Ab3dE9xQ", Some(build), &label_build, &have).unwrap();
    assert_eq!(p.will_install.unwrap().verdict, ReleaseVerdict::Fits);
}

/// A project whose only release fits `build` but lists `needs` as having no release for it.
fn dependant(id: &str, slug: &str, build: &str, needs: &[&str], sha_ch: char) -> Project {
    let mut release = make_release("1.0.0", "2026-11-10", build, &[build], &[], sha_ch);
    release.dependency_unavailable = needs
        .iter()
        .map(|m| index_v2::DependencyUnavailable {
            build: build.to_owned(),
            mod_id: (*m).to_owned(),
        })
        .collect();
    make_project(id, slug, slug, "A block set.", &[], vec![release])
}

#[test]
fn a_collection_whose_base_has_no_release_for_the_build_installs_the_others_and_greys_the_dependants(
) {
    let build = "26.3-0.1.0";
    let mut index = sample_index(
        vec![
            // The base has no release for the build at all.
            make_project(
                "Ab3dE9xQ",
                "macaws_lib",
                "Macaws Lib",
                "",
                &[],
                vec![make_release(
                    "1.0.0",
                    "2026-11-10",
                    build,
                    &[],
                    &[build],
                    '1',
                )],
            ),
            dependant("Bc4eF0yR", "doors", build, &["macaws_lib"], '2'),
            dependant("Cd5fG1zS", "roofs", build, &["macaws_lib"], '3'),
            dependant("De6gH2aT", "fences", build, &[], '4'),
        ],
        vec![],
    );
    let member = |project: &str| Member {
        project: project.to_owned(),
        default: true,
    };
    index.collections = vec![Collection {
        id: "Qm4tZ81c".to_owned(),
        name: "Macaw's Builds".to_owned(),
        summary: String::new(),
        description: String::new(),
        publisher: "author".to_owned(),
        icon: format!("pages/Qm4tZ81c/{}.png", hex64('9')),
        members: ["Ab3dE9xQ", "Bc4eF0yR", "Cd5fG1zS", "De6gH2aT"]
            .map(member)
            .into(),
    }];
    let cards = cards(&index, Some(build), &label_build, &LocalFits::default());
    let found = collections(&index, &cards);
    let collection = &found[0];

    // Only the member with nothing missing is picked and installed.
    assert_eq!(collection.fitting(&cards), 1);
    assert_eq!(
        collection.initial_picks(&cards),
        BTreeSet::from(["De6gH2aT".to_owned()])
    );
    let all = ["Ab3dE9xQ", "Bc4eF0yR", "Cd5fG1zS", "De6gH2aT"]
        .map(str::to_owned)
        .into();
    assert_eq!(
        collection.to_install(&cards, &all, &|_| false),
        ["De6gH2aT"]
    );
    assert_eq!(
        collection.install_label(&cards, 1, &|_| false),
        (
            "Install 1".to_owned(),
            Some("Installs 1 of its 4 mods: the rest don't fit this build yet.".to_owned())
        )
    );

    // The dependants are greyed with the reason; the one with nothing missing shows none.
    let reason = Some("Needs macaws_lib, which has no release for Sunrise Stable · 0.1.0.");
    for dependants in [&cards[1], &cards[2]] {
        assert!(dependants.fit.faded());
        assert_eq!(dependants.fit.reason().as_deref(), reason);
    }
    assert!(!cards[3].fit.faded() && cards[3].fit.reason().is_none());
}

#[test]
fn the_mod_page_shows_the_reason_when_a_required_dependency_has_no_release_for_the_build() {
    let build = "26.3-0.1.0";
    let mut doors = dependant("Bc4eF0yR", "doors", build, &["macaws_lib"], '2');
    doors.releases[0].dependencies = vec![index_v2::Dependency {
        mod_id: "macaws_lib".to_owned(),
        version: semver::VersionReq::STAR,
        project: None,
        required: true,
    }];
    let index = sample_index(vec![doors], vec![]);
    let p = page(
        &index,
        "Bc4eF0yR",
        Some(build),
        &label_build,
        &LocalFits::default(),
    )
    .unwrap();
    let reason = "Needs macaws_lib, which has no release for Sunrise Stable · 0.1.0.";
    assert!(!p.fit.fits());
    assert_eq!(p.fit.reason().as_deref(), Some(reason));
    // Install is unavailable: no release is picked, and the release row says why.
    assert!(p.will_install.is_none() && p.fitting_versions.is_empty());
    assert_eq!(
        p.other_versions[0].verdict.reason().as_deref(),
        Some(reason)
    );
    // The dependency row says the provider has no release for this build.
    assert_eq!(
        p.dependencies[0].no_release_for.as_deref(),
        Some("Stable · 0.1.0")
    );

    // On a build the release isn't listed for, nothing is marked.
    let other = "nightly-2026-11-20-abcdef0";
    let mut fits_other = index.clone();
    fits_other.projects[0].releases[0]
        .fits
        .push(other.to_owned());
    let p = page(
        &fits_other,
        "Bc4eF0yR",
        Some(other),
        &label_build,
        &LocalFits::default(),
    )
    .unwrap();
    assert!(p.fit.fits());
    assert!(p.dependencies[0].no_release_for.is_none());
}
