//! The game's log as the console shows it, and sharing it.
//!
//! Sharing uploads the log to mclo.gs, and only when the player asks (a click, or `logs share`).
//! mclo.gs is a paste service made for Minecraft logs by Aternos: no account, links expire after
//! 90 days unread, and the uploader gets a token that deletes the paste. Before anything leaves
//! the computer, [`private`] takes out the player's home folder, their user name and their
//! account names.
use std::path::Path;

/// mclo.gs's API; `SUNRISE_LAUNCHER_PASTE_URL` points it elsewhere for tests.
const API: &str = "";
const PASTE_ENV: &str = "SUNRISE_LAUNCHER_PASTE_URL";
/// mclo.gs keeps at most 25,000 lines and 10 MiB: the end of a longer log is sent.
const MAX_LINES: usize = 25_000;
const MAX_BYTES: usize = 10 << 20;

/// The paste API to share with: `SUNRISE_LAUNCHER_PASTE_URL` when it is set, else mclo.gs's.
pub fn api() -> String {
    std::env::var(PASTE_ENV)
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| API.into())
}

/// Whether a console line matches the search: every word, in any case.
pub fn matches(line: &str, search: &str) -> bool {
    let line = line.to_lowercase();
    search
        .split_whitespace()
        .all(|w| line.contains(&w.to_lowercase()))
}

/// The names to take out of a log: the account names, the offline player name kept in each game
/// folder (`sunrise-player.txt`), and the computer's user name.
pub fn names(homes: &[&Path]) -> Vec<String> {
    let accounts = crate::account::Accounts::load();
    let mut names: Vec<String> = accounts.accounts.into_iter().map(|a| a.name).collect();
    for home in homes {
        if let Ok(name) = std::fs::read_to_string(home.join("sunrise-player.txt")) {
            names.push(name.trim().to_string());
        }
    }
    for var in ["USER", "USERNAME"] {
        if let Ok(user) = std::env::var(var) {
            names.push(user);
        }
    }
    if let Some(user) =
        dirs::home_dir().and_then(|h| h.file_name().map(|n| n.to_string_lossy().into_owned()))
    {
        names.push(user);
    }
    names.retain(|n| n.chars().count() >= 3);
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    names.dedup();
    names
}

/// The log without the home folder (`~` instead, with either slash) and without `names` (each
/// becomes `<player>`), as whole words in any case.
pub fn private(text: &str, home: Option<&Path>, names: &[String]) -> String {
    let mut out = text.to_string();
    if let Some(home) = home
        .map(|h| h.to_string_lossy().into_owned())
        .filter(|h| h.len() > 1)
    {
        out = out.replace(&home, "~");
        out = out.replace(&home.replace('\\', "/"), "~");
        out = out.replace(&home.replace('/', "\\"), "~");
    }
    for name in names {
        out = replace_word(&out, name, "<player>");
    }
    out
}

/// Replaces `word` (any case) where it isn't part of a longer name.
fn replace_word(text: &str, word: &str, with: &str) -> String {
    let lower = text.to_lowercase();
    let find = word.to_lowercase();
    if find.is_empty() || lower.len() != text.len() {
        // A case change that moves bytes (rare scripts): exact matches only.
        return text.replace(word, with);
    }
    let part = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(i) = lower[at..].find(&find).map(|i| i + at) {
        let end = i + find.len();
        if part(text[..i].chars().next_back()) || part(text[end..].chars().next()) {
            out.push_str(&text[at..end]);
        } else {
            out.push_str(&text[at..i]);
            out.push_str(with);
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// The end of the log that fits what mclo.gs keeps.
pub fn fit(text: &str) -> &str {
    let mut start = 0;
    let lines: Vec<usize> = text.match_indices('\n').map(|(i, _)| i + 1).collect();
    if lines.len() > MAX_LINES {
        start = lines[lines.len() - MAX_LINES - 1];
    }
    while text.len() - start > MAX_BYTES {
        start = text[start..]
            .find('\n')
            .map_or(text.len(), |i| start + i + 1);
    }
    &text[start..]
}

/// A shared log: its page, and what deletes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Shared {
    pub url: String,
    pub id: String,
    pub token: String,
}

/// Uploads `text` (already [`private`]) to the paste API at `api` ([`api`]) and returns its link.
pub fn share(api: &str, text: &str) -> Result<Shared, String> {
    if api.is_empty() {
        return Err(crate::i18n::tr("Log sharing is not configured for this build.").into());
    }
    let body = serde_json::json!({ "content": fit(text), "source": "Sunrise launcher" });
    let answer: serde_json::Value = ureq::post(&format!("{api}/log"))
        .send_json(body)
        .map_err(|e| {
            crate::i18n::trf(
                "mclo.gs didn't take the log: {error}",
                &[("error", &e.to_string())],
            )
        })?
        .into_json()
        .map_err(|e| {
            crate::i18n::trf(
                "mclo.gs answered something unexpected: {error}",
                &[("error", &e.to_string())],
            )
        })?;
    let field = |k: &str| answer[k].as_str().map(str::to_string);
    match (
        answer["success"].as_bool(),
        field("url"),
        field("id"),
        field("token"),
    ) {
        (Some(true), Some(url), Some(id), Some(token)) => Ok(Shared { url, id, token }),
        _ => Err(crate::i18n::trf(
            "mclo.gs didn't take the log: {reason}",
            &[(
                "reason",
                &field("error").unwrap_or_else(|| crate::i18n::tr("no reason given").to_string()),
            )],
        )),
    }
}

/// Deletes a log shared from here, at the paste API at `api` ([`api`]).
pub fn unshare(api: &str, shared: &Shared) -> Result<(), String> {
    ureq::delete(&format!("{api}/log/{}", shared.id))
        .set("Authorization", &format!("Bearer {}", shared.token))
        .call()
        .map_err(|e| {
            crate::i18n::trf(
                "mclo.gs didn't delete it: {error}",
                &[("error", &e.to_string())],
            )
        })?;
    Ok(())
}

#[cfg(test)]
mod tests;
