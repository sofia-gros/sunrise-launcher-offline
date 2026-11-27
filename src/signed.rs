//! Signed downloads (docs/security.md): a game or launcher archive is installed only when its
//! SHA-256 matches the release's SHA256SUMS and that list is signed by one of the keys built in
//! from crates/release-verify/keys/. Anything else is refused with a message saying why.

use crate::backend::{GAME_REPO, LAUNCHER_REPO};
use sunrise_launcher_release_verify::{Channel, PublicKey, SIGNATURE, SIGNATURE_V2, SUMS};

/// Where a game release's attached file downloads from (GitHub's fixed address for it).
pub fn asset_url(tag: &str, name: &str) -> String {
    #[cfg(test)]
    if let Some(url) = TEST_GAME_ASSET_ORIGIN.with_borrow(|origin| {
        origin
            .as_ref()
            .map(|origin| format!("{origin}/{tag}/{name}"))
    }) {
        return url;
    }
    asset_url_in(GAME_REPO, tag, name)
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_GAME_ASSET_ORIGIN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Where this launcher's own release's attached file downloads from ([`LAUNCHER_REPO`]).
pub fn launcher_asset_url(tag: &str, name: &str) -> String {
    asset_url_in(LAUNCHER_REPO, tag, name)
}

/// Where a release's attached file in `repository` downloads from.
fn asset_url_in(repository: &str, tag: &str, name: &str) -> String {
    // Metadata supplies path components, never URL syntax or another repository.
    let component = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'@' | b'+' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    format!(
        "https://github.com/{repository}/releases/download/{}/{}",
        component(tag),
        component(name)
    )
}

/// The signed SHA-256 of `asset` in release `tag`, fetching its SHA256SUMS and SHA256SUMS.sig
/// with `fetch` (None: the release has no such file), or why the download must be refused.
pub fn digest(
    tag: &str,
    asset: &str,
    keys: &[PublicKey],
    fetch: impl Fn(&str) -> Result<Option<Vec<u8>>, String>,
) -> Result<String, String> {
    let sums = fetch(&asset_url(tag, SUMS))?;
    let signature = fetch(&asset_url(tag, SIGNATURE))?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    sunrise_launcher_release_verify::signed_digest(
        tag,
        asset,
        sums.as_deref(),
        signature.as_deref(),
        keys,
    )
}

/// Whether a check couldn't reach the release or actually rejected its manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LauncherCheckError {
    Unavailable(String),
    Rejected { tag: String, reason: String },
}

impl LauncherCheckError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(reason) | Self::Rejected { reason, .. } => reason,
        }
    }
}

impl std::fmt::Display for LauncherCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl serde::Serialize for LauncherCheckError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Keep the CLI's launcher_error JSON value a message, not an enum.
        serializer.serialize_str(self.message())
    }
}

/// The signed SHA-256 of `asset` in this launcher's own release `tag` ([`LAUNCHER_REPO`]), signed for the launcher's releases only ([`sunrise_launcher_release_verify::launcher_message`]), or why the update must be refused.
pub fn launcher_digest(
    tag: &str,
    asset: &str,
    keys: &[PublicKey],
    fetch: impl Fn(&str) -> Result<Option<Vec<u8>>, String>,
) -> Result<String, LauncherCheckError> {
    let sums =
        fetch(&asset_url_in(LAUNCHER_REPO, tag, SUMS)).map_err(LauncherCheckError::Unavailable)?;
    let signature = fetch(&asset_url_in(LAUNCHER_REPO, tag, SIGNATURE))
        .map_err(LauncherCheckError::Unavailable)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    sunrise_launcher_release_verify::launcher_digest(
        tag,
        asset,
        sums.as_deref(),
        signature.as_deref(),
        keys,
    )
    .map_err(|reason| LauncherCheckError::Rejected {
        tag: tag.to_owned(),
        reason,
    })
}

/// The version the title of the build published under `tag` names, on `tag`'s channel only
/// ([`build_digest`]): a Dev title under a Nightly tag is None, and the reverse.
pub fn title_version(tag: &str, title: &str) -> Option<String> {
    sunrise_launcher_release_verify::build_title_version(Channel::of(tag)?, title)
}

