//! Where secrets live: the SecretStore trait, the memory and time-bounded stores, the layered
//! keychain-then-file store, and which one this process uses.
use super::*;
use crate::backend::lock::recover;

/// Where secrets are kept: the OS keychain, a file of the launcher's own when there is none, or
/// memory in tests.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError>;
    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError>;
    fn delete(&self, key: &str) -> Result<(), StoreError>;
    /// False for memory: secrets are gone when the launcher closes.
    fn persistent(&self) -> bool;
    /// A shared lock for read/renew/write operations; memory stores need only the process lock.
    fn lock_file(&self) -> Option<PathBuf> {
        None
    }
    /// The in-process half of that lock. The system keychain is one per computer, so every
    /// handle to it shares the process-wide lock; a memory store's secrets are its own, and so
    /// is its lock: two of them (one per unit test) never wait for each other.
    fn operations(&self) -> &Mutex<()> {
        session::process_lock()
    }
}

/// Why a secret store couldn't read, keep or forget a secret, in the words the player sees. The
/// stores never tell one failure from another (a failing keychain is only ever fallen back from),
/// so this carries the message and nothing else; it turns back into that String unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for StoreError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for StoreError {
    fn from(message: &str) -> Self {
        Self(message.into())
    }
}

impl From<StoreError> for String {
    fn from(error: StoreError) -> Self {
        error.0
    }
}

#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, String>>, Mutex<()>);
impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(recover(&self.0).get(key).cloned())
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError> {
        recover(&self.0).insert(key.into(), secret.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), StoreError> {
        recover(&self.0).remove(key);
        Ok(())
    }
    fn persistent(&self) -> bool {
        false
    }
    fn operations(&self) -> &Mutex<()> {
        &self.1
    }
}

/// How long a call to the system keyring may take before the launcher gives up on it (#12179). An
/// unlock prompt the player is typing a password into counts as an answer still coming, so this is
/// longer than a working keyring ever takes, short enough that Play says what is wrong.
pub const KEYCHAIN_WAIT: Duration = Duration::from_secs(20);

/// What the launcher says when the system keyring didn't answer within `wait`.
pub fn keyring_no_answer(wait: Duration) -> String {
    crate::i18n::trf(
        "Your system keyring didn't answer within {seconds} seconds. Make sure it is running and unlocked, then restart the launcher.",
        &[("seconds", &wait.as_secs().max(1).to_string())],
    )
}

/// One call to a [`Bounded`] keyring: its answer, or that the caller gave up on it.
struct Call<T> {
    answer: Option<T>,
    gave_up: bool,
}

/// A keyring whose calls give up after `wait` (#12179): a Secret Service that never answers (a
/// stuck KWallet or GNOME Keyring daemon) otherwise holds the account lock forever, and Play stays
/// on "Starting…" with nothing said. Each call runs on a thread of its own; one given up on keeps
/// running there, and while it does, later calls fail at once instead of piling up behind it.
pub struct Bounded {
    keyring: std::sync::Arc<dyn SecretStore>,
    wait: Duration,
    /// Calls given up on that haven't come back yet.
    stuck: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Bounded {
    pub fn new(keyring: impl SecretStore + 'static, wait: Duration) -> Self {
        Self {
            keyring: std::sync::Arc::new(keyring),
            wait,
            stuck: Default::default(),
        }
    }

    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SecretStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        use std::sync::atomic::Ordering::SeqCst;
        let no_answer = || {
            let said = keyring_no_answer(self.wait);
            eprintln!("Sunrise: {said}");
            Err(said.into())
        };
        if self.stuck.load(SeqCst) > 0 {
            return no_answer();
        }
        let call = std::sync::Arc::new((
            Mutex::new(Call::<Result<T, StoreError>> {
                answer: None,
                gave_up: false,
            }),
            std::sync::Condvar::new(),
        ));
        let (keyring, stuck, theirs) = (self.keyring.clone(), self.stuck.clone(), call.clone());
        let spawned = std::thread::Builder::new()
            .name("keyring".into())
            .spawn(move || {
                let answer = f(keyring.as_ref());
                let (slot, done) = &*theirs;
                let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
                if slot.gave_up {
                    stuck.fetch_sub(1, SeqCst);
                } else {
                    slot.answer = Some(answer);
                    done.notify_one();
                }
            });
        if let Err(e) = spawned {
            return Err(e.to_string().into());
        }
        let (slot, done) = &*call;
        let slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        let (mut slot, _) = done
            .wait_timeout_while(slot, self.wait, |call| call.answer.is_none())
            .unwrap_or_else(|e| e.into_inner());
        if let Some(answer) = slot.answer.take() {
            return answer;
        }
        // Under the call's lock: the thread, when it ends, sees this and counts itself back.
        slot.gave_up = true;
        self.stuck.fetch_add(1, SeqCst);
        drop(slot);
        no_answer()
    }
}

