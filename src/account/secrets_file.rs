//! The launcher's own secrets file, sealed with a key of this computer's, for when no keychain answers.
use super::*;

/// The secrets file in the launcher's data folder, when no keychain answers (#5095): many Linux
/// systems run no Secret Service, and sign-ins would otherwise last one run. One JSON object, key
/// to secret, encrypted with a key of this computer's ([`seal`]), readable and writable by the
/// player only (0600 on Unix), replaced whole on every write. A lock file beside it keeps each read-modify-replace whole across every launcher and CLI
/// process. The keychain takes its secrets back at the first start it answers ([`Layered`]).
pub struct FileStore {
    path: PathBuf,
}

/// The secrets file's name in the launcher's data folder.
pub const SECRETS_FILE: &str = "account-secrets.json";

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The launcher's own: [`SECRETS_FILE`] in its data folder.
    pub fn launcher() -> Self {
        Self::new(crate::backend::data_dir().join(SECRETS_FILE))
    }

    fn shown(&self, e: std::io::Error) -> String {
        format!("{}: {e}", self.path.display())
    }

    /// Runs `f` holding the file's lock (`account-secrets.lock`): other processes wait.
    fn locked<T>(&self, f: impl FnOnce() -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.holding("lock", f)
    }

    /// Runs `f` holding the lock file `account-secrets.<name>`, made if need be.
    pub(super) fn holding<T>(
        &self,
        name: &str,
        f: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| self.shown(e))?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension(name))
            .map_err(|e| self.shown(e))?;
        lock.lock().map_err(|e| self.shown(e))?;
        // Closing the file releases the lock, after `f` whatever it answers.
        f()
    }

    /// Whether the file holds a sign-in: not only sign-out marks, and readable.
    pub(super) fn holds_secrets(&self) -> bool {
        self.entries()
            .is_ok_and(|secrets| secrets.values().any(|secret| secret != SIGNED_OUT))
    }

    fn read(&self) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
        match std::fs::read(&self.path) {
            // Only where the file breaks: serde's own message can quote what the file holds.
            Ok(bytes) => {
                let (json, old) = self.unseal(&bytes)?.ok_or_else(|| {
                    format!(
                        "{} can't be read on this computer: it was written on another one, or changed. \
                         Remove it and sign in again.",
                        self.path.display()
                    )
                })?;
                let secrets = serde_json::from_slice(&json).map_err(|e| {
                    crate::i18n::trf(
                        "{file} can't be read ({kind} error at line {line}, column {column}).",
                        &[
                            ("file", &self.path.display().to_string()),
                            ("kind", &format!("{:?}", e.classify())),
                            ("line", &e.line().to_string()),
                            ("column", &e.column().to_string()),
                        ],
                    )
                })?;
                // Sealed under the old key: sealed again under this install's. Every read runs
                // under the lock; a write that fails leaves the old file, read again next time.
                if old {
                    let _ = self.write(&secrets);
                }
                Ok(secrets)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(e) => Err(self.shown(e).into()),
        }
    }

    /// Replaces the file with `secrets` through a temporary file of this write's own beside it, so
    /// a crash leaves the old file or the new one; removes it when there are none left.
    fn write(
        &self,
        secrets: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), StoreError> {
        if secrets.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(self.shown(e).into()),
                _ => Ok(()),
            };
        }
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self
            .path
            .with_extension(format!("json.{}.{n}.tmp", std::process::id()));
        let json = serde_json::to_vec(secrets).map_err(|e| e.to_string())?;
        let sealed = self.seal(&json)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options.open(&tmp).and_then(|mut file| {
            std::io::Write::write_all(&mut file, &sealed)?;
            file.sync_all()
        });
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &self.path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(self.shown(e).into());
        }
        Ok(())
    }

    /// The file's entry for `key`, if the file is there.
    pub(super) fn entry(&self, key: &str) -> Result<Option<String>, StoreError> {
        if !self.path.exists() {
            return Ok(None);
        }
        self.locked(|| Ok(self.read()?.remove(key)))
    }

    /// Everything the file holds, if it is there.
    pub(super) fn entries(&self) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
        if !self.path.exists() {
            return Ok(Default::default());
        }
        self.locked(|| self.read())
    }

    pub(super) fn put(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.locked(|| {
            let mut secrets = self.read()?;
            secrets.insert(key.into(), value.into());
            self.write(&secrets)
        })
    }

    /// The keychain took `key`: removes its entry if it still holds `before`, what was read before
    /// the keychain call. Whether the file still has an entry for it: one another process wrote
    /// since stays, and is read first.
    pub(super) fn settle(&self, key: &str, before: Option<&str>) -> Result<bool, StoreError> {
        if !self.path.exists() {
            return Ok(false);
        }
        self.locked(|| {
            let mut secrets = self.read()?;
            if before.is_some() && secrets.get(key).map(String::as_str) == before {
                secrets.remove(key);
                self.write(&secrets)?;
                return Ok(false);
            }
            Ok(secrets.contains_key(key))
        })
    }

    /// The install's own secret beside the file (`account-secrets.key`): 32 random bytes, readable
    /// and writable by the player only, made at the first write when `make`. `None` when it isn't
    /// there and not made.
    fn install_secret(&self, make: bool) -> Result<Option<[u8; 32]>, StoreError> {
        let path = self.path.with_extension("key");
        match std::fs::read(&path) {
            Ok(bytes) => <[u8; 32]>::try_from(bytes).map(Some).map_err(|_| {
                crate::i18n::trf(
                    "{key} is damaged. Remove it and {file} and sign in again.",
                    &[
                        ("key", &path.display().to_string()),
                        ("file", &self.path.display().to_string()),
                    ],
                )
                .into()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && make => {
                let mut secret = [0u8; 32];
                getrandom::fill(&mut secret).map_err(|e| e.to_string())?;
                // Whole or not at all, as the secrets file: a temporary file, then a rename.
                let tmp = path.with_extension(format!("key.{}.tmp", std::process::id()));
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                let written = options.open(&tmp).and_then(|mut file| {
                    std::io::Write::write_all(&mut file, &secret)?;
                    file.sync_all()
                });
                if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &path)) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!("{}: {e}", path.display()).into());
                }
                Ok(Some(secret))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display()).into()),
        }
    }

    /// `json` sealed for the file ([`SEALED`]), under [`file_key`] with this install's secret.
    pub(super) fn seal(&self, json: &[u8]) -> Result<Vec<u8>, StoreError> {
        use chacha20poly1305::{aead::Aead, KeyInit};
        let key = file_key(&self.install_secret(true)?.expect("made"));
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        let sealed = chacha20poly1305::ChaCha20Poly1305::new(&key)
            .encrypt(&nonce.into(), json)
            .map_err(|_| "the secrets couldn't be sealed".to_string())?;
        Ok([SEALED, &nonce, &sealed].concat())
    }

    /// What [`Self::seal`] sealed, and whether it was sealed in format 1 ([`SEALED_V1`]), to seal
    /// again; `None` for a file in another format, changed, sealed on another computer or by
    /// another install, or whose install secret is gone.
    fn unseal(&self, bytes: &[u8]) -> Result<Option<(Vec<u8>, bool)>, StoreError> {
        use chacha20poly1305::{aead::Aead, KeyInit};
        let (key, old, rest) = if let Some(rest) = bytes.strip_prefix(SEALED) {
            let Some(secret) = self.install_secret(false)? else {
                return Ok(None);
            };
            (file_key(&secret), false, rest)
        } else if let Some(rest) = bytes.strip_prefix(SEALED_V1) {
            (file_key_v1(), true, rest)
        } else {
            return Ok(None);
        };
        let Some((nonce, sealed)) = rest.split_at_checked(12) else {
            return Ok(None);
        };
        let nonce: [u8; 12] = nonce.try_into().expect("12 bytes");
        Ok(chacha20poly1305::ChaCha20Poly1305::new(&key)
            .decrypt(&nonce.into(), sealed)
            .ok()
            .map(|json| (json, old)))
    }
}

