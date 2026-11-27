//! The account's player certificate: the key pair the Minecraft services issue each account and
//! sign with Mojang's key (the key the game signs chat with). The launcher signs registry ratings
//! with it, so the registry learns who rated from Mojang's signature and never sees a token.
//! The key lives in memory only, as the game keeps it (`AccountProfileKeyPairManager`).
use super::{bearer, request_to, send, str_field, Endpoints};
use crate::backend::lock::recover;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A certificate is asked for again no sooner than this after the last time, unless it has
/// expired (vanilla's `AccountProfileKeyPairManager.MINIMUM_PROFILE_KEY_REFRESH_INTERVAL`).
const MINIMUM_REFRESH_INTERVAL: Duration = Duration::from_secs(3600);

/// One account's certificate and its private key.
pub struct PlayerKey {
    /// The account's UUID, 32 hex digits.
    pub uuid: String,
    /// `expiresAt`, in milliseconds since the epoch: the number Mojang signed.
    pub expires_at: i64,
    /// `refreshedAfter`, in milliseconds: from then on the services offer a new one.
    refreshed_after: i64,
    /// `keyPair.publicKey` as the services answered it.
    public_key: String,
    /// `publicKeySignatureV2`, base64.
    signature_v2: String,
    private_key: ring::signature::RsaKeyPair,
}

impl std::fmt::Debug for PlayerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlayerKey({}, <hidden>)", self.uuid)
    }
}

/// An ISO-8601 instant as epoch milliseconds, truncated as `Instant.toEpochMilli` does.
fn epoch_millis(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// The bytes inside a PEM block. Base64 characters only, as the game's MIME decoder reads them
/// (`Crypt.stringToPemRsaPrivateKey`).
fn pem_bytes(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .skip_while(|line| !line.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|line| !line.starts_with("-----END"))
        .flat_map(str::chars)
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    STANDARD.decode(body).ok()
}

impl PlayerKey {
    /// Reads `POST /player/certificates`' answer for the account `uuid`. The private key is the
    /// PKCS#8 bytes the services wrap in an `RSA PRIVATE KEY` block.
    pub fn parse(uuid: &str, answer: &Value) -> Result<Self, String> {
        let unreadable =
            || crate::i18n::tr("The Minecraft services' answer couldn't be read.").to_string();
        let pair = &answer["keyPair"];
        let private_key = str_field(pair, "privateKey")
            .and_then(|pem| pem_bytes(&pem))
            .and_then(|der| ring::signature::RsaKeyPair::from_pkcs8(&der).ok())
            .ok_or_else(unreadable)?;
        let field = |v: &Value, key| str_field(v, key).ok_or_else(unreadable);
        let instant = |key| {
            str_field(answer, key)
                .and_then(|t| epoch_millis(&t))
                .ok_or_else(unreadable)
        };
        Ok(Self {
            uuid: uuid.to_owned(),
            expires_at: instant("expiresAt")?,
            refreshed_after: instant("refreshedAfter")?,
            public_key: field(pair, "publicKey")?,
            signature_v2: field(answer, "publicKeySignatureV2")?,
            private_key,
        })
    }

    pub fn expired(&self, now_ms: i64) -> bool {
        self.expires_at <= now_ms
    }

    /// Whether to ask the services for a new certificate, `held_for` after this one came.
    fn stale(&self, now_ms: i64, held_for: Duration) -> bool {
        self.expired(now_ms)
            || (now_ms >= self.refreshed_after && held_for >= MINIMUM_REFRESH_INTERVAL)
    }

    /// The certificate's public part, as the registry reads it.
    pub fn certificate(&self) -> Value {
        json!({
            "uuid": self.uuid,
            "expires_at": self.expires_at,
            "key": self.public_key,
            "signature_v2": self.signature_v2,
        })
    }

    /// `bytes` signed with the private key, RSA PKCS#1 v1.5 with SHA-256 as the game signs with
    /// a player key (`ProfilePublicKey.createSignatureValidator`), in base64.
    pub fn sign(&self, bytes: &[u8]) -> Result<String, String> {
        let mut signature = vec![0; self.private_key.public().modulus_len()];
        self.private_key
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                bytes,
                &mut signature,
            )
            .map_err(|_| crate::i18n::tr("The rating couldn't be signed.").to_string())?;
        Ok(STANDARD.encode(signature))
    }

    #[cfg(test)]
    pub(crate) fn verifies(&self, bytes: &[u8], signature: &str) -> bool {
        use ring::signature::KeyPair;
        let Ok(signature) = STANDARD.decode(signature) else {
            return false;
        };
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            self.private_key.public_key().as_ref(),
        )
        .verify(bytes, &signature)
        .is_ok()
    }
}

/// Asks the services for the account's certificate: a `POST` with no body, as the game does.
fn fetch(ep: &Endpoints, token: &str, uuid: &str) -> Result<PlayerKey, String> {
    let unreachable = crate::i18n::tr("Couldn't reach Minecraft's services.");
    let request = bearer(
        request_to("POST", &format!("{}/player/certificates", ep.minecraft))?,
        token,
    );
    let (status, answer) = send(
        request.set("Content-Length", "0"),
        None,
        "Minecraft's services",
    )
    .map_err(|_| unreachable.to_string())?;
    if status != 200 {
        return Err(unreachable.into());
    }
    PlayerKey::parse(uuid, &answer)
}