impl SecretStore for Bounded {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let key = key.to_string();
        self.call(move |keyring| keyring.get(&key))
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError> {
        let (key, secret) = (key.to_string(), secret.to_string());
        self.call(move |keyring| keyring.set(&key, &secret))
    }
    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let key = key.to_string();
        self.call(move |keyring| keyring.delete(&key))
    }
    fn persistent(&self) -> bool {
        self.keyring.persistent()
    }
    fn lock_file(&self) -> Option<PathBuf> {
        self.keyring.lock_file()
    }
    fn operations(&self) -> &Mutex<()> {
        self.keyring.operations()
    }
}

pub const NO_KEYCHAIN: &str = crate::i18n::tr_key("No keychain available: you are signed in until the launcher closes, then you will have to sign in again.");

/// What the keychain answered when [`store`] fell back to memory.
static NO_KEYCHAIN_WHY: OnceLock<String> = OnceLock::new();

/// What the window and the CLI say when sign-ins last only this run: [`NO_KEYCHAIN`] and why.
/// None while the keychain keeps them.
pub fn no_keychain() -> Option<String> {
    if store().persistent() {
        return None;
    }
    use crate::i18n::{tr, trf};
    Some(match NO_KEYCHAIN_WHY.get() {
        Some(why) => format!(
            "{} {}",
            tr(NO_KEYCHAIN),
            trf("The keychain said: {why}", &[("why", why)])
        ),
        None => tr(NO_KEYCHAIN).into(),
    })
}

/// What a keychain this run stopped asking answers.
pub(super) const KEYCHAIN_AWAY: &str =
    "The keychain didn't answer earlier: sign in again to keep this account.";

/// The keychain, with the secrets file for what it can't keep (#5095). Each call asks both, never a
/// choice made once: a launcher that wrote to the file while the keychain was away still finds its
/// secrets after another process handed them to the keychain. Reads take the file's copy first (it
/// is the newer one), writes go to the keychain and to the file only when it refuses.
///
/// The file's lock is never held across a keychain call, which can wait on an unlock prompt: other
/// processes keep reading and writing the file meanwhile. Every write to the keychain holds a lock of
/// its own instead (`account-secrets.keychain.lock`), with what it writes read under it, so a hand-over
/// never puts back a secret another process renewed or signed out since; only keychain writes wait
/// on a prompt, as they would on the keychain itself. A file entry is only removed if it still holds
/// what was read before the keychain call, so another process's newer write stays. And once the
/// keychain refuses or doesn't answer, this run stops asking it (no prompt after a dismissed
/// prompt), until another process hands it a secret this run saw in the file.
pub struct Layered {
    pub(super) keychain: Box<dyn SecretStore>,
    pub(super) file: FileStore,
    pub(super) away: std::sync::atomic::AtomicBool,
    /// What this run saw in the file and didn't hand over itself: put there, read there, or there
    /// when it started. Gone from the file later, the keychain answers again (another process
    /// handed it).
    pub(super) parked: Mutex<std::collections::HashSet<String>>,
}

impl Layered {
    /// Starts by handing the keychain what the file kept while it was away, one secret at a time,
    /// up to the first it refuses; sign-outs it missed are done there too. Each is read again under
    /// the keychain's lock: one another process renewed or signed out meanwhile isn't handed over.
    pub fn new(keychain: Box<dyn SecretStore>, file: FileStore) -> Self {
        let layered = Self {
            keychain,
            file,
            away: Default::default(),
            parked: Default::default(),
        };
        let keys: Vec<String> = layered
            .file
            .entries()
            .unwrap_or_default()
            .into_keys()
            .collect();
        // Seen before the hand-over starts: whatever another process hands over meanwhile, this
        // run asks the keychain for it again.
        recover(&layered.parked).extend(keys.iter().cloned());
        for key in keys {
            let handed = layered.keychain_writes(|| {
                let Some(secret) = layered.file.entry(&key)? else {
                    return Ok(());
                };
                layered.ask(|keychain| match secret.as_str() {
                    SIGNED_OUT => keychain.delete(&key),
                    _ => keychain.set(&key, &secret),
                })?;
                layered.unpark(&key, Some(secret))
            });
            if handed.is_err() {
                break;
            }
        }
        layered
    }

    /// Runs `f`, a write to the keychain and what it reads first, holding the keychain's lock.
    fn keychain_writes<T>(
        &self,
        f: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.file.holding("keychain.lock", f)
    }

