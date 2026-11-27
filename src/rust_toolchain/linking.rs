//! What a build needs from this computer besides the toolchain: the system's linker and libraries.
use super::*;

/// Whether this computer has what a build needs besides the toolchain. Rust brings its compiler,
/// but uses the system's linker on macOS (the Command Line Tools) and Linux (`cc`). On Linux the
/// game also builds against the system's sound and controller libraries ([`linux_needs`]).
/// Windows' `windows-gnu` toolchain brings its own linker, and MinGW-w64 for C and import
/// libraries ([`MINGW`]).
pub fn linker() -> Result<(), String> {
    link(true)
}

/// The headless build's ([`crate::native_mods::build_server`]): the same check, minus what only
/// the game links. A server build compiles without the default features, so it links none of
/// the system's sound or controller libraries and needs no pkg-config to find them: only a C
/// compiler and the C library's headers.
pub fn linker_server() -> Result<(), String> {
    link(false)
}

fn link(full: bool) -> Result<(), String> {
    let ok = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    match std::env::consts::OS {
        // `xcode-select -p` only says where they are; running `cc` without them would open
        // Apple's install dialog by surprise.
        "macos" if !ok("xcode-select", &["-p"]) => Err(
            crate::i18n::tr("Native mods need Apple's Command Line Tools to link the game. Run `xcode-select --install` in Terminal, then build again.")
                .into(),
        ),
        "linux" => linux_needs(
            ok,
            &fs::read_to_string("/etc/os-release").unwrap_or_default(),
            full,
        ),
        _ => Ok(()),
    }
}

/// What a Linux build needs from the system, checked before anything is downloaded: a C compiler
/// (the linker; `ring` and `hidapi` compile C) with the C library's headers and, for the game
/// only (`full`), `pkg-config` and the development files of ALSA (`alsa-sys`, sound) and
/// libudev (`libudev-sys` and `hidapi`, controllers), which those crates look up with it. The
/// headless server build links none of those (`--no-default-features`, docs/server.md).
/// Desktop distributions ship the libraries but not these files, and SteamOS has GCC without
/// the C library's headers (`stdint.h` not found in `ring`, #4770), so a build without them
/// failed after starting. `ok` runs a check; `os` is `/etc/os-release`, which picks the install
/// command.
pub(super) fn linux_needs(
    ok: impl Fn(&str, &[&str]) -> bool,
    os: &str,
    full: bool,
) -> Result<(), String> {
    let cc = ok("cc", &["--version"]);
    // GCC's and Clang's own `stdint.h` include the C library's.
    let headers = [
        "-fsyntax-only",
        "-x",
        "c",
        "-include",
        "stdint.h",
        "/dev/null",
    ];
    let mut missing: Vec<&str> = [
        (cc, "a C compiler (cc)"),
        (!cc || ok("cc", &headers), "the C library's headers"),
    ]
    .into_iter()
    .filter(|(found, _)| !found)
    .map(|(_, what)| what)
    .collect();
    if full {
        let pkg_config = ok("pkg-config", &["--version"]);
        let has = |name: &str| pkg_config && ok("pkg-config", &["--exists", name]);
        missing.extend(
            [
                (pkg_config, "pkg-config"),
                (has("alsa"), crate::i18n::tr("ALSA's development files")),
                (
                    has("libudev"),
                    crate::i18n::tr("libudev's development files"),
                ),
            ]
            .into_iter()
            .filter(|(found, _)| !found)
            .map(|(_, what)| what),
        );
    }
    if missing.is_empty() {
        return Ok(());
    }
    let field = |name: &str| {
        os.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix('='))
            .map(|v| v.trim_matches('"').to_owned())
            .unwrap_or_default()
    };
    let (id, like) = (field("ID"), field("ID_LIKE"));
    let family = |names: &[&str]| {
        std::iter::once(id.as_str())
            .chain(like.split_whitespace())
            .any(|n| names.contains(&n))
    };
    let apt = if full {
        "sudo apt install build-essential pkg-config libasound2-dev libudev-dev"
    } else {
        "sudo apt install build-essential"
    };
    let command = if family(&["debian", "ubuntu"]) {
        Some(apt)
    } else if family(&["fedora", "rhel"]) {
        Some(if full {
            "sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel systemd-devel"
        } else {
            "sudo dnf install gcc glibc-devel"
        })
    } else if family(&["arch"]) && id != "steamos" {
        Some(if full {
            "sudo pacman -S --needed base-devel alsa-lib systemd-libs"
        } else {
            "sudo pacman -S --needed base-devel"
        })
    } else if family(&["suse", "opensuse"]) {
        Some(if full {
            "sudo zypper install gcc pkg-config alsa-devel libudev-devel"
        } else {
            "sudo zypper install gcc glibc-devel"
        })
    } else {
        None
    };
    // NixOS keeps every package in the store, where pkg-config finds them only inside a shell that
    // lists them (`nix-shell`, or `nix develop`); installing them in `environment.systemPackages`
    // changes nothing for a build started from the desktop (#5248). Its shell is the launcher's
    // `shell.nix`, so point at that rather than at a package manager that would not help.
    let how = match command {
        _ if id == "nixos" => {
            crate::i18n::tr("On NixOS they are only visible inside a shell that provides them: run `nix-shell` in the launcher's folder (its shell.nix has these packages), then build again.")
                .to_owned()
        }
        _ if id == "steamos" => crate::i18n::tr("SteamOS ships without them and its system is read-only, so native mods can't be built on it yet.").to_owned(),
        Some(command) => crate::i18n::trf("Install them in a terminal, then build again:\n{command}", &[("command", command)]),
        None => crate::i18n::trf("Install them with your distribution's package manager, then build again (on Debian, Ubuntu or Mint: {apt}).", &[("apt", apt)]),
    };
    Err(crate::i18n::trf(
        "Native mods need these to build the game on Linux, and this computer lacks them: {missing}. {how}",
        &[("missing", &missing.join(", ")), ("how", &how)],
    ))
}