/// The secrets file's first bytes: format 2, the JSON sealed with ChaCha20-Poly1305 under
/// [`file_key`], then a random 12-byte nonce and the ciphertext with its tag.
pub(super) const SEALED: &[u8] = b"HZSECv2\n";

/// Format 1, sealed under [`file_key_v1`]: still read, and sealed again in format 2 at once.
const SEALED_V1: &[u8] = b"HZSECv1\n";

/// This computer's identity: `/etc/machine-id` on Linux, `/etc/hostid` on FreeBSD.
fn machine_id() -> Option<Vec<u8>> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id", "/etc/hostid"]
        .iter()
        .find_map(|path| std::fs::read(path).ok())
        .map(|id| id.trim_ascii().to_vec())
}

/// The secrets file's key: a constant of the launcher's, this computer's identity and the install's
/// own random secret (`account-secrets.key`, 0600). The tokens are never on disk in plain text,
/// and the key is never something every local user can work out (the machine id is readable by
/// all, and some systems have none): it takes the install secret, which only the player can read.
/// The file's mode (0600) is still what keeps other users out; the secret means a copy of the file
/// alone, or one left readable by others while the secret wasn't, can't be read either.
fn file_key(install_secret: &[u8; 32]) -> chacha20poly1305::Key {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    hash.update(b"sunrise-launcher account-secrets v2");
    let id = machine_id().unwrap_or_default();
    hash.update((id.len() as u64).to_le_bytes());
    hash.update(&id);
    hash.update(install_secret);
    hash.finalize()
}

/// Format 1's key, only to read a file sealed before format 2: the constant and the machine id,
/// which anyone on the computer can read (the bare constant where there is none).
pub(super) fn file_key_v1() -> chacha20poly1305::Key {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    hash.update(b"sunrise-launcher account-secrets v1");
    if let Some(id) = machine_id() {
        hash.update(id);
    }
    hash.finalize()
}

/// A file entry for an account signed out while the keychain was away: the keychain's copy, if any,
/// counts as gone, and goes at the first start it answers.
pub(super) const SIGNED_OUT: &str = "";

impl SecretStore for FileStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.locked(|| Ok(self.read()?.remove(key)))
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError> {
        self.locked(|| {
            let mut secrets = self.read()?;
            secrets.insert(key.into(), secret.into());
            self.write(&secrets)
        })
    }
    fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.locked(|| {
            let mut secrets = self.read()?;
            if secrets.remove(key).is_none() {
                return Ok(());
            }
            self.write(&secrets)
        })
    }
    fn persistent(&self) -> bool {
        true
    }
}
