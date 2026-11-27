//! A sign-in on its own thread, from the device code to the saved account.
use super::*;
use crate::backend::lock::recover;

/// Called under the account-operation lock, after the final profile is known. Rejected or
/// cancelled sign-ins must not save a credential, add another identity, or change selection.
pub(super) fn accept_sign_in(
    accounts: &mut Accounts,
    store: &dyn SecretStore,
    expected: Option<&Account>,
    account: Account,
    secrets: &Secrets,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(session::Phase),
) -> Result<(), String> {
    if cancelled() {
        return Err(crate::i18n::tr("Sign-in cancelled.").into());
    }
    if let Some(expected) = expected {
        if expected.uuid != account.uuid {
            return Err(crate::i18n::trf(
                "Signed in as {signed_in} instead of {expected}. Use the Microsoft account for {expected} and try again.",
                &[("signed_in", &account.name), ("expected", &expected.name)],
            ));
        }
        if accounts.find(&expected.uuid).is_none() {
            return Err(crate::i18n::tr(
                "That account was signed out while sign-in was in progress. Add it again to continue.",
            )
            .into());
        }
    }
    let selected = accounts.selected.clone();
    progress(session::Phase::Saving);
    accounts.keep_sign_in(store, account, secrets)?;
    if expected.is_some() {
        accounts.selected = selected;
    }
    Ok(())
}

/// Where a background sign-in is (the window and the MCP server show it).
#[derive(Clone, Debug, PartialEq)]
pub enum SignInState {
    Starting,
    /// Waiting for the player at `verification_uri` with `user_code`.
    Code(DeviceCode),
    Working(session::Phase),
    Done(Result<Account, String>),
}

/// A sign-in on its own thread, from the code to the saved account.
pub struct SignIn {
    state: std::sync::Arc<Mutex<SignInState>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SignIn {
    pub fn start() -> Self {
        Self::start_expected(None)
    }

    /// Restore this exact Minecraft identity without changing who currently plays.
    pub fn start_for(account: Account) -> Self {
        Self::start_expected(Some(account))
    }

    fn start_expected(expected: Option<Account>) -> Self {
        use std::sync::atomic::Ordering;
        let state = std::sync::Arc::new(Mutex::new(SignInState::Starting));
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (thread_state, thread_cancel) = (state.clone(), cancel.clone());
        std::thread::spawn(move || {
            let set = |s| *recover(&thread_state) = s;
            let result = (|| {
                let ep = Endpoints::live();
                let client_id = client_id().ok_or(NOT_CONFIGURED)?;
                let code = device_code(&ep, &client_id)?;
                set(SignInState::Code(code.clone()));
                let ms = wait_for_sign_in(&ep, &client_id, &code, &|| {
                    thread_cancel.load(Ordering::Relaxed)
                })?;
                let progress = |phase| set(SignInState::Working(phase));
                let (account, secrets) = finish_sign_in_with_progress(&ep, ms, &progress)?;
                progress(session::Phase::Queued);
                let store = store();
                let _operation = session::lock(store)?;
                let mut accounts = Accounts::load();
                accept_sign_in(
                    &mut accounts,
                    store,
                    expected.as_ref(),
                    account.clone(),
                    &secrets,
                    &|| thread_cancel.load(Ordering::Relaxed),
                    &progress,
                )?;
                progress(session::Phase::Saving);
                accounts.save()?;
                Ok(account)
            })();
            set(SignInState::Done(result));
        });
        Self { state, cancel }
    }
    pub fn state(&self) -> SignInState {
        recover(&self.state).clone()
    }
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
