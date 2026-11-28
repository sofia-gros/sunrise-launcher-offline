//! A portable Rust toolchain of the launcher's own, for native mods (docs/modding/src/how-to/native-mods.md), which
//! compile the game. levzzz5154 (2026-10-27): players combine many native mods per build, so they
//! build on their own computer, and the launcher brings the compiler.
//!
//! It downloads only the official Rust components a build needs (`rustc`, `cargo`, `rust-std`, and
//! on Windows `rust-mingw`) from static.rust-lang.org, each pinned by its SHA-256 below, and
//! unpacks them into the launcher's data folder. No rustup or player-installed compiler is used.
//! Windows also gets a pinned C/link toolchain: WinLibs for x64 and LLVM-MinGW for ARM64.
mod download;
mod linking;

use crate::backend;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use download::install;
#[cfg(test)]
use download::{
    component_path, download_archive, llvm_mingw_path, mingw_path, save_archive, unpack,
    unpack_llvm_mingw, unpack_mingw,
};
pub(crate) use download::{install_cancellable, unsupported_host};
#[cfg(test)]
use linking::linux_needs;
pub use linking::{linker, linker_server};

/// Native mods use the same Rust release as the game CI (`tools/ci/rust-version`).
/// Bumping it also updates [`DIST`] and each platform's component checksums below.
pub const VERSION: &str = include_str!("../rust-version").trim_ascii();
const DIST: &str = sunrise_launcher_addresses::RUST_DIST;

/// One platform's components: (name, SHA-256, compressed bytes) of `<name>-<VERSION>-<target>.tar.gz`.
#[derive(Debug, PartialEq)]
pub struct Host {
    pub target: &'static str,
    components: &'static [(&'static str, &'static str, u64)],
    /// The C and link toolchain it brings besides Rust's components.
    c_toolchain: Option<&'static CToolchain>,
}

