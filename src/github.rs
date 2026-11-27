//! GitHub's REST API, as the launcher reads it: release lists, releases by tag, commits.
//!
//! Without a token GitHub allows 60 API calls an hour per address, and players behind a shared one
//! (a school, a dorm, a carrier's NAT) spend them together. So:
//! - every answer is kept on disk (`<data>/cache/github/`) and read again without asking for
//!   [`FRESH_SECS`]: a launcher started a few times in a row, or several pages asking for the same
//!   list, spend one call. After that the read is a conditional request with the saved ETag. An
//!   anonymous 304 still counts against the hour (measured 2026-11-01), so that only saves the
//!   download; with `SUNRISE_GITHUB_TOKEN` (CI) a 304 is free;
//! - when GitHub answers with its rate limit (or not at all), Sunrise's own repository is read
//!   through the mirror `SUNRISE_GITHUB_MIRROR` names, if any ([`MIRROR`]: none built in);
//! - when both fail, the last answer kept on disk is used, else the error says what happened.
//!
//! Release files are not read here: their `browser_download_url` (github.com, then
//! release-assets.githubusercontent.com) is not part of the API and has no such limit.
use crate::backend;
use sha2::Digest;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const API: &str = sunrise_launcher_addresses::GITHUB_API;
/// The built-in read-only mirror of Sunrise's release reads: none. `SUNRISE_GITHUB_MIRROR` names one.
pub const MIRROR: &str = "";

/// How long a saved answer is used without asking again: the launcher looks for releases every
/// [`backend::CHECK_EVERY`], so a few minutes late at worst.
pub const FRESH_SECS: u64 = 5 * 60;

/// What one source answered.
#[derive(Debug)]
enum Answer {
    /// 200, with the ETag GitHub gave (none from the mirror).
    Fresh(Page, Option<String>),
    /// 304: the copy on disk is still current. The reply can update its pagination link.
    NotModified(Option<bool>),
    NotFound,
    /// 429, or 403 with GitHub's rate-limit headers.
    RateLimited,
    /// Any other status, or no connection: the error to show.
    Failed(String),
}

/// GitHub's JSON for a required resource. Missing resources are reported as errors.
pub fn json(url: &str) -> Result<serde_json::Value, String> {
    json_optional(url)?.ok_or_else(|| crate::i18n::trf("{url} answered 404", &[("url", url)]))
}

/// Whether the saved answer can stand in for a new request.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CachePolicy {
    PreferCache,
    Revalidate,
}

/// GitHub's JSON when the resource may not exist, such as a repository's latest release.
/// A 404 is None, distinct from a connection or rate-limit failure.
pub fn json_optional(url: &str) -> Result<Option<serde_json::Value>, String> {
    Ok(page(Request {
        url,
        policy: CachePolicy::PreferCache,
        paginated: false,
    })?
    .map(|page| page.body))
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Page {
    body: serde_json::Value,
    /// Missing when a legacy mirror stripped Link, or a saved copy predates pagination.
    #[serde(default)]
    next: Option<bool>,
}

#[derive(Clone, Copy)]
struct Request<'a> {
    url: &'a str,
    policy: CachePolicy,
    paginated: bool,
}

const RELEASES_PER_PAGE: usize = 100;
// SDK and dated Nightly releases accumulate much faster than Stable releases. Bound the walk
// well below GitHub's anonymous hourly limit without mistaking a partial feed for the whole list.
const MAX_RELEASE_PAGES: usize = 32;

/// Only a complete sequence of JSON arrays counts as a checked release feed.
pub(crate) fn release_list(
    repo: &str,
    policy: CachePolicy,
) -> Result<Vec<serde_json::Value>, String> {
    release_list_with(repo, policy, page)
}

