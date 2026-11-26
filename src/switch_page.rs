//! Who plays: the top bar's quick switch (a menu under the account chip) between the signed-in
//! Microsoft accounts and the offline names.
use super::*;

impl Launcher {
    pub(super) fn play_as(&mut self, uuid: &str) {
        if self.account_locked() {
            self.say_account_locked();
            return;
        }
        match Accounts::switch(uuid) {
            Ok(accounts) => self.accounts = accounts,
            Err(e) => self.sign_in_state = Some(SignInState::Done(Err(e))),
        }
    }

    /// Plays as an offline name, or as the game's own name: who plays is on disk at once,
    /// like [`Self::play_as`]. This fork's `dev_offline` (set from `OFFLINE_PLAY` at load)
    /// is the pass that lets a name play without an account that owns the game.
    pub(super) fn play_as_player(&mut self, who: account::Player) {
        if self.account_locked() {
            self.say_account_locked();
            return;
        }
        match Accounts::switch_player(who, self.settings.dev_offline) {
            Ok(accounts) => self.accounts = accounts,
            Err(e) => self.sign_in_state = Some(SignInState::Done(Err(e))),
        }
    }

    /// Keeps the offline name typed in Settings › Account and plays as it; a name that
    /// can't be one is refused the way a refused switch is (`play_as`).
    pub(super) fn add_offline_name(&mut self, name: String) {
        if self.account_locked() {
            self.say_account_locked();
            return;
        }
        match Accounts::switch_player(account::Player::Offline(name), self.settings.dev_offline) {
            Ok(accounts) => {
                self.accounts = accounts;
                self.offline_input.set(String::new());
            }
            Err(e) => self.sign_in_state = Some(SignInState::Done(Err(e))),
        }
    }

    /// Forgets an offline name, on disk at once.
    pub(super) fn forget_offline_name(&mut self, name: &str) {
        if self.account_locked() {
            self.say_account_locked();
            return;
        }
        match Accounts::forget_offline(name) {
            Ok(accounts) => self.accounts = accounts,
            Err(e) => self.sign_in_state = Some(SignInState::Done(Err(e))),
        }
    }
}