    fn is_away(&self) -> bool {
        self.away.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Asks the keychain unless it was away; an error means it is away for the rest of the run.
    fn ask<T>(
        &self,
        call: impl FnOnce(&dyn SecretStore) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if self.is_away() {
            return Err(crate::i18n::tr(KEYCHAIN_AWAY).into());
        }
        let answer = call(self.keychain.as_ref());
        if answer.is_err() {
            self.away.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        answer
    }

    /// Puts `value` in the file for `key`, the keychain being away.
    fn park(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.file.put(key, value)?;
        recover(&self.parked).insert(key.into());
        Ok(())
    }

    /// The keychain took `key`: the file's copy read before goes, unless another process wrote since.
    /// Still in the file, it stays counted as seen there, so its hand-over by another process is
    /// noticed.
    fn unpark(&self, key: &str, before: Option<String>) -> Result<(), StoreError> {
        let still_there = self.file.settle(key, before.as_deref())?;
        let mut parked = recover(&self.parked);
        if still_there {
            parked.insert(key.into());
        } else {
            parked.remove(key);
        }
        Ok(())
    }
}

impl SecretStore for Layered {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        match self.file.entry(key)? {
            Some(secret) if secret == SIGNED_OUT => return Ok(None),
            Some(secret) => {
                recover(&self.parked).insert(key.into());
                return Ok(Some(secret));
            }
            None => {}
        }
        // Seen in the file and gone from it: another process handed it to the keychain, which
        // answers again.
        if self.is_away() && recover(&self.parked).remove(key) {
            self.away.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        self.ask(|keychain| keychain.get(key))
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError> {
        if self.is_away() {
            return self.park(key, secret);
        }
        self.keychain_writes(|| {
            let before = self.file.entry(key)?;
            match self.ask(|keychain| keychain.set(key, secret)) {
                Ok(()) => self.unpark(key, before),
                Err(_) => self.park(key, secret),
            }
        })
    }
    fn delete(&self, key: &str) -> Result<(), StoreError> {
        // The keychain may hold a copy: the mark hides it until it can go.
        if self.is_away() {
            return self.park(key, SIGNED_OUT);
        }
        self.keychain_writes(|| {
            let before = self.file.entry(key)?;
            match self.ask(|keychain| keychain.delete(key)) {
                Ok(()) => self.unpark(key, before),
                Err(_) => self.park(key, SIGNED_OUT),
            }
        })
    }
    fn persistent(&self) -> bool {
        true
    }
}

/// The OS keychain if it answers, else why not.
fn keychain_or_why() -> Result<Box<dyn SecretStore>, StoreError> {
    // Unit tests never reach the player's keychain.
    #[cfg(all(
        not(test),
        any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "macos",
            target_os = "windows"
        )
    ))]
    {
        if std::env::var_os("SUNRISE_NO_KEYCHAIN").is_some() {
            return Err("SUNRISE_NO_KEYCHAIN is set.".into());
        }
        let keychain = Bounded::new(SystemStore, KEYCHAIN_WAIT);
        keychain.get("probe").map(|_| Box::new(keychain) as _)
    }
    #[cfg(not(all(
        not(test),
        any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "macos",
            target_os = "windows"
        )
    )))]
    Err(crate::i18n::tr("This system has no keychain.").into())
}

/// Microsoft sign-ins. Linux and FreeBSD: the Secret Service with the secrets file beside it
/// ([`Layered`], #5095). macOS and Windows: the keychain if it answers. Memory with
/// `SUNRISE_NO_KEYCHAIN` (the launcher's own tests) or no keychain on macOS and Windows (said in the
/// UI with [`no_keychain`]).
pub fn store() -> &'static dyn SecretStore {
    static STORE: OnceLock<Box<dyn SecretStore>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            #[cfg(all(not(test), any(target_os = "linux", target_os = "freebsd")))]
            if std::env::var_os("SUNRISE_NO_KEYCHAIN").is_none() {
                return Box::new(Layered::new(
                    Box::new(Bounded::new(SystemStore, KEYCHAIN_WAIT)),
                    FileStore::launcher(),
                ));
            }
            keychain_or_why().unwrap_or_else(|why| {
                let _ = NO_KEYCHAIN_WHY.set(why.into());
                Box::<MemoryStore>::default()
            })
        })
        .as_ref()
}

/// Secrets never written to a file of the launcher's own, the Tailscale access token (it
/// administers the player's whole tailnet): the keychain if it answers, else memory for this run.
/// [`store`] itself on macOS and Windows, where that is what it is.
pub fn keychain() -> &'static dyn SecretStore {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        static KEYCHAIN: OnceLock<Box<dyn SecretStore>> = OnceLock::new();
        KEYCHAIN
            .get_or_init(|| keychain_or_why().unwrap_or_else(|_| Box::<MemoryStore>::default()))
            .as_ref()
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    store()
}

/// Whether sign-ins sit in the secrets file now, the keychain having refused them (#5095): sign-out
/// marks alone, or a file that can't be read, don't count.
pub fn in_secrets_file() -> bool {
    cfg!(any(target_os = "linux", target_os = "freebsd"))
        && store().persistent()
        && FileStore::launcher().holds_secrets()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic while the store's map is held poisons its lock; reading and writing must still work.
    #[test]
    fn a_poisoned_memory_store_still_answers() {
        let store = std::sync::Arc::new(MemoryStore::default());
        let poisoned = store.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoned.0.lock().unwrap();
            panic!("poison the memory store's lock on purpose");
        })
        .join();
        assert_eq!(store.set("key", "secret"), Ok(()));
        assert_eq!(store.get("key"), Ok(Some("secret".into())));
    }
}