/// A Windows C/link toolchain archive, pinned by its SHA-256 and byte length.
#[derive(Debug, PartialEq)]
pub struct CToolchain {
    pub version: &'static str,
    url: &'static str,
    sha256: &'static str,
    bytes: u64,
    directory: &'static str,
    kind: CToolchainKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CToolchainKind {
    WinLibs,
    LlvmMingw,
}

/// WinLibs' build of GCC and binutils on MinGW-w64 (MSVCRT, the C runtime `rust-mingw` links
/// against; POSIX threads; SEH), the release's `mingw64/` folder without its documentation.
pub const MINGW: CToolchain = CToolchain {
    version: "GCC 16.2.0, MinGW-w64 14.0.0 (WinLibs r3)",
    url: sunrise_launcher_addresses::WINLIBS_MINGW,
    sha256: "62af32ea58acdea1318ea5dd8eb7017eaee694dd40b269d52a59c769dcfefb82",
    bytes: 277203392,
    directory: MINGW_DIR,
    kind: CToolchainKind::WinLibs,
};

/// LLVM-MinGW 20261104, with UCRT and native ARM64 host tools. It targets ARM64 Windows using
/// LLVM's GNU-compatible ABI; it is not an MSVC object/library toolchain.
pub const LLVM_MINGW: CToolchain = CToolchain {
    version: "LLVM-MinGW 23.1.5 (20261104, UCRT)",
    url: sunrise_launcher_addresses::LLVM_MINGW,
    sha256: "7d7f8387a8706639b0b47a629d6e23f8b3ef9acce67e9832b8cd4c5bb2816eab",
    bytes: 186164073,
    directory: LLVM_MINGW_DIR,
    kind: CToolchainKind::LlvmMingw,
};
/// Where MinGW-w64 goes in the toolchain, as the zip names it.
const MINGW_DIR: &str = "mingw64";
const LLVM_MINGW_DIR: &str = "llvm-mingw";

/// The Windows x64 host, whose GNU runtime libraries ship beside the game.
pub const WINDOWS_X64_TARGET: &str = "x86_64-pc-windows-gnu";
pub const WINDOWS_ARM64_TARGET: &str = "aarch64-pc-windows-gnullvm";
pub const WINDOWS_X64_MACHINE: u16 = 0x8664;
pub const WINDOWS_ARM64_MACHINE: u16 = 0xaa64;

const HOSTS: &[Host] = &[
    Host {
        target: "aarch64-apple-darwin",
        components: &[
            (
                "rustc",
                "cc998c1dc8df006106472357aa87838b2da3f7b25446b98a94cfd58796cb0610",
                85305190,
            ),
            (
                "cargo",
                "b5cc7634f919949c3e5e131123f813416dc47f6b809a5371d12f1f593595ae13",
                13082476,
            ),
            (
                "rust-std",
                "c124006d63885ba1029aca49efd59e93be6b2b171c379318ff94a00ef5eba664",
                48506029,
            ),
        ],
        c_toolchain: None,
    },
    // Intel Macs (#1161): the same system linker (Command Line Tools) as Apple Silicon.
    Host {
        target: "x86_64-apple-darwin",
        components: &[
            (
                "rustc",
                "dad7c1300549bc846e1f02652fa11d131b86cdbb8288d64da5f62b4504d62a8f",
                104855037,
            ),
            (
                "cargo",
                "ed05dab7a703d1b4c6fa5cec8e99272cb1c3902a323986c80e0bc57915c85ddd",
                13439668,
            ),
            (
                "rust-std",
                "5cc4606fdd6c9226be51f59a333942cc8a203e74f7da38d47aaaf44b986bfe3c",
                48891033,
            ),
        ],
        c_toolchain: None,
    },
    Host {
        target: "x86_64-unknown-linux-gnu",
        components: &[
            (
                "rustc",
                "85e7c0e6f21636f714e2a42966011364a6cc56b6548126d32144d6ba90904966",
                141445153,
            ),
            (
                "cargo",
                "47babc028247e26577f8a1bf390b11f447ad9b7ab93f5c9a0dda078935fc149b",
                14520879,
            ),
            (
                "rust-std",
                "d1290c05994a59c029e562cb75be1bddc6fb11a4c501e1b7b0f61d119c6cbb12",
                50361623,
            ),
        ],
        c_toolchain: None,
    },
    Host {
        target: WINDOWS_X64_TARGET,
        components: &[
            (
                "rustc",
                "43b17e9a9134ea72562bfbb23728a830f671ea9be1db6a72597a8d240e986b35",
                161274138,
            ),
            (
                "cargo",
                "57e54617be4420dbb967ca8293f5ace7168104ac12efa9dc7c5c3bda112f8b36",
                18659998,
            ),
            (
                "rust-std",
                "6546651238a3ac092b763511be524ca6eda6103e5d47342416923be1b3ce837c",
                44957351,
            ),
            (
                "rust-mingw",
                "30263d7bb4fd0bf63a153afe36d7fc102f929a7d42781651c6b3a03a414ba428",
                9594887,
            ),
        ],
        c_toolchain: Some(&MINGW),
    },
    Host {
        target: WINDOWS_ARM64_TARGET,
        components: &[
            (
                "rustc",
                "261a6bff00129905f97e07db33aaecb994d22ab40efef2b6b1e27b8cc37231a8",
                140086017,
            ),
            (
                "cargo",
                "a678122654b6ca4240997cec6ab6e3c91fa0e79466939c06b079c8a717efc6aa",
                14008619,
            ),
            (
                "rust-std",
                "a492480db6ba615c70a10c8c146012d04276122cd57d00aef225764e0791968f",
                45343615,
            ),
            (
                "rust-mingw",
                "69fe700a14751b7b5a191bab92a9620e4deb4095f232873476033d1a6a5d8cd7",
                870178,
            ),
        ],
        c_toolchain: Some(&LLVM_MINGW),
    },
];

impl Host {
    /// Exact compressed bytes downloaded for this pinned compiler and its linker.
    pub fn download_bytes(&self) -> u64 {
        self.components
            .iter()
            .map(|(_, _, bytes)| bytes)
            .sum::<u64>()
            + self.c_toolchain.map_or(0, |compiler| compiler.bytes)
    }
}

/// This computer's host, if the launcher ships a toolchain for it.
fn host_for(os: &str, arch: &str) -> Option<&'static Host> {
    let target = match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "x86_64") => WINDOWS_X64_TARGET,
        ("windows", "aarch64") => WINDOWS_ARM64_TARGET,
        _ => return None,
    };
    HOSTS.iter().find(|h| h.target == target)
}

