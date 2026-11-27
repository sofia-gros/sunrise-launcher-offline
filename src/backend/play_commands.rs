//! The commands that start the game and the server, and the steps the window, CLI and MCP share
//! before playing: the version to run, the EULA, what must be prepared, and joining a friend.
use super::{
    check_build_preparation, client_as, complete_reports, data_dir, dedicated,
    default_minecraft_dir, game_architecture_launch_error, game_home, game_version, installed,
    prepare_command, prepare_lock, resources, server_root, version_dir, versions_dir, RuntimeGuard,
    Settings, CLIENT, COMPLETE,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Splits the player's extra game arguments (Settings' one line) into words, the way a shell
/// would: spaces separate them, single or double quotes group a word with spaces in it, and a
/// backslash escapes the next character (outside quotes, or inside double quotes).
pub fn split_args(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    // Inside double quotes only `"` and `\` are escaped, as a shell does.
                    match chars.next() {
                        Some(n @ ('"' | '\\')) => word.push(n),
                        Some(n) => {
                            word.push('\\');
                            word.push(n);
                        }
                        None => word.push('\\'),
                    }
                } else {
                    word.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                '\\' => {
                    if let Some(n) = chars.next() {
                        word.push(n);
                        started = true;
                    }
                }
                c if c.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                _ => {
                    word.push(c);
                    started = true;
                }
            },
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// The extra game arguments the player set in Settings, split into words.
pub fn game_args() -> Vec<String> {
    split_args(&Settings::load().game_args)
}

/// Appends the player's extra game arguments, so they add to the launcher's own or override them.
pub fn append_game_args(cmd: &mut Command) {
    cmd.args(game_args());
}

/// Renderer workarounds a player may have exported to start the launcher itself (a transparent
/// window on old Mesa drivers): they belong to the launcher's own webview and must not reach the
/// game, which picks its own backend. Inherited, they left the client on CPU rendering at 2-3 FPS
/// (#7832).
pub const LAUNCHER_RENDER_ENV: [&str; 4] = [
    "LIBGL_ALWAYS_SOFTWARE",
    "GALLIUM_DRIVER",
    "WEBKIT_DISABLE_DMABUF_RENDERER",
    "WEBKIT_DISABLE_COMPOSITING_MODE",
];

pub(super) fn strip_launcher_render_env(cmd: &mut Command) {
    for key in LAUNCHER_RENDER_ENV {
        cmd.env_remove(key);
    }
}

pub fn client_command(tag: &str) -> Command {
    client_with_resources(tag, &resources(tag))
}

pub(crate) fn client_with_resources(tag: &str, resources: &Path) -> Command {
    let home = game_home();
    let _ = fs::create_dir_all(&home);
    let mut cmd = Command::new(client_program(tag));
    if crate::developer_install::uses_developer(tag) {
        cmd.env("SUNRISE_TELEMETRY_URL", "off");
    }
    crate::run_leases::mark(&mut cmd, &version_dir(tag));
    strip_launcher_render_env(&mut cmd);
    crate::telemetry::ping_only_if_allowed(&mut cmd, !Settings::load().ping_off);
    cmd.current_dir(&home)
        .env("SUNRISE_HOME", &home)
        .arg("--resources")
        .arg(resources);
    // How the game asks for a renewed session (`account session`) while it runs.
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("SUNRISE_LAUNCHER_EXE", exe);
    }
    // PipeWire remembers volume and mute per application name. The game's own name is
    // "PipeWire ALSA [sunrise-client]", shared with development and test runs, where
    // muting it once left every later game silent: played from here it is "Sunrise".
    if std::env::var_os("PIPEWIRE_PROPS").is_none() {
        cmd.env(
            "PIPEWIRE_PROPS",
            "{ application.name = \"Sunrise\" media.role = \"Game\" }",
        );
    }
    cmd
}

/// The game's program: on macOS the one inside the version's Sunrise.app, so the Dock and Cmd+Tab
/// show Sunrise's name and icon (game_app.rs); the bare binary elsewhere, or if that fails.
pub(crate) fn client_program(tag: &str) -> PathBuf {
    let dir = version_dir(tag);
    #[cfg(target_os = "macos")]
    match crate::game_app::ensure(&dir) {
        Ok(program) => return program,
        Err(e) => eprintln!("Starting the game outside {}: {e}", crate::game_app::APP),
    }
    dir.join(CLIENT)
}

pub fn server_command(tag: &str, folder: &Path) -> Result<Command, String> {
    dedicated::command(tag, folder)
}

/// Check the command returned by `prepare_server_command`, while its runtime guard is held.
pub fn verify_server_command(tag: &str, folder: &Path, command: &Command) -> Result<(), String> {
    dedicated::verify(tag, folder, command, true)
}

