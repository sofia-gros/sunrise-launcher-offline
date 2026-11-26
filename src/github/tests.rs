use super::*;
use std::cell::RefCell;

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("sunrise-github-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

const T0: u64 = 1_800_000_000;
const LATER: u64 = T0 + FRESH_SECS;

const RELEASES: &str = "https://api.github.com/repos/sunrise-org/sunrise/releases?per_page=100";

fn list(tag: &str) -> serde_json::Value {
    serde_json::json!([{ "tag_name": tag }])
}

/// A single JSON read with a mirror configured, as `SUNRISE_GITHUB_MIRROR` would.
fn json_with(
    url: &str,
    cache: &Path,
    now: u64,
    github: impl Fn(&str, Option<&str>) -> Answer,
    mirror: impl Fn(&str) -> Answer,
) -> Result<Option<serde_json::Value>, String> {
    json_with_policy(
        url,
        cache,
        now,
        "https://mcp.example/github/public",
        CachePolicy::PreferCache,
        github,
        mirror,
    )
}

fn json_with_policy(
    url: &str,
    cache: &Path,
    now: u64,
    mirror_base: &str,
    policy: CachePolicy,
    github: impl Fn(&str, Option<&str>) -> Answer,
    mirror: impl Fn(&str) -> Answer,
) -> Result<Option<serde_json::Value>, String> {
    Ok(page_with(
        Request {
            url,
            policy,
            paginated: false,
        },
        cache,
        now,
        mirror_base,
        github,
        mirror,
    )?
    .map(|page| page.body))
}

fn fresh(body: serde_json::Value, etag: Option<String>) -> Answer {
    Answer::Fresh(
        Page {
            body,
            next: Some(false),
        },
        etag,
    )
}

fn no_mirror(_: &str) -> Answer {
    panic!("the mirror is only asked when GitHub can't answer")
}

#[test]
fn mirror_serves_only_sunrises_repositories() {
    let base = "https://mcp.example/github/public";
    assert_eq!(
        mirror_url_with(RELEASES, base).as_deref(),
        Some("https://mcp.example/github/public/repos/sunrise-org/sunrise/releases?per_page=100")
    );
    assert_eq!(
        mirror_url_with(
            "https://api.github.com/repos/sunrise-org/sunrise/releases/tags/launcher@0.2.33",
            &format!("{base}/")
        )
        .as_deref(),
        Some("https://mcp.example/github/public/repos/sunrise-org/sunrise/releases/tags/launcher@0.2.33")
    );
    // The launcher's own repository only in a development build: a shipped launcher learns of its own updates from GitHub.
    assert_eq!(
        mirror_url_with("https://api.github.com/repos/sofia-gros/sunrise-launcher-offline/releases?per_page=100", base).as_deref(),
        cfg!(debug_assertions).then_some("https://mcp.example/github/public/repos/sofia-gros/sunrise-launcher-offline/releases?per_page=100")
    );
    assert_eq!(
        mirror_url_with("https://api.github.com/repos/someone/mod/commits/abc", base),
        None
    );
    assert_eq!(
        mirror_url_with("https://github.com/sunrise-org/sunrise/releases", base),
        None
    );
    for url in [
        "https://api.github.com/repos/horizon-rust/horizon/releases",
        "https://api.github.com/repos/arismoko/horizon-extra/releases",
    ] {
        assert_eq!(mirror_url_with(url, base), None);
    }
    assert_eq!(
        mirror_url_with(RELEASES, ""),
        None,
        "an empty mirror is off"
    );
}

#[test]
fn a_saved_upstream_list_cannot_answer_a_fork_check() {
    let cache = scratch("repository-scope");
    json_with(
        "https://api.github.com/repos/horizon-rust/horizon/releases?per_page=100",
        &cache,
        T0,
        |_, _| fresh(list("upstream"), None),
        no_mirror,
    )
    .unwrap();
    let absent = json_with(RELEASES, &cache, T0, |_, _| Answer::NotFound, no_mirror).unwrap();
    assert_eq!(absent, None);
    let error = json_with(
        RELEASES,
        &cache,
        T0,
        |_, _| Answer::Failed("offline".into()),
        |_| Answer::Failed("offline".into()),
    )
    .unwrap_err();
    assert!(error.contains("offline"));
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn rate_limit_is_429_or_403_with_githubs_headers() {
    let none = |_: &str| None;
    assert!(rate_limited(429, none));
    assert!(!rate_limited(403, none), "a plain 403 is a refusal");
    assert!(rate_limited(403, |h| (h == "x-ratelimit-remaining").then(|| "0".into())));
    assert!(!rate_limited(403, |h| (h == "x-ratelimit-remaining").then(|| "12".into())));
    assert!(rate_limited(403, |h| (h == "retry-after").then(|| "60".into())));
    assert!(!rate_limited(500, |h| (h == "retry-after").then(|| "60".into())));
}

#[test]
fn etag_is_kept_and_a_304_reads_the_saved_copy() {
    let cache = scratch("etag");
    let sent = RefCell::new(Vec::new());
    let first = json_with(
        RELEASES,
        &cache,
        T0,
        |_, etag| {
            sent.borrow_mut().push(etag.map(str::to_owned));
            fresh(list("nightly"), Some("\"abc\"".into()))
        },
        no_mirror,
    );
    assert_eq!(first.unwrap(), Some(list("nightly")));
    // Started again within FRESH_SECS: the saved list, without asking.
    let again = json_with(
        RELEASES,
        &cache,
        T0 + FRESH_SECS - 1,
        |_, _| panic!("a fresh copy is used without asking"),
        no_mirror,
    );
    assert_eq!(again.unwrap(), Some(list("nightly")));
    // Later: a conditional request; a 304 gives the saved list and starts a new fresh window.
    let second = json_with(
        RELEASES,
        &cache,
        LATER,
        |_, etag| {
            sent.borrow_mut().push(etag.map(str::to_owned));
            Answer::NotModified(None)
        },
        no_mirror,
    );
    assert_eq!(second.unwrap(), Some(list("nightly")));
    assert_eq!(*sent.borrow(), [None, Some("\"abc\"".to_owned())]);
    let third = json_with(
        RELEASES,
        &cache,
        LATER + FRESH_SECS - 1,
        |_, _| panic!("confirmed by the 304"),
        no_mirror,
    );
    assert_eq!(third.unwrap(), Some(list("nightly")));
    // A clock set back doesn't make the copy fresh forever.
    let back = json_with(
        RELEASES,
        &cache,
        T0 - 1,
        |_, _| fresh(list("v9"), None),
        no_mirror,
    );
    assert_eq!(back.unwrap(), Some(list("v9")));
    // Another URL doesn't read this one's copy.
    let other = "https://api.github.com/repos/sunrise-org/sunrise/releases/latest";
    let etag_sent = RefCell::new(Some(String::new()));
    let _ = json_with(
        other,
        &cache,
        T0,
        |_, etag| {
            *etag_sent.borrow_mut() = etag.map(str::to_owned);
            fresh(list("v1"), None)
        },
        no_mirror,
    );
    assert_eq!(*etag_sent.borrow(), None);
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn rate_limited_reads_go_through_the_mirror() {
    let cache = scratch("mirror");
    let asked = RefCell::new(None);
    let got = json_with(
        RELEASES,
        &cache,
        T0,
        |_, _| Answer::RateLimited,
        |url| {
            *asked.borrow_mut() = Some(url.to_owned());
            fresh(list("nightly"), None)
        },
    );
    assert_eq!(got.unwrap(), Some(list("nightly")));
    assert!(asked
        .borrow()
        .as_deref()
        .unwrap()
        .ends_with("/github/public/repos/sunrise-org/sunrise/releases?per_page=100"));
    // The mirror's answer is kept like GitHub's.
    let kept = json_with(RELEASES, &cache, T0 + 1, |_, _| panic!("fresh"), no_mirror);
    assert_eq!(kept.unwrap(), Some(list("nightly")));
    // No connection to GitHub (blocked, DNS): the mirror too.
    let got = json_with(
        RELEASES,
        &cache,
        LATER,
        |_, _| Answer::Failed("No connection: dns".into()),
        |_| fresh(list("v2"), None),
    );
    assert_eq!(got.unwrap(), Some(list("v2")));
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn a_404_is_not_retried_on_the_mirror() {
    let cache = scratch("404");
    let latest = "https://api.github.com/repos/sunrise-org/sunrise/releases/latest";
    let absent = json_with(
        latest,
        &cache,
        T0,
        |_, _| {
            failure(
                latest,
                ureq::Error::Status(404, ureq::Response::new(404, "Not Found", "{}").unwrap()),
            )
        },
        no_mirror,
    )
    .unwrap();
    assert_eq!(absent, None);
    let absent = json_with(
        RELEASES,
        &cache,
        T0,
        |_, _| Answer::RateLimited,
        |_| Answer::NotFound,
    )
    .unwrap();
    assert_eq!(absent, None);
    let empty = json_with(
        RELEASES,
        &cache,
        T0,
        |_, _| fresh(serde_json::json!([]), None),
        no_mirror,
    )
    .unwrap();
    assert_eq!(empty, Some(serde_json::json!([])));
    // A failure whose text mentions 404 is not an absent release.
    let url = format!("{latest}?note=404");
    let error = json_with(
        &url,
        &cache,
        T0,
        |_, _| {
            failure(
                &url,
                ureq::Error::Status(
                    500,
                    ureq::Response::new(500, "Server Error", "404 in the body").unwrap(),
                ),
            )
        },
        |_| Answer::Failed("offline".into()),
    )
    .unwrap_err();
    assert!(error.contains("answered 500"), "{error}");
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn both_failing_uses_the_saved_copy_else_says_so() {
    let cache = scratch("both");
    let err = json_with(
        RELEASES,
        &cache,
        T0,
        |_, _| Answer::RateLimited,
        |_| Answer::Failed("No connection: timed out".into()),
    )
    .unwrap_err();
    assert!(err.contains("rate limit"), "{err}");
    assert!(err.contains("mirror"), "{err}");
    assert!(err.contains("timed out"), "{err}");
    assert!(err.contains("Try again in a few minutes"), "{err}");

    // With a copy from an earlier start, that copy is better than nothing.
    json_with(
        RELEASES,
        &cache,
        T0,
        |_, _| fresh(list("nightly"), Some("\"e\"".into())),
        no_mirror,
    )
    .unwrap();
    let got = json_with(
        RELEASES,
        &cache,
        LATER,
        |_, _| Answer::RateLimited,
        |_| Answer::RateLimited,
    );
    assert_eq!(got.unwrap(), Some(list("nightly")));
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn other_repositories_have_no_mirror() {
    let cache = scratch("other");
    let err = json_with(
        "https://api.github.com/repos/someone/mod/commits/abc",
        &cache,
        T0,
        |_, _| Answer::RateLimited,
        no_mirror,
    )
    .unwrap_err();
    assert!(err.contains("rate limit"), "{err}");
    assert!(!err.contains("mirror"), "{err}");
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn no_mirror_is_built_in() {
    let cache = scratch("built-in");
    let err = json_with_policy(
        RELEASES,
        &cache,
        T0,
        MIRROR,
        CachePolicy::PreferCache,
        |_, _| Answer::RateLimited,
        no_mirror,
    )
    .unwrap_err();
    assert!(err.contains("rate limit"), "{err}");
    assert!(!err.contains("mirror"), "{err}");
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn a_corrupt_or_foreign_copy_is_ignored() {
    let cache = scratch("corrupt");
    fs::write(cache.join(cache_name(RELEASES)), b"not json").unwrap();
    let got = json_with(
        RELEASES,
        &cache,
        T0,
        |_, etag| {
            assert_eq!(etag, None);
            fresh(list("v1"), None)
        },
        no_mirror,
    );
    assert_eq!(got.unwrap(), Some(list("v1")));
    let foreign = Kept {
        url: "https://api.github.com/other".into(),
        etag: "\"x\"".into(),
        saved: T0,
        page: Page {
            body: list("x"),
            next: None,
        },
    };
    fs::write(
        cache.join(cache_name(RELEASES)),
        serde_json::to_vec(&foreign).unwrap(),
    )
    .unwrap();
    let got = json_with(
        RELEASES,
        &cache,
        T0,
        |_, etag| {
            assert_eq!(etag, None);
            fresh(list("v2"), None)
        },
        no_mirror,
    );
    assert_eq!(got.unwrap(), Some(list("v2")));
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn a_manual_launcher_check_revalidates_even_a_fresh_release_list() {
    let url = format!(
        "{API}/repos/{}/releases?per_page=100",
        backend::LAUNCHER_REPO
    );
    let cache = scratch("manual-launcher-check");
    write_kept(
        &cache.join(cache_name(&url)),
        &url,
        "old",
        &Page {
            body: list("v1.1.0"),
            next: Some(false),
        },
        T0,
    );
    assert_eq!(
        json_with(
            &url,
            &cache,
            T0 + 1,
            |_, _| panic!("fresh cached list"),
            no_mirror
        )
        .unwrap(),
        Some(list("v1.1.0")),
    );

    let forced = |answer: &dyn Fn() -> Answer, etag: &str| {
        json_with_policy(
            &url,
            &cache,
            T0 + 1,
            "",
            CachePolicy::Revalidate,
            |_, sent| {
                assert_eq!(sent, Some(etag));
                answer()
            },
            no_mirror,
        )
    };
    assert_eq!(
        forced(&|| fresh(list("v1.1.1"), Some("new".into())), "old").unwrap(),
        Some(list("v1.1.1")),
    );
    assert_eq!(
        forced(&|| Answer::NotModified(None), "new").unwrap(),
        Some(list("v1.1.1"))
    );
    assert_eq!(
        forced(&|| Answer::Failed("offline".into()), "new"),
        Err("offline".into())
    );
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn the_launchers_releases_are_never_kept_from_the_mirror() {
    const LAUNCHER: &str =
        "https://api.github.com/repos/sofia-gros/sunrise-launcher-offline/releases?per_page=100";
    let cache = scratch("launcher-mirror");
    let mirrored = |_: &str| fresh(list("v9.9.9"), None);
    let got = json_with(LAUNCHER, &cache, T0, |_, _| Answer::RateLimited, mirrored);
    if cfg!(debug_assertions) {
        assert_eq!(got.unwrap(), Some(list("v9.9.9")));
    }
    // Nothing was kept: the next read asks GitHub again, and with it failing there is no copy to fall back on.
    let again = json_with(
        LAUNCHER,
        &cache,
        T0 + 1,
        |_, _| Answer::RateLimited,
        |_| Answer::Failed("down".into()),
    );
    assert!(again.is_err());
    // A copy without GitHub's ETag (what a mirror's answer was kept as) is ignored.
    write_kept(
        &cache.join(cache_name(LAUNCHER)),
        LAUNCHER,
        "",
        &Page {
            body: list("v9.9.9"),
            next: Some(false),
        },
        T0,
    );
    let fresh = json_with(
        LAUNCHER,
        &cache,
        T0 + 1,
        |_, _| fresh(list("v1.1.0"), Some("e".into())),
        no_mirror,
    );
    assert_eq!(fresh.unwrap(), Some(list("v1.1.0")));
    let _ = fs::remove_dir_all(cache);
}

#[test]
fn a_stable_profile_finds_the_release_on_page_two_even_when_offline_afterwards() {
    use crate::instances::Channel::{Nightly, Stable};
    let Some(asset) = backend::ASSET else { return };
    let cache = tempfile::tempdir().unwrap();
    let first = {
        let mut rows = vec![serde_json::json!({
            "tag_name": "nightly", "name": "Nightly 2026-11-20 · aaaaaaa", "prerelease": true,
            "assets": [{"name": asset, "size": 1}],
        })];
        rows.extend(
            (1..RELEASES_PER_PAGE)
                .map(|i| serde_json::json!({"tag_name": format!("sdk-{i:040x}")})),
        );
        serde_json::Value::Array(rows)
    };
    let stable = serde_json::json!([{
        "tag_name": "26.3-0.1.0", "assets": [{"name": asset, "size": 1}],
    }]);
    assert_eq!(
        Stable.following(&backend::game_releases(first.as_array().unwrap())),
        Nightly
    );

    let fetch = |now, offline| {
        release_list_with(backend::GAME_REPO, CachePolicy::PreferCache, |request| {
        page_with(request, cache.path(), now, "https://mirror.example/github", |url, _| {
            if offline || url != RELEASES {
                return Answer::RateLimited;
            }
            // GitHub spells the target with its numerical repository ID. Only the next relation
            // travels into the cache, and the mirror gets the original repo path for page two.
            answer(url, format!("HTTP/1.1 200 OK\r\nLink: <{API}/repositories/1438526794/releases?per_page=100&page=2>; rel=\"next\", <{API}/repositories/1438526794/releases?per_page=100&page=2>; rel=\"last\"\r\nETag: page1\r\n\r\n{first}").parse(), true)
        }, |url| {
            if offline {
                return Answer::Failed("offline".into());
            }
            assert_eq!(url, "https://mirror.example/github/repos/sunrise-org/sunrise/releases?per_page=100&page=2");
            fresh(stable.clone(), None)
        })
    }).unwrap()
    };

    for list in [
        fetch(T0, false),
        fetch(T0 + 1, true),
        fetch(LATER + 1, true),
    ] {
        let releases = backend::game_releases(&list);
        assert_eq!(Stable.following(&releases), Stable);
        assert_eq!(
            Stable
                .play_target(&releases, &[], None, None)
                .map(String::as_str),
            Some("26.3-0.1.0")
        );
    }
}

#[test]
fn launcher_updates_revalidate_every_page_and_do_not_accept_an_incomplete_feed() {
    let cache = tempfile::tempdir().unwrap();
    let first = format!(
        "{API}/repos/{}/releases?per_page=100",
        backend::LAUNCHER_REPO
    );
    let second = format!("{first}&page=2");
    let asset = backend::launcher_asset();
    let published = |tag: &str| {
        serde_json::json!([{
            "tag_name": tag, "assets": [{"name": asset, "size": 1}],
        }])
    };
    let read = |step: u8| {
        release_list_with(backend::LAUNCHER_REPO, CachePolicy::Revalidate, |request| {
            assert!(request.policy == CachePolicy::Revalidate);
            page_with(
                request,
                cache.path(),
                T0,
                "",
                |url, etag| match (step, url == first) {
                    (0, true) => Answer::Fresh(
                        Page {
                            body: published("v1.1.0"),
                            next: Some(true),
                        },
                        Some("one".into()),
                    ),
                    (0, false) => {
                        assert_eq!(url, second);
                        fresh(published("v99.0.0"), Some("two".into()))
                    }
                    (_, true) => {
                        assert_eq!(etag, Some("one"));
                        // Even when a 304 omits Link, the original pagination is kept.
                        Answer::NotModified(None)
                    }
                    (1, false) => {
                        assert_eq!(url, second);
                        assert_eq!(etag, Some("two"));
                        Answer::NotModified(None)
                    }
                    (2, false) => Answer::Failed("offline".into()),
                    _ => Answer::NotFound,
                },
                no_mirror,
            )
        })
    };
    for step in [0, 1] {
        let list = read(step).unwrap();
        assert_eq!(
            backend::parse_launcher_releases(&list, asset)[0].tag,
            "v99.0.0"
        );
    }
    assert_eq!(read(2).unwrap_err(), "offline");
    assert_eq!(read(3).unwrap_err(), format!("{second} answered 404"));
}

#[test]
fn only_an_array_counts_as_a_checked_release_feed() {
    let cache = tempfile::tempdir().unwrap();
    for repo in [backend::GAME_REPO, backend::LAUNCHER_REPO] {
        let first = format!("{API}/repos/{repo}/releases?per_page=100");
        let read = |reply: fn() -> Answer| {
            release_list_with(repo, CachePolicy::Revalidate, |request| {
                assert_eq!(request.url, first);
                page_with(request, cache.path(), T0, "", |_, _| reply(), no_mirror)
            })
        };
        assert_eq!(
            read(|| Answer::NotFound).unwrap_err(),
            format!("{first} answered 404")
        );
        assert_eq!(
            read(|| Answer::Fresh(
                Page {
                    body: serde_json::json!({"message": "not a list"}),
                    next: Some(true)
                },
                None,
            ))
            .unwrap_err(),
            format!("{first} didn't answer with a release list.")
        );
        assert!(read(|| fresh(serde_json::json!([]), None))
            .unwrap()
            .is_empty());
    }
}

#[test]
fn old_full_pages_without_link_metadata_are_refetched_unconditionally() {
    let cache = tempfile::tempdir().unwrap();
    let first = cache.path().join(cache_name(RELEASES));
    fs::write(first, serde_json::json!({
        "url": RELEASES, "saved": T0, "etag": "old", "body": vec![serde_json::Value::Null; RELEASES_PER_PAGE],
    }).to_string()).unwrap();
    let read = || {
        release_list_with(backend::GAME_REPO, CachePolicy::PreferCache, |request| {
            page_with(
                request,
                cache.path(),
                T0 + 1,
                "",
                |url, etag| {
                    assert_eq!(url, RELEASES);
                    assert_eq!(etag, None);
                    fresh(list("26.3-0.1.0"), Some("new".into()))
                },
                no_mirror,
            )
        })
    };
    assert_eq!(read().unwrap()[0]["tag_name"], "26.3-0.1.0");
}

#[test]
fn release_pagination_refuses_a_feed_past_the_cap() {
    let calls = std::cell::Cell::new(0);
    let err = release_list_with(backend::GAME_REPO, CachePolicy::PreferCache, |_| {
        calls.set(calls.get() + 1);
        Ok(Some(Page {
            body: list("nightly"),
            next: Some(true),
        }))
    })
    .unwrap_err();
    assert_eq!(calls.get(), MAX_RELEASE_PAGES);
    assert!(err.contains("release list exceeds"));
}

#[test]
fn a_json_only_mirror_does_not_hide_the_next_page_after_a_full_one() {
    let cache = tempfile::tempdir().unwrap();
    let read = release_list_with(backend::GAME_REPO, CachePolicy::PreferCache, |request| {
        page_with(
            request,
            cache.path(),
            T0,
            "https://mirror.example",
            |_, _| Answer::RateLimited,
            |url| {
                let body = if url.ends_with("per_page=100") {
                    serde_json::json!(vec![serde_json::Value::Null; RELEASES_PER_PAGE])
                } else {
                    assert!(url.ends_with("per_page=100&page=2"), "{url}");
                    list("26.3-0.1.0")
                };
                answer(url, format!("HTTP/1.1 200 OK\r\n\r\n{body}").parse(), false)
            },
        )
    })
    .unwrap();
    assert_eq!(read[RELEASES_PER_PAGE]["tag_name"], "26.3-0.1.0");
}