/// Every supported host, for cross-platform command tests.
#[cfg(test)]
pub(crate) fn hosts() -> &'static [Host] {
    HOSTS
}

pub fn host() -> Option<&'static Host> {
    host_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// The Rust target for an installed Windows game's PE machine type.
pub fn windows_target_for_machine(machine: u16) -> Option<&'static str> {
    match machine {
        WINDOWS_X64_MACHINE => Some(WINDOWS_X64_TARGET),
        WINDOWS_ARM64_MACHINE => Some(WINDOWS_ARM64_TARGET),
        _ => None,
    }
}

/// Native-mod builds are supported when the selected game's architecture matches the launcher.
pub fn windows_target_for_host(host_target: &str, machine: u16) -> Option<&'static str> {
    let target = windows_target_for_machine(machine)?;
    (target == host_target).then_some(target)
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// An unpacked toolchain.
#[derive(Clone, Debug, PartialEq)]
pub struct Toolchain {
    pub dir: PathBuf,
    pub(crate) host: &'static Host,
}

impl Toolchain {
    pub fn cargo(&self) -> PathBuf {
        self.dir.join("bin").join(exe("cargo"))
    }

    pub fn rustc(&self) -> PathBuf {
        self.dir.join("bin").join(exe("rustc"))
    }

    /// Where Rust's own libraries for this computer are: the standard library a dynamically
    /// linked program loads (the prebuilt game's).
    pub fn target_libdir(&self) -> Option<PathBuf> {
        Some(
            self.dir
                .join("lib")
                .join("rustlib")
                .join(self.host.target)
                .join("lib"),
        )
    }

    /// MinGW-w64's programs (gcc, as, dlltool), when the toolchain brings it (Windows).
    pub fn mingw_bin(&self) -> PathBuf {
        self.host
            .c_toolchain
            .map_or_else(|| self.dir.join(MINGW_DIR), |c| self.dir.join(c.directory))
            .join("bin")
    }

    fn compiler_available(&self) -> bool {
        let Some(compiler) = self.host.c_toolchain else {
            return true;
        };
        match compiler.kind {
            CToolchainKind::WinLibs => self.mingw_bin().join(exe("gcc")).is_file(),
            CToolchainKind::LlvmMingw => [
                exe("aarch64-w64-mingw32-clang"),
                exe("aarch64-w64-mingw32-clang++"),
                exe("llvm-ar"),
                exe("llvm-ranlib"),
                exe("llvm-dlltool"),
                // winresource defaults to an unprefixed x64 `windres` on a native ARM64 host.
                // Give it the target-specific resource compiler and archive tool too.
                exe("aarch64-w64-mingw32-ar"),
                exe("aarch64-w64-mingw32-windres"),
            ]
            .iter()
            .all(|program| self.mingw_bin().join(program).is_file()),
        }
    }

    /// `program` (this toolchain's cargo, or a tool built with it) set up to use only this
    /// toolchain: its rustc, its C/link tools where it has them, the launcher's Cargo home, and none of
    /// the player's rustup, compiler-wrapper or compiler-flag settings ([`scrubbed`]). On Windows it
    /// opens no console window: cargo is a console program, and the launcher has none to share, so
    /// each build showed an empty one for as long as it ran (#4770).
    pub fn command(&self, program: &Path) -> Command {
        self.command_with(
            program,
            cfg!(windows),
            std::env::var_os("ComSpec").as_deref(),
            std::env::var_os("SystemRoot").as_deref(),
        )
    }