/// Keep the isolated directory and the installed source fenced before Windows stages its slot,
/// then check the exact staged command before either dedicated spawn route can use it.
pub fn prepare_server_command(
    tag: &str,
    folder: &Path,
    cmd: Command,
) -> Result<(Command, RuntimeGuard), String> {
    dedicated::verify(tag, folder, &cmd, false)?;
    let (cmd, runtime) = prepare_command(cmd)?;
    verify_server_command(tag, folder, &cmd)?;
    Ok((cmd, runtime))
}

/// Whether `eula.txt` says `eula` is true, read as a Java properties file as the server reads it:
/// `#` and `!` lines are comments, the key ends at the first `=`, `:` or blank, spaces around the
/// separator are free, the value runs to the end of the line (a trailing blank is part of it), the
/// last `eula` line wins, and the value counts as true in any case. A line ending in a backslash
/// continues on the next line (its leading blanks dropped), and `\n`, `\r\n` and a lone `\r` all end
/// a line. Left out: backslash escapes inside the text (`\u0074rue`), which no real file uses.
pub fn eula_agreed(folder: &Path) -> bool {
    let Ok(text) = fs::read_to_string(folder.join("eula.txt")) else {
        return false;
    };
    let mut agreed = false;
    // The logical lines: a physical line ending in an odd number of backslashes joins the next.
    let mut logical: Vec<String> = Vec::new();
    let mut joining = false;
    for physical in text.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        let physical = physical.trim_start_matches([' ', '\t', '\x0c']);
        let comment = !joining && physical.starts_with(['#', '!']);
        if joining {
            logical.last_mut().unwrap().push_str(physical);
        } else {
            logical.push(physical.to_string());
        }
        let slashes = physical.len() - physical.trim_end_matches('\\').len();
        joining = !comment && slashes % 2 == 1;
        if joining {
            let last = logical.last_mut().unwrap();
            last.pop();
        }
    }
    for line in &logical {
        if line.starts_with(['#', '!']) {
            continue;
        }
        let Some(rest) = line.strip_prefix("eula") else {
            continue;
        };
        // `eulas=...` is another key.
        if !rest.is_empty() && !rest.starts_with([' ', '\t', '\x0c', '=', ':']) {
            continue;
        }
        let rest = rest.trim_start_matches([' ', '\t', '\x0c']);
        let value = rest
            .strip_prefix(['=', ':'])
            .unwrap_or(rest)
            .trim_start_matches([' ', '\t', '\x0c']);
        agreed = value.eq_ignore_ascii_case("true");
    }
    agreed
}

/// Writes eula.txt as vanilla's server expects it, once the player agreed in the launcher.
pub fn agree_eula(folder: &Path) -> Result<(), String> {
    let folder = server_root(folder)?;
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let text = "#By changing the setting below to TRUE you are indicating your agreement to our EULA (https://aka.ms/MinecraftEULA).\neula=true\n";
    // Replace a directory entry rather than writing through an existing link to player data.
    let mut eula = tempfile::NamedTempFile::new_in(&folder).map_err(|e| e.to_string())?;
    eula.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    eula.persist(folder.join("eula.txt"))
        .map(|_| ())
        .map_err(|e| e.error.to_string())
}

// ----- Shared steps for the front ends -----------------------------------------------------------

/// The installed version to use: the one asked for, else the selected one, else the newest.
pub fn resolve_version(requested: Option<&str>) -> Result<String, String> {
    let installed = installed();
    if let Some(tag) = requested {
        return installed
            .iter()
            .find(|t| *t == tag)
            .cloned()
            .ok_or_else(|| format!("{tag} is not installed."));
    }
    let selected = Settings::load().selected.filter(|s| installed.contains(s));
    selected
        .or_else(|| installed.first().cloned())
        .ok_or_else(no_version_installed)
}

/// What a command that needs an installed version says when there is none: the way to get one.
pub fn no_version_installed() -> String {
    use crate::i18n::tr;
    format!(
        "{} {}",
        tr("No version is installed yet."),
        tr("Run `sunrise-launcher install latest` to get one.")
    )
}

/// Whether a running program was started from the version's folder: the game and a server take
/// their program and `--resources` from it. Windows refuses to delete a running program's files,
/// so there the delete itself fails. With no answer from `ps` the version counts as running.
#[cfg(unix)]
pub(crate) fn in_use(tag: &str) -> bool {
    let dir = format!("{}/", version_dir(tag).display());
    Command::new("ps")
        .args(["-A", "-ww", "-o", "args="])
        .output()
        .ok()
        .is_none_or(|out| String::from_utf8_lossy(&out.stdout).contains(&dir))
}