fn release_list_with(
    repo: &str,
    policy: CachePolicy,
    fetch: impl Fn(Request<'_>) -> Result<Option<Page>, String>,
) -> Result<Vec<serde_json::Value>, String> {
    let first = format!("{API}/repos/{repo}/releases?per_page={RELEASES_PER_PAGE}");
    let mut releases = Vec::new();
    for number in 1..=MAX_RELEASE_PAGES {
        // GitHub can spell Link targets /repositories/<id>/releases. Follow its next relation,
        // not the response's URL: each page keeps our configured repo, mirror path and credentials.
        let next_url = (number > 1).then(|| format!("{first}&page={number}"));
        let url = next_url.as_deref().unwrap_or(&first);
        let page = fetch(Request {
            url,
            policy,
            paginated: true,
        })?
        .ok_or_else(|| crate::i18n::trf("{url} answered 404", &[("url", url)]))?;
        let serde_json::Value::Array(items) = page.body else {
            return Err(crate::i18n::trf(
                "{url} didn't answer with a release list.",
                &[("url", url)],
            ));
        };
        releases.extend(items);
        if page.next != Some(true) {
            return Ok(releases);
        }
    }
    Err(crate::i18n::trf(
        "{repo}'s release list exceeds {pages} pages.",
        &[("repo", repo), ("pages", &MAX_RELEASE_PAGES.to_string())],
    ))
}

fn page(request: Request<'_>) -> Result<Option<Page>, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    page_with(request, &cache_dir(), now, &mirror_base(), github, mirror)
}

fn page_with(
    Request {
        url,
        policy,
        paginated,
    }: Request<'_>,
    cache: &Path,
    now: u64,
    mirror_base: &str,
    github: impl Fn(&str, Option<&str>) -> Answer,
    mirror: impl Fn(&str) -> Answer,
) -> Result<Option<Page>, String> {
    let file = cache.join(cache_name(url));
    let launcher = launcher_url(url);
    let kept = read_kept(&file, url).filter(|k| {
        // A pre-pagination full page has no saved Link. Fetch it without an ETag rather
        // than let a 304 or an offline cache make a truncated list look complete.
        !(launcher && k.etag.is_empty()
            || paginated
                && k.page.next.is_none()
                && k.page
                    .body
                    .as_array()
                    .is_some_and(|items| items.len() >= RELEASES_PER_PAGE))
    });
    if policy == CachePolicy::PreferCache {
        if let Some(k) = kept.as_ref() {
            // A copy saved "later" than now (the clock was set back) is not trusted as fresh.
            if now >= k.saved && now - k.saved < FRESH_SECS {
                return Ok(Some(k.page.clone()));
            }
        }
    }
    let etag = kept
        .as_ref()
        .map(|k| k.etag.as_str())
        .filter(|e| !e.is_empty());
    let why = match github(url, etag) {
        Answer::Fresh(page, etag) => {
            write_kept(&file, url, etag.as_deref().unwrap_or(""), &page, now);
            return Ok(Some(page));
        }
        Answer::NotModified(next) => match kept {
            Some(mut k) => {
                k.page.next = next.or(k.page.next);
                write_kept(&file, url, &k.etag, &k.page, now);
                return Ok(Some(k.page));
            }
            // A 304 to a request that had no ETag: can't happen, but don't trust it.
            None => crate::i18n::tr("GitHub answered 304 without a saved copy").to_owned(),
        },
        Answer::NotFound => return Ok(None),
        Answer::RateLimited => RATE_LIMITED.to_owned(),
        Answer::Failed(e) => e,
    };
    let from_mirror = match mirror_url_with(url, mirror_base) {
        Some(m) => match mirror(&m) {
            Answer::Fresh(mut page, _) => {
                // A JSON-only mirror may omit GitHub's Link header. Only there, probe the
                // next page after a full page rather than assume the rest doesn't exist.
                if paginated && page.next.is_none() {
                    page.next = Some(
                        page.body
                            .as_array()
                            .is_some_and(|rows| rows.len() == RELEASES_PER_PAGE),
                    );
                }
                // The launcher's own releases are never kept from a mirror.
                if !launcher {
                    write_kept(&file, url, "", &page, now);
                }
                return Ok(Some(page));
            }
            Answer::NotFound => return Ok(None),
            Answer::RateLimited => {
                Some(crate::i18n::tr("too many requests from this network").to_owned())
            }
            Answer::Failed(e) => Some(e),
            Answer::NotModified(_) => Some(crate::i18n::tr("an unexpected 304").to_owned()),
        },
        None => None,
    };
    if policy == CachePolicy::PreferCache {
        if let Some(k) = kept {
            return Ok(Some(k.page));
        }
    }
    Err(match from_mirror {
        Some(m) => crate::i18n::trf(
            "{why}, and Sunrise's mirror of GitHub couldn't help either ({mirror}). Try again in a few minutes.",
            &[("why", why.as_str()), ("mirror", m.as_str())],
        ),
        None => why,
    })
}

const RATE_LIMITED: &str = "GitHub's rate limit was reached for your network";

/// Whether `url` is an API address of the launcher's own repository.
fn launcher_url(url: &str) -> bool {
    url.strip_prefix(API)
        .is_some_and(|path| path.starts_with(&format!("/repos/{}/", backend::LAUNCHER_REPO)))
}

/// The mirror's URL for an API URL it serves, or None: the game's repository, and the launcher's in a development build only.
fn mirror_url_with(url: &str, base: &str) -> Option<String> {
    let path = url.strip_prefix(API)?;
    let launcher = cfg!(debug_assertions).then_some(backend::LAUNCHER_REPO);
    let served = std::iter::once(backend::GAME_REPO)
        .chain(launcher)
        .any(|repo| path.starts_with(&format!("/repos/{repo}/")));
    (!base.is_empty() && served).then(|| format!("{}{path}", base.trim_end_matches('/')))
}

/// The mirror `SUNRISE_GITHUB_MIRROR` names, when there is one: what a child the launcher starts is told too.
pub(crate) fn mirror_named() -> Option<String> {
    Some(mirror_base()).filter(|base| !base.is_empty())
}

fn mirror_base() -> String {
    std::env::var("SUNRISE_GITHUB_MIRROR").unwrap_or_else(|_| MIRROR.to_owned())
}

/// GitHub itself, with the saved ETag when there is one.
fn github(url: &str, etag: Option<&str>) -> Answer {
    let mut request = match backend::agent(url) {
        Ok(agent) => agent.get(url),
        Err(error) => return Answer::Failed(error),
    };
    // CI runners share addresses and hit GitHub's anonymous rate limit; they pass their own token.
    if let Some(token) = std::env::var("SUNRISE_GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    if let Some(etag) = etag {
        request = request.set("If-None-Match", etag);
    }
    answer(url, request.call(), true)
}

fn mirror(url: &str) -> Answer {
    match backend::agent(url) {
        Ok(agent) => answer(url, agent.get(url).call(), false),
        Err(error) => Answer::Failed(error),
    }
}

fn answer(url: &str, response: Result<ureq::Response, ureq::Error>, direct: bool) -> Answer {
    match response {
        Ok(r) if r.status() == 304 => Answer::NotModified(next_link(&r)),
        Ok(r) => {
            let etag = r.header("ETag").filter(|_| direct).map(str::to_owned);
            let next = next_link(&r).or(direct.then_some(false));
            match r.into_json() {
                Ok(body) => Answer::Fresh(Page { body, next }, etag),
                Err(e) => Answer::Failed(e.to_string()),
            }
        }
        Err(e) => failure(url, e),
    }
}

fn next_link(response: &ureq::Response) -> Option<bool> {
    let links = response.all("Link");
    (!links.is_empty()).then(|| {
        links
            .iter()
            .flat_map(|header| header.split(','))
            .any(|link| {
                link.trim()
                    .strip_prefix('<')
                    .and_then(|target| target.split_once('>'))
                    .is_some_and(|(_, params)| {
                        params
                            .split(';')
                            .any(|param| param.trim() == "rel=\"next\"")
                    })
            })
    })
}

fn failure(url: &str, e: ureq::Error) -> Answer {
    match e {
        ureq::Error::Status(404, _) => Answer::NotFound,
        ureq::Error::Status(code, r) if rate_limited(code, |h| r.header(h).map(str::to_owned)) => {
            Answer::RateLimited
        }
        ureq::Error::Status(code, _) => Answer::Failed(crate::i18n::trf(
            "{url} answered {code}",
            &[("url", url), ("code", &code.to_string())],
        )),
        ureq::Error::Transport(t) => Answer::Failed(format!("No connection: {t}")),
    }
}

/// Whether a status with these headers is GitHub's rate limit: always a 429; a 403 only with
/// `x-ratelimit-remaining: 0` (the hourly limit) or `retry-after` (the secondary one), since a 403
/// can also be a refusal.
pub(crate) fn rate_limited(code: u16, header: impl Fn(&str) -> Option<String>) -> bool {
    code == 429
        || (code == 403
            && (header("x-ratelimit-remaining").as_deref() == Some("0")
                || header("retry-after").is_some()))
}

fn cache_dir() -> PathBuf {
    backend::data_dir().join("cache").join("github")
}

fn cache_name(url: &str) -> String {
    let hash = sha2::Sha256::digest(url.as_bytes());
    let hex: String = hash[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Kept {
    url: String,
    /// GitHub's ETag, or empty (the mirror's answers, or GitHub gave none).
    etag: String,
    /// When it was saved or last confirmed (Unix seconds).
    saved: u64,
    #[serde(flatten)]
    page: Page,
}

fn read_kept(file: &Path, url: &str) -> Option<Kept> {
    let kept: Kept = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    (kept.url == url).then_some(kept)
}

/// Saves an answer for the next reads; a failure only costs that saving.
fn write_kept(file: &Path, url: &str, etag: &str, page: &Page, saved: u64) {
    let kept = Kept {
        url: url.to_owned(),
        etag: etag.to_owned(),
        saved,
        page: page.clone(),
    };
    let Some(dir) = file.parent() else { return };
    let Ok(text) = serde_json::to_vec(&kept) else {
        return;
    };
    let partial = file.with_extension("partial");
    if fs::create_dir_all(dir).is_ok() && fs::write(&partial, text).is_ok() {
        let _ = fs::rename(&partial, file);
    }
}

#[cfg(test)]
mod tests;