/// The certificate held for the selected account, and when it came.
static HELD: Mutex<Option<(Arc<PlayerKey>, Instant)>> = Mutex::new(None);

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The certificate of `account`, which must still be the selected one: the one held when it is
/// still good, else a new one from the services with the account's Minecraft token (renewed first
/// as a launch renews it). Offline play and accounts without the game get none. A held
/// certificate that isn't expired yet is kept when a new one can't be had, as the game keeps its
/// key pair (`AccountProfileKeyPairManager.readOrFetchProfileKeyPair`), and asked for again only
/// after the refresh interval.
pub fn current(account: &str) -> Result<Arc<PlayerKey>, String> {
    let held = HELD
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(key, _)| key.uuid == account)
        .map(|(key, came)| (key.clone(), came.elapsed()));
    if let Some((key, held_for)) = &held {
        if !key.stale(now_ms(), *held_for) {
            return Ok(key.clone());
        }
    }
    let fetched = (|| {
        let mut accounts = super::Accounts::load();
        if accounts.selected().map(|a| a.uuid.as_str()) != Some(account) {
            return Err(crate::i18n::tr("Another account plays now. Rate again.").to_string());
        }
        let ep = Endpoints::live();
        let launch = super::launch_for_game(
            &ep,
            super::client_id().as_deref(),
            super::store(),
            &mut accounts,
            // A profile key needs the account's real session: the gate stays strict.
            false,
        )
        .map_err(|_| crate::i18n::tr(super::NEEDS_OWNER).to_string())?;
        let _ = accounts.save_renewed();
        let (selected, secrets) = launch.session.ok_or_else(|| {
            launch
                .offline
                .unwrap_or_else(|| crate::i18n::tr(super::NEEDS_OWNER).to_string())
        })?;
        fetch(&ep, &secrets.minecraft_token, &selected.uuid)
    })();
    let key = match (fetched, held) {
        (Ok(key), _) => Arc::new(key),
        (Err(_), Some((key, _))) if !key.expired(now_ms()) => key,
        (Err(why), _) => return Err(why),
    };
    *recover(&HELD) = Some((key.clone(), Instant::now()));
    Ok(key)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A certificate answer as the services give one, around the test key: a throwaway key made
    /// for tests (the registry's own test player key), never a real account's.
    pub(crate) fn answer(expires_at: &str, refreshed_after: &str) -> Value {
        let pem = include_str!("../../tests/fixtures/test-player-key.pem");
        let body = pem
            .lines()
            .skip_while(|l| !l.starts_with("-----BEGIN"))
            .skip(1)
            .take_while(|l| !l.starts_with("-----END"))
            .collect::<String>();
        json!({
            "keyPair": {
                "privateKey": format!("-----BEGIN RSA PRIVATE KEY-----\n{body}\n-----END RSA PRIVATE KEY-----\n"),
                "publicKey": "-----BEGIN RSA PUBLIC KEY-----\nAAAA\n-----END RSA PUBLIC KEY-----\n",
            },
            "publicKeySignatureV2": "c2lnbmVk",
            "expiresAt": expires_at,
            "refreshedAfter": refreshed_after,
        })
    }

    pub(crate) fn key() -> PlayerKey {
        PlayerKey::parse(
            "0123456789abcdef0123456789abcdef",
            &answer("2026-11-24T19:00:00.123456Z", "2026-11-24T11:00:00Z"),
        )
        .unwrap()
    }

    #[test]
    fn a_certificate_answer_gives_the_signed_expiry_and_a_key_that_signs() {
        let key = key();
        // 2026-11-24T19:00:00Z is 1795546800 s; the microseconds are truncated to milliseconds.
        assert_eq!(key.expires_at, 1_795_546_800_123);
        assert_eq!(
            key.certificate(),
            json!({
                "uuid": "0123456789abcdef0123456789abcdef",
                "expires_at": 1_795_546_800_123i64,
                "key": "-----BEGIN RSA PUBLIC KEY-----\nAAAA\n-----END RSA PUBLIC KEY-----\n",
                "signature_v2": "c2lnbmVk",
            })
        );
        let signature = key.sign(b"sunrise-rating-v1\nrate").unwrap();
        assert!(key.verifies(b"sunrise-rating-v1\nrate", &signature));
        assert!(!key.verifies(b"sunrise-rating-v1\nread", &signature));
        let mut broken = answer("2026-11-24T19:00:00Z", "2026-11-24T11:00:00Z");
        broken["keyPair"]["privateKey"] =
            json!("-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----");
        assert!(PlayerKey::parse("u", &broken).is_err());
    }

    #[test]
    fn a_certificate_is_asked_for_again_once_expired_or_after_its_refresh_time_and_an_hour() {
        let key = key();
        let refresh = 1_795_518_000_000; // 2026-11-24T11:00:00Z
        let hour = MINIMUM_REFRESH_INTERVAL;
        assert!(!key.stale(refresh - 1, hour));
        assert!(!key.stale(refresh, hour - Duration::from_secs(1)));
        assert!(key.stale(refresh, hour));
        assert!(key.stale(key.expires_at, Duration::ZERO));
    }
}
