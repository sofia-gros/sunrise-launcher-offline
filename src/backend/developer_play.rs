//! Account and unsigned-developer policy for CLI Play.
use super::{
    append_game_args, client_command, complete_assets, game_architecture_launch_error, game_home,
    need_prepared, play_step, SIGN_IN_HINT,
};
use crate::account;
use std::process::Command;

/// [`client`], playing as `who` for this launch only (`play --account`).
pub fn client_as(tag: &str, who: Option<&str>) -> Result<Command, String> {
    if let Some(error) = game_architecture_launch_error(tag) {
        return Err(error);
    }
    play_step("reading the sign-in");
    let hint = |refused: String| format!("{refused} {}", crate::i18n::tr(SIGN_IN_HINT));
    let no_owner = |refused: account::NoOwner| {
        // A keyring that didn't answer isn't a missing sign-in (#12179).
        match refused.keyring {
            Some(why) => why,
            None => hint(refused.to_string()),
        }
    };
    let session = if crate::backend::OFFLINE_PLAY && who.is_none() {
        // This fork: an offline name (or the game's own name) plays without an account that
        // owns the game. No developer mark on the command: `--developer-single-player`
        // would bar LAN play and offline servers, which offline names keep.
        Some(
            account::session_for_game(&game_home(), crate::backend::OFFLINE_PLAY)
                .map_err(no_owner)?,
        )
    } else if crate::developer_offline::enabled(tag) && who.is_none() {
        crate::developer_offline::require_capability(tag)?;
        None
    } else {
        Some(match who {
            None => account::session_for_game(&game_home(), false).map_err(no_owner),
            Some(who) => account::session_for_game_as(&game_home(), who).map_err(hint),
        }?)
    };
    if let Err(e) = need_prepared(tag) {
        account::discard_unread_sessions();
        return Err(e);
    }
    play_step("checking the game's assets");
    complete_assets(tag);
    let mut cmd = client_command(tag);
    if let Some(session) = session {
        if let Some(e) = session.apply(&mut cmd) {
            eprintln!(
                "{}",
                crate::i18n::trf("Playing offline: {reason}", &[("reason", &e)])
            );
        }
        if let Some(e) = account::take_unsaved() {
            eprintln!("{e}");
        }
    } else {
        crate::developer_offline::mark(&mut cmd);
        eprintln!("{}", crate::i18n::tr(crate::developer_offline::NOTICE));
    }
    append_game_args(&mut cmd);
    Ok(cmd)
}
