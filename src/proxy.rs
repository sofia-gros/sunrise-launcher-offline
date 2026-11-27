//! The proxy every download and request goes through, when the player needs one: a school's or an
//! office's, or a country where a host is unreachable. The Settings page's Proxy row (kept as
//! `proxy` in `settings.json`) wins; without it the usual `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`
//! and `NO_PROXY` variables apply, so a launcher started from a shell that has them needs nothing.
//!
//! What goes through it: Sunrise's releases and the launcher's own update, Minecraft's files and
//! Java from Mojang, Modrinth, CurseForge, Sunrise's mod registry, pictures, Microsoft sign-in,
//! Tailscale's API, and the jars a modpack import downloads. A local address never does, and the
//! Servers page's calls to a game server's API (`servers.rs`) build their agent without it: they
//! are on loopback and asked every second.
//!
//! Proxy selection is shared with the game's porter and tools. This module keeps the launcher's
//! own setting from `settings.json` in memory and passes it to those tools as `HTTPS_PROXY` and
//! `HTTP_PROXY`.
use crate::backend::Settings;
use std::sync::Mutex;

/// The launcher's setting, as `announce` last read it, or None before it has: a request shouldn't
/// read `settings.json` every time, and an import fetches hundreds of jars.
static SET: Mutex<Option<Option<String>>> = Mutex::new(None);

pub(crate) const USER_AGENT: &str = concat!("sunrise-launcher/", env!("CARGO_PKG_VERSION"));

/// Puts the launcher's network settings in `command`'s environment, as the programs it starts (the porter, the native build tool) read them: its proxy setting in `HTTPS_PROXY`/`HTTP_PROXY` (the environment's own, and `NO_PROXY`, pass through), and the GitHub mirror.
pub fn child_env(command: &mut std::process::Command) {
    if let Some(spec) = setting() {
        let spec = address(spec);
        command.env("HTTPS_PROXY", &spec).env("HTTP_PROXY", spec);
    }
    if let Some(mirror) = crate::github::mirror_named() {
        command.env("SUNRISE_GITHUB_MIRROR", mirror);
    }
}

/// `spec` as a program that is handed it reads it, and as the launcher's own downloads do: a bare
/// `socks://` or `socks5://` would be SOCKS4 or resolve names locally there, so both become
/// `socks5h://` (SOCKS5, the proxy resolving names), and a bare `host:port` gets `http://`.
pub fn address(spec: String) -> String {
    match spec.split_once("://") {
        Some(("socks" | "socks5", address)) => format!("socks5h://{address}"),
        Some(_) => spec,
        None => format!("http://{spec}"),
    }
}

/// Agent for this slot's HTTP settings and the route selected for `url`.
pub(crate) fn agent(
    slot: &str,
    url: &str,
    configure: impl FnOnce(ureq::AgentBuilder) -> ureq::AgentBuilder,
) -> Result<ureq::Agent, String> {
    sunrise_launcher_proxy::agent(slot, url, setting().as_deref(), configure)
        .map_err(|e| e.to_string())
}

/// Anonymous GET with the configured proxy, leaving redirects and status to the caller.
pub(crate) fn anonymous_get_no_redirect(address: &str) -> Result<ureq::Request, String> {
    let agent = agent("anonymous-get-no-redirect", address, |builder| {
        builder.redirects(0)
    })?;
    Ok(agent.get(address).set("User-Agent", USER_AGENT))
}

/// Why `spec` can't be used as a proxy, or None when it can (empty means none).
pub fn refuse(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    sunrise_launcher_proxy::validate(spec).err().map(|error| {
        crate::i18n::trf(
            "{error}. Write it as http://host:port or socks5://host:port.",
            &[("error", &error.to_string())],
        )
    })
}

/// Update the stored setting and drop pooled routes to the old proxy.
pub fn announce(setting: Option<String>) {
    let spec = setting
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty());
    let mut kept = held();
    if kept.as_ref() != Some(&spec) {
        *kept = Some(spec);
        sunrise_launcher_proxy::clear_agents();
    }
}

/// The launcher's setting: read from `settings.json` the first time it is needed and kept after
/// that, so a download doesn't read the file once per file.
pub(crate) fn setting() -> Option<String> {
    let mut set = held();
    match set.clone() {
        Some(kept) => kept,
        None => {
            let read = written();
            *set = Some(read.clone());
            read
        }
    }
}

/// The launcher's setting as it is written down, or None when there is none.
fn written() -> Option<String> {
    Settings::load()
        .proxy
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
}

/// The setting's lock, even after a thread panicked while holding it: an address lost to a panic
/// is worse than an old one kept.
fn held() -> std::sync::MutexGuard<'static, Option<Option<String>>> {
    SET.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxy_address_is_read_the_same_for_downloads_and_children() {
        let spec = |s: &str| address(s.to_owned());
        assert_eq!(spec("socks://10.0.0.2:1080"), "socks5h://10.0.0.2:1080");
        assert_eq!(
            spec("socks5://u:p@10.0.0.2:1080"),
            "socks5h://u:p@10.0.0.2:1080"
        );
        assert_eq!(spec("socks5h://10.0.0.2:1080"), "socks5h://10.0.0.2:1080");
        assert_eq!(spec("http://10.0.0.2:3128"), "http://10.0.0.2:3128");
        assert_eq!(spec("10.0.0.2:3128"), "http://10.0.0.2:3128");
    }

    #[test]
    fn child_env_gives_a_child_the_socks5h_address() {
        static TURN: Mutex<()> = Mutex::new(());
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let kept = held().replace(Some("socks://h:1".into()));
        let mut command = std::process::Command::new("unused");
        child_env(&mut command);
        *held() = kept;
        let env = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned())
        };
        assert_eq!(env("HTTPS_PROXY").as_deref(), Some("socks5h://h:1"));
        assert_eq!(env("HTTP_PROXY").as_deref(), Some("socks5h://h:1"));
    }

    #[test]
    fn a_proxy_address_is_checked_before_it_is_kept() {
        assert_eq!(refuse(""), None);
        assert_eq!(refuse("  "), None);
        assert_eq!(refuse("http://127.0.0.1:7890"), None);
        assert_eq!(refuse("127.0.0.1:7890"), None);
        assert_eq!(refuse("http://user:pass@proxy.local:3128"), None);
        assert!(refuse("http://").is_some());
        assert_eq!(refuse("socks5://127.0.0.1:7891"), None);
        assert_eq!(refuse("socks5h://user:pass@127.0.0.1:7891"), None);
        assert!(refuse("socks5://").is_some());
    }

    /// The reason is shown under the field and printed by `sunrise-launcher settings set`.
    #[test]
    fn a_refused_address_keeps_its_credentials_to_itself() {
        for spec in [
            "socks3://user:secret@127.0.0.1:7891",
            "http://user:secret@",
            "http://user:secret@proxy.example:bad-port",
        ] {
            let why = refuse(spec).unwrap();
            assert!(why.starts_with("Launcher proxy setting: "), "{why}");
            assert!(!why.contains("secret"), "{why}");
        }
    }
}