#[cfg(windows)]
pub(crate) fn in_use(tag: &str) -> bool {
    crate::windows_runtime::in_use(&version_dir(tag))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn in_use(_tag: &str) -> bool {
    false
}

/// The Minecraft folder to prepare from: the one given, the one chosen in the launcher, or the
/// official launcher's default. It may be absent when imported files can be reused.
pub fn minecraft_dir(requested: Option<PathBuf>) -> Result<PathBuf, String> {
    Ok(requested
        .or(Settings::load().minecraft_dir)
        .or_else(default_minecraft_dir)
        .unwrap_or_else(|| crate::preparer::minecraft_dir(None)))
}

/// The server folder: the one given, the one chosen in the launcher, or `server/` in the data folder.
pub fn server_dir(requested: Option<PathBuf>) -> PathBuf {
    requested
        .or(Settings::load().server_dir)
        .unwrap_or_else(|| data_dir().join("server"))
}

/// The build holding the prepared resources, with a reinstall error if a native base is gone.
pub(crate) fn installed_resources_owner(tag: &str) -> Result<String, String> {
    installed_resources_root(tag).map(|(owner, _)| owner)
}

/// The verified base folder accompanies the tag so hosting can keep using the same path.
pub(crate) fn installed_resources_root(tag: &str) -> Result<(String, PathBuf), String> {
    let Some(about) = crate::native_mods::checked_about(tag)? else {
        return Ok((tag.to_owned(), version_dir(tag)));
    };
    let base = about.base;
    let folder = versions_dir().canonicalize().ok().and_then(|versions| {
        versions.join(&base).canonicalize().ok().filter(|folder| {
            folder.parent() == Some(versions.as_path()) && folder.join(COMPLETE).is_file()
        })
    });
    let folder = folder.ok_or_else(|| {
        crate::i18n::trf(
            "This native build needs its base version {base}; reinstall it.",
            &[("base", &base)],
        )
    })?;
    Ok((base, folder))
}

/// Whether `tag` is prepared, with the run-time reports it needs completed when another copy has them.
pub fn need_prepared(tag: &str) -> Result<(), String> {
    need_base_prepared(&installed_resources_owner(tag)?, None)
}

pub(crate) fn need_base_prepared(
    tag: &str,
    preparation: Option<&crate::preparer::Preparation>,
) -> Result<(), String> {
    check_build_preparation(tag).map_err(|issue| issue.message(|| prepare_first(tag)))?;
    let _owned = preparation
        .is_none()
        .then(|| prepare_lock(tag))
        .transpose()?;
    complete_reports(&versions_dir(), tag);
    Ok(())
}

pub(super) fn prepare_first(tag: &str) -> String {
    format!(
        "{tag} is not prepared yet. Prepare it first: `sunrise-launcher prepare --version {tag}` \
         uses your own Minecraft {v}, or downloads it from Mojang for the signed-in account \
         that owns it.",
        v = game_version(tag)
    )
}

/// A step Play waits on before the game starts that the window says by name (`Msg::PlayStep`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayStep {
    Assets,
    Mods,
    Download,
    Prepare,
    Native,
    SignIn,
}

/// Says on stderr which step Play is at, one line each: a terminal run shows where it stops when
/// it never gets to the game (#12179).
pub fn play_step(step: &str) {
    eprintln!("Sunrise: Play: {step}…");
}

/// The client's command, once the owner gate lets it start and its resources are prepared. Past
/// the gate, a sign-in that can't be renewed (no network, expired) still lets the game start,
/// offline.
pub fn client(tag: &str) -> Result<Command, String> {
    client_as(tag, None)
}

/// What the command line adds to a refusal to play for want of an account.
pub const SIGN_IN_HINT: &str =
    crate::i18n::tr_key("Sign in with `sunrise-launcher account sign-in`.");

/// Start the game straight into a friend's world: `--server friend:CODE`.
pub fn join_friend(cmd: &mut Command, code: &str) -> Result<(), String> {
    sunrise_launcher_friends::invite::parse_detailed(code).map_err(|e| e.to_string())?;
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    cmd.arg("--server").arg(format!(
        "{}{code}",
        sunrise_launcher_friends::ADDRESS_PREFIX
    ));
    Ok(())
}

/// The dedicated server's command in `folder`, once prepared and the EULA is agreed to.
pub fn server(tag: &str, folder: &Path, accept_eula: bool) -> Result<Command, String> {
    let command = server_command(tag, folder)?;
    let folder = command
        .get_current_dir()
        .expect("dedicated command owns its directory");
    if let Some(error) = game_architecture_launch_error(tag) {
        return Err(error);
    }
    dedicated::need_prepared(tag, &command)?;
    if accept_eula {
        agree_eula(folder)?;
    }
    if !eula_agreed(folder) {
        return Err(crate::i18n::tr("Running a server needs your agreement to the Minecraft EULA (https://aka.ms/MinecraftEULA). Accept it to continue.").into());
    }
    Ok(command)
}