    /// [`Toolchain::command`] with the platform and the player's `ComSpec` and `SystemRoot`
    /// passed in, so the `ComSpec` repair can be tested on any platform.
    fn command_with(
        &self,
        program: &Path,
        windows: bool,
        comspec: Option<&std::ffi::OsStr>,
        system_root: Option<&std::ffi::OsStr>,
    ) -> Command {
        let mut cmd = crate::process::command(program);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mingw = self.host.c_toolchain.map(|_| self.mingw_bin());
        let paths = std::iter::once(self.dir.join("bin"))
            .chain(mingw)
            .chain(std::env::split_paths(&path));
        if let Some(root) = outside_configs() {
            cmd.current_dir(root);
        }
        cmd.env("PATH", std::env::join_paths(paths).unwrap_or(path))
            .env("RUSTC", self.rustc())
            .env("CARGO_HOME", cargo_home());
        // gcc, as, ld — and `cargo install`'s build — put scratch files in the temp dir,
        // which has to be a path they can open too: the build's own, never the player's
        // (#5189: a Cyrillic user name makes the player's unreadable by MinGW-w64).
        let tmp = backend::build_dir().join("tmp");
        if fs::create_dir_all(&tmp).is_ok() {
            cmd.env("TMPDIR", &tmp).env("TMP", &tmp).env("TEMP", &tmp);
        }
        if windows {
            set_comspec(&mut cmd, comspec, system_root);
        }
        for name in SCRUBBED {
            cmd.env_remove(name);
        }
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(scrubbed) {
                cmd.env_remove(name);
            }
        }
        // These are the launcher's own settings, so add them after removing the player's Cargo
        // target overrides. The ARM64 target's linker is LLVM-MinGW's Clang driver; Rust and cc
        // must both see tools from this managed toolchain.
        if self.host.target == WINDOWS_ARM64_TARGET {
            let bin = self.mingw_bin();
            let cc = bin.join(exe("aarch64-w64-mingw32-clang"));
            let cxx = bin.join(exe("aarch64-w64-mingw32-clang++"));
            let ar = bin.join(exe("llvm-ar"));
            let ranlib = bin.join(exe("llvm-ranlib"));
            let resource_ar = bin.join(exe("aarch64-w64-mingw32-ar"));
            let windres = bin.join(exe("aarch64-w64-mingw32-windres"));
            cmd.env("CC_aarch64_pc_windows_gnullvm", &cc)
                .env("CXX_aarch64_pc_windows_gnullvm", &cxx)
                .env("AR_aarch64_pc_windows_gnullvm", &ar)
                .env("RANLIB_aarch64_pc_windows_gnullvm", &ranlib)
                .env("AR", resource_ar)
                .env("WINDRES", windres)
                .env("CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER", cc);
        }
        cmd
    }

    /// Adds the ARM64-specific Rust flags Cargo needs for Windows imports and a self-contained
    /// unwind runtime. A Cargo config value keeps paths with spaces intact.
    pub fn configure_cargo(&self, cmd: &mut Command) {
        for value in self.cargo_config() {
            cmd.arg("--config").arg(value);
        }
    }

    /// The `--config` values every cargo command of this toolchain gets (Windows ARM64's flags; none elsewhere).
    pub fn cargo_config(&self) -> Vec<String> {
        if self.host.target != WINDOWS_ARM64_TARGET {
            return Vec::new();
        }
        let dlltool = self.mingw_bin().join(exe("llvm-dlltool"));
        let rustflags = toml::Value::Array(vec![
            toml::Value::String("-C".into()),
            // Windows doesn't ship LLVM-MinGW's libunwind.dll. On this target, crt-static also
            // makes Rust use the static unwinder so the built game has no extra runtime DLL.
            toml::Value::String("target-feature=+crt-static".into()),
            toml::Value::String("-C".into()),
            toml::Value::String(format!("dlltool={}", dlltool.display())),
        ]);
        vec![format!(
            "target.{WINDOWS_ARM64_TARGET}.rustflags={rustflags}"
        )]
    }

    /// The folder of the C and link programs this toolchain brings, when it brings them (Windows).
    pub fn c_bin(&self) -> Option<PathBuf> {
        self.host.c_toolchain.map(|_| self.mingw_bin())
    }

    /// `rustc --version`, once the toolchain answers.
    pub fn version(&self) -> Result<String, String> {
        let out = self
            .command(&self.rustc())
            .arg("--version")
            .output()
            .map_err(|e| format!("Rust {VERSION} doesn't start: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "Rust {VERSION} doesn't start: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

/// Whether the player's environment variable `name` stays out of a build: another Rust, a wrapper, or other compiler flags.
pub fn scrubbed(name: &str) -> bool {
    const PREFIXES: [&str; 3] = ["CARGO_PROFILE_", "CARGO_BUILD_", "CARGO_TARGET_"];
    // Windows' variable names ignore case.
    let name = name.to_ascii_uppercase();
    // The number of jobs changes no flag: a player may still limit it.
    if name == "CARGO_BUILD_JOBS" {
        return false;
    }
    SCRUBBED.contains(&name.as_str()) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// `ioprio_set`'s "which" for one process.
#[cfg(target_os = "linux")]
const IOPRIO_WHO_PROCESS: libc::c_long = 1;
/// The best-effort I/O class (2) at its lowest level (7), as `ioprio_set` takes it.
#[cfg(target_os = "linux")]
const LOW_IO: libc::c_long = (2 << 13) | 7;

/// Runs `cmd` (a build) below the player's own programs: nice 19 and the lowest best-effort I/O on Unix, below normal on Windows.
pub fn low_priority(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: two async-signal-safe system calls on the child between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpriority(libc::PRIO_PROCESS, 0, 19);
                #[cfg(target_os = "linux")]
                {
                    libc::syscall(
                        libc::SYS_ioprio_set,
                        IOPRIO_WHO_PROCESS,
                        0 as libc::c_long,
                        LOW_IO,
                    );
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    cmd
}

/// The `ComSpec` a build tool needs when the player's names no file: `windres` runs its
/// preprocessor through the command interpreter `ComSpec` names, so the resource step of a
/// build fails when it is stale or empty. `None` when it is fine, or when `system_root`
/// (`%SystemRoot%`) gives no `cmd.exe` to put there.
fn comspec_fix(
    comspec: Option<&std::ffi::OsStr>,
    system_root: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if comspec.is_some_and(|c| Path::new(c).is_file()) {
        return None;
    }
    let cmd_exe = Path::new(system_root?).join("System32").join("cmd.exe");
    cmd_exe.is_file().then_some(cmd_exe)
}

/// Sets `ComSpec` on `cmd` to the system's `cmd.exe` when `comspec_fix` offers one.
fn set_comspec(
    cmd: &mut Command,
    comspec: Option<&std::ffi::OsStr>,
    system_root: Option<&std::ffi::OsStr>,
) {
    if let Some(cmd_exe) = comspec_fix(comspec, system_root) {
        cmd.env("ComSpec", cmd_exe);
    }
}

pub const SCRUBBED: [&str; 8] = [
    "RUSTUP_TOOLCHAIN",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "CARGO_INCREMENTAL",
];

/// Where commands run so that no player's Cargo configuration reaches them: the filesystem root
/// above the launcher's data (`/` on Unix, whatever mount the data is on; the drive's root on
/// Windows). Cargo reads `.cargo/config.toml` in the folder it starts in and in
/// every folder above it, so from a folder under the player's home it would read
/// `~/.cargo/config.toml` even with another `CARGO_HOME`: its `[profile]`, `[build]` and
/// `[target]` settings (rustflags, linker, incremental) beat both the workspace's profile and
/// the environment, and no command-line switch turns that search off. A build names its workspace
/// with `--manifest-path` instead.
pub fn outside_configs() -> Option<PathBuf> {
    let data = backend::build_dir();
    data.ancestors()
        .last()
        .filter(|root| root.has_root())
        .map(Path::to_path_buf)
}

/// Where toolchains live: `<build dir>/rust/` ([`backend::build_dir`] — a path the tools can
/// read, which the data folder's isn't on Windows with a non-ASCII user name, #5189).
pub fn root() -> PathBuf {
    backend::build_dir().join("rust")
}

/// The Cargo home of native builds (downloaded crates, horizon-mixins), never the player's own.
pub fn cargo_home() -> PathBuf {
    backend::build_dir().join("cargo")
}

fn dir_for(host: &Host) -> PathBuf {
    root().join(format!("{VERSION}-{}", host.target))
}

const COMPLETE: &str = ".installed";

/// The toolchain, when it is installed. An incomplete Windows C/link toolchain is reinstalled.
pub fn installed() -> Option<Toolchain> {
    let host = host()?;
    let toolchain = Toolchain {
        dir: dir_for(host),
        host,
    };
    (toolchain.dir.join(COMPLETE).is_file() && toolchain.compiler_available()).then_some(toolchain)
}

/// Removes every toolchain, the native builds' Cargo home and their scratch folder.
pub fn remove() -> Result<(), String> {
    for dir in [root(), cargo_home(), backend::build_dir().join("tmp")] {
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