/// The signed SHA-256 of `asset` in the Nightly or Dev build published under `tag` (`nightly`, a
/// dated `nightly-<date>`, or `dev-<date>-<commit>`) that installs as `version`
/// (`<channel>-<date>-<commit>`, parsed strictly from its title by
/// `sunrise_launcher_release_verify::build_title_version`): its SHA256SUMS.v2.sig must sign the checksums for
/// exactly that version, so a build cannot be served as another build or on another channel.
/// The channel's SHA256SUMS.sig is not read.
///
/// The channel checked is the one `tag` is on ([`Channel::of`]): the launcher's choice, never the
/// release's title. Dev builds live at their exact version tag.
pub fn build_digest(
    tag: &str,
    version: &str,
    asset: &str,
    keys: &[PublicKey],
    fetch: impl Fn(&str) -> Result<Option<Vec<u8>>, String>,
) -> Result<String, String> {
    let channel = Channel::of(tag)
        .ok_or_else(|| crate::i18n::trf("{tag} is not a Sunrise release.", &[("tag", tag)]))?;
    let sums = fetch(&asset_url(tag, SUMS))?;
    let signature = fetch(&asset_url(tag, SIGNATURE_V2))?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    sunrise_launcher_release_verify::build_digest(
        channel,
        version,
        asset,
        sums.as_deref(),
        signature.as_deref(),
        keys,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_launcher_release_verify::{sha256_hex, sums_file, SigningKey};

    #[test]
    fn metadata_cannot_escape_the_fork_download_path() {
        assert_eq!(asset_url("v1/../../other?x#y", "a%b.zip"),
            "https://github.com/sunrise-org/sunrise/releases/download/v1%2F..%2F..%2Fother%3Fx%23y/a%25b.zip");
    }

    #[test]
    fn upstream_retains_its_release_key() {
        // With no key in release-keys.txt every release after the pinned ones would be refused:
        // this fails until the release environment's public key is listed (docs/security.md).
        assert!(
            !sunrise_launcher_release_verify::trusted_keys().is_empty(),
            "crates/release-verify/keys/sunrise-org-sunrise.txt lists no key"
        );
    }

    #[test]
    fn an_archive_without_a_signature_is_refused() {
        let tampered =
            |_: &str| -> Result<Option<Vec<u8>>, String> { Ok(Some(b"anything".to_vec())) };
        assert!(digest("v2026.10.12", "sunrise-linux-x86_64.tar.gz", &[], tampered).is_err());
    }

    #[test]
    fn only_archives_signed_by_a_built_in_key_install() {
        let (key, _) = SigningKey::generate().unwrap();
        let sums = sums_file(&[("sunrise.zip".into(), sha256_hex(b"game"))]);
        let signature = key.sign("v2099.01.01", sums.as_bytes());
        // GitHub as it answers for a signed release (any tag: the files are what a replay would serve).
        let signed = |url: &str| -> Result<Option<Vec<u8>>, String> {
            assert!(url.starts_with("https://github.com/sunrise-org/sunrise/releases/download/"));
            Ok(Some(
                if url.ends_with(".sig") {
                    signature.clone()
                } else {
                    sums.clone()
                }
                .into_bytes(),
            ))
        };
        let keys = [key.public()];
        assert_eq!(
            digest("v2099.01.01", "sunrise.zip", &keys, signed),
            Ok(sha256_hex(b"game"))
        );
        // Signed for another tag, by another key, or not listing the archive: refused.
        assert!(digest("v2099.01.02", "sunrise.zip", &keys, signed).is_err());
        let (other, _) = SigningKey::generate().unwrap();
        assert!(digest("v2099.01.01", "sunrise.zip", &[other.public()], signed).is_err());
        assert!(digest("v2099.01.01", "other.zip", &keys, signed).is_err());
        // No checksums (a release from before signing): refused, saying so.
        let unsigned = |_: &str| -> Result<Option<Vec<u8>>, String> { Ok(None) };
        let refused = digest("v2099.01.01", "sunrise.zip", &keys, unsigned).unwrap_err();
        assert!(refused.contains("not signed"), "{refused}");
        // Checksums that can't be fetched are an error too, never a pass.
        let offline = |_: &str| -> Result<Option<Vec<u8>>, String> { Err("No connection".into()) };
        assert!(digest("v2099.01.01", "sunrise.zip", &keys, offline).is_err());
    }

    #[test]
    fn stable_release_uses_sums_signed_for_its_exact_tag() {
        const TAG: &str = "26.3-0.1.0";
        const ASSET: &str = "sunrise-linux-x86_64.tar.gz";
        let (key, _) = SigningKey::generate().unwrap();
        let expected = sha256_hex(b"fixture archive");
        let sums = sums_file(&[
            (ASSET.into(), expected.clone()),
            ("sunrise-macos-arm64.zip".into(), sha256_hex(b"mac fixture")),
        ]);
        let signature = key.sign(TAG, sums.as_bytes());
        let fetch = |url: &str| {
            // Stable publishes both signature files, but signed_digest uses SHA256SUMS.sig.
            let file = url.rsplit('/').next().unwrap();
            assert!(url.starts_with("https://github.com/sunrise-org/sunrise/releases/download/"));
            Ok(Some(match file {
                SUMS => sums.as_bytes().to_vec(),
                SIGNATURE => signature.as_bytes().to_vec(),
                other => panic!("requested {other}"),
            }))
        };
        assert!(crate::backend::is_game_tag(TAG));
        assert_eq!(digest(TAG, ASSET, &[key.public()], fetch), Ok(expected));
        assert!(digest("26.3-0.1.1", ASSET, &[key.public()], fetch).is_err());
    }

    /// The release channel never installs what was signed for a Nightly, whatever tag it is served
    /// under; each pre-release installs only with its exact build signature (SHA256SUMS.v2.sig),
    /// never the channel's, another build's, or a stable release's.
    #[test]
    fn stable_nightly_and_dev_signatures_are_not_interchangeable() {
        const VERSION: &str = "nightly-2026-10-28-0c02067";
        let (key, _) = SigningKey::generate().unwrap();
        let keys = [key.public()];
        let sums = sums_file(&[("sunrise.zip".into(), sha256_hex(b"game"))]);
        // Serves SHA256SUMS, SHA256SUMS.sig signed for `channel` and SHA256SUMS.v2.sig for `v2`.
        let serving = |channel: &'static str, v2: &'static str| {
            let (one, two) = (
                key.sign(channel, sums.as_bytes()),
                key.sign(v2, sums.as_bytes()),
            );
            let sums = sums.clone();
            move |url: &str| -> Result<Option<Vec<u8>>, String> {
                Ok(Some(
                    if url.ends_with(SIGNATURE_V2) {
                        two.clone()
                    } else if url.ends_with(SIGNATURE) {
                        one.clone()
                    } else {
                        sums.clone()
                    }
                    .into_bytes(),
                ))
            }
        };
        let nightly = serving(sunrise_launcher_release_verify::NIGHTLY, VERSION);
        assert_eq!(
            build_digest("nightly", VERSION, "sunrise.zip", &keys, &nightly),
            Ok(sha256_hex(b"game"))
        );
        // Another version than the one signed (an older Nightly served as tonight's, or the reverse).
        assert!(build_digest(
            "nightly",
            "nightly-2026-10-29-1111111",
            "sunrise.zip",
            &keys,
            &nightly
        )
        .is_err());
        for tag in ["v0.2.81", "nightly", "nightly-2026-10-27"] {
            assert!(
                digest(tag, "sunrise.zip", &keys, &nightly).is_err(),
                "{tag}"
            );
        }
        // Only the channel's signature (a Nightly from before v2): refused by a current launcher.
        let old = |url: &str| -> Result<Option<Vec<u8>>, String> {
            if url.ends_with(SIGNATURE_V2) {
                Ok(None)
            } else {
                nightly(url)
            }
        };
        let refused = build_digest("nightly", VERSION, "sunrise.zip", &keys, old).unwrap_err();
        assert!(refused.contains("SHA256SUMS.v2.sig"), "{refused}");
        let release = serving("v0.2.81", "v0.2.81");
        assert!(digest("v0.2.81", "sunrise.zip", &keys, &release).is_ok());
        assert!(build_digest("nightly", VERSION, "sunrise.zip", &keys, &release).is_err());
        // Dev installs with its own version's signature; it never passes for a Nightly or Stable.
        const DEV: &str = "dev-2026-10-28-0c02067";
        let dev = serving(sunrise_launcher_release_verify::DEV, DEV);
        assert_eq!(
            build_digest(DEV, DEV, "sunrise.zip", &keys, &dev),
            Ok(sha256_hex(b"game"))
        );
        assert!(build_digest("nightly", VERSION, "sunrise.zip", &keys, &dev).is_err());
        assert!(build_digest(DEV, DEV, "sunrise.zip", &keys, &nightly).is_err());
        // A genuine Dev build served under the Nightly's tag: its title is no Nightly's, and its
        // version is refused on the Nightly channel before its (valid) signature is read.
        assert_eq!(title_version("nightly", "Dev 2026-10-28 · 0c02067"), None);
        assert_eq!(title_version(DEV, "Nightly 2026-10-28 · 0c02067"), None);
        assert_eq!(
            title_version(DEV, "Dev 2026-10-28 · 0c02067").as_deref(),
            Some(DEV)
        );
        assert!(build_digest("nightly", DEV, "sunrise.zip", &keys, &dev).is_err());
        assert!(build_digest(DEV, VERSION, "sunrise.zip", &keys, &nightly).is_err());
        for tag in [DEV, "dev"] {
            assert!(digest(tag, "sunrise.zip", &keys, &dev).is_err(), "{tag}");
        }
    }
}
