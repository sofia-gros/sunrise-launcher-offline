//! Fetching over HTTP: the agent with the launcher's user agent and the player's proxy, and the
//! small, cancellable and missing-tolerant gets built on it.
use super::VERSION;
use std::io::Read;

pub(crate) const CLIENT: &str = if cfg!(windows) {
    "sunrise-client.exe"
} else {
    "sunrise-client"
};

/// The agent `url` is fetched with: the launcher's own user agent, through the player's proxy when
/// one is set (`proxy`).
pub(crate) fn agent(url: &str) -> Result<ureq::Agent, String> {
    crate::proxy::agent("backend", url, |builder| {
        builder
            .user_agent(&format!("sunrise-launcher/{VERSION}"))
            // A connection that goes quiet fails instead of waiting for ever.
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(60))
    })
}

/// A file's body, from the network as the launcher reaches it.
pub fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    get(url)?
        .into_reader()
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    Ok(body)
}

pub(crate) fn get(url: &str) -> Result<ureq::Response, String> {
    get_unless_missing(url)?.ok_or_else(|| format!("{url} answered 404"))
}

/// [`get`], with `None` when the server answers 404: a file that may not exist (a prebuilt game).
pub(crate) fn get_unless_missing(url: &str) -> Result<Option<ureq::Response>, String> {
    let mut request = agent(url)?.get(url);
    // CI runners share addresses and hit GitHub's anonymous rate limit; they pass their own token.
    if let Some(token) = std::env::var("SUNRISE_GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        if url.starts_with("https://api.github.com/") {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
    }
    match crate::download_retry::get(&request).map_err(|e| *e) {
        Ok(response) => Ok(Some(response)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(e) => Err(match e {
            ureq::Error::Status(code, r)
                if url.starts_with("https://api.github.com/")
                    && crate::github::rate_limited(code, |h| r.header(h).map(str::to_owned)) =>
            {
                "GitHub's rate limit was reached. Try again in a few minutes.".into()
            }
            ureq::Error::Status(code, _) => format!("{url} answered {code}"),
            ureq::Error::Transport(t) => format!("No connection: {t}"),
        }),
    }
}

/// A small release file (its checksums or their signature), whole; None when it has none.
pub(crate) fn fetch_small(url: &str) -> Result<Option<Vec<u8>>, String> {
    fetch_small_cancellable(url, None)
}

pub(crate) fn fetch_small_cancellable(
    url: &str,
    stop: Option<&crate::native_mods::Cancellation>,
) -> Result<Option<Vec<u8>>, String> {
    if let Some(stop) = stop {
        stop.check()?;
    }
    if let Some(path) = crate::draft_releases::local(url)? {
        return crate::draft_releases::read(&path, 1 << 20);
    }
    let response = match get(url) {
        Ok(response) => response,
        Err(e) if e.ends_with("answered 404") => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut body = Vec::new();
    let reader = response.into_reader().take(1 << 20);
    let result = match stop {
        Some(stop) => stop.reader(reader).read_to_end(&mut body),
        None => {
            let mut reader = reader;
            reader.read_to_end(&mut body)
        }
    };
    if let Err(e) = result {
        if let Some(stop) = stop {
            stop.check()?;
        }
        return Err(crate::i18n::trf(
            "Download interrupted: {error}",
            &[("error", &e.to_string())],
        ));
    }
    Ok(Some(body))
}

pub(super) fn fetch_bytes_cancellable(
    url: &str,
    stop: Option<&crate::native_mods::Cancellation>,
) -> Result<Vec<u8>, String> {
    if let Some(stop) = stop {
        stop.check()?;
    }
    let mut body = Vec::new();
    let mut reader = get(url)?.into_reader();
    let result = match stop {
        Some(stop) => stop.reader(&mut reader).read_to_end(&mut body),
        None => reader.read_to_end(&mut body),
    };
    if let Err(error) = result {
        if let Some(stop) = stop {
            stop.check()?;
        }
        return Err(error.to_string());
    }
    Ok(body)
}
