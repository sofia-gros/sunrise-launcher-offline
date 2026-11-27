//! The launcher's identity: its names, folders, keychain service and Windows registry keys.
pub const VERSION_PREFIX: &str = "sunrise-launcher ";
pub const DESKTOP_ENTRY: &str = "net.playsunrise.launcher.desktop";
pub const WINDOW_TITLE: &str = crate::i18n::tr_key("Sunrise Launcher");
pub const QUIT_LABEL: &str = crate::i18n::tr_key("Quit Sunrise Launcher");
pub const DISPLAY_NAME: &str = "Sunrise Launcher";
// The installed copy's folder, shortcuts and Add/Remove entry carry Sunrise's own name, never a Horizon install's.
pub const INSTALL_NAME: &str = "Sunrise Launcher";
pub const EXECUTABLE: &str = "sunrise-launcher";
pub const WINDOWS_EXECUTABLE: &str = "sunrise-launcher.exe";
pub const WINDOWS_EXECUTABLE_PREFIX: &str = "sunrise-launcher.exe.";
pub const APP_ID: &str = "net.playsunrise.launcher";
// Sunrise's own data, game and location folders, all under `sunrise/`, and saved accounts, copied on first start from the launchers before it (`carry_over`).
pub const KEYCHAIN_SERVICE: &str = "net.playsunrise.launcher";
pub const LOCATION_FOLDER: &str = "sunrise/location";
pub const LAUNCHER_FOLDER: &str = "sunrise/launcher";
pub const GAME_FOLDER: &str = "sunrise/game";
pub const PUBLISHER: &str = "sunrise-org";
pub const UNINSTALL_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\SunriseLauncher";
pub const INSTALL_KEY: &str = "Software\\Sunrise\\Launcher";
pub const REGISTRY_PARENT: &str = "Software\\Sunrise";
pub const UNINSTALL_PREFIX: &str = "sunrise-launcher-uninstall-";
pub const SETUP_PREFIX: &str = "sunrise-launcher-setup";
pub const PROCESS_FILTER: &str = "IMAGENAME eq sunrise-launcher.exe";

/// The launchers before this one, newest first: Horizon's own, then the arismoko fork's. Each
/// is (launcher folder, game folder, location folder) under the same bases as this launcher's.
/// A Sunrise build copies its first data from the first of them that has any (`carry_over`);
/// it never writes to them, and the uninstaller never removes them.
pub const EARLIER: &[(&str, &str, &str)] = &[
    ("horizon-launcher", "horizon", "horizon-launcher-location"),
    (
        "horizon-fork-arismoko/launcher",
        "horizon-fork-arismoko/game",
        "horizon-fork-arismoko-location",
    ),
];

/// The keychain services of the launchers before this one ([`EARLIER`]): a sign-in saved there is
/// copied under [`KEYCHAIN_SERVICE`] the first time it is read.
pub const EARLIER_KEYCHAIN_SERVICES: &[&str] = &["dev.playhorizon.launcher"];

/// The launcher's data folder under `base`.
pub fn launcher_folder_in(base: &std::path::Path) -> std::path::PathBuf {
    base.join(LAUNCHER_FOLDER)
}

/// The game's folder under `base`.
pub fn game_folder_in(base: &std::path::Path) -> std::path::PathBuf {
    base.join(GAME_FOLDER)
}

/// Product home used by the Windows installer's About link.
pub const ABOUT_URL: &str = sunrise_launcher_addresses::LAUNCHER_ABOUT;

/// Whether the uninstaller may remove `path` as this launcher's own data: the complete owned
/// suffix, not just a generic leaf selected by an override. An earlier launcher's folder
/// ([`EARLIER`]) never is. Uninstallation keeps the existing named-folder guard for relocated
/// data as well.
pub fn owns_launcher_data_dir(path: &std::path::Path) -> bool {
    ends_with(path, LAUNCHER_FOLDER)
}

/// Whether `path` ends with every component of `suffix`, case aside.
fn ends_with(path: &std::path::Path, suffix: &str) -> bool {
    let mut actual = path.components().rev();
    std::path::Path::new(suffix)
        .components()
        .rev()
        .all(|expected| {
            actual.next().is_some_and(|component| {
                component
                    .as_os_str()
                    .eq_ignore_ascii_case(expected.as_os_str())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn data_removal_requires_the_complete_owned_namespace() {
        let profile = Path::new("profile");
        let own = profile.join(LAUNCHER_FOLDER);
        assert!(owns_launcher_data_dir(&own));
        assert!(owns_launcher_data_dir(
            &profile.join(LAUNCHER_FOLDER.to_ascii_uppercase())
        ));
        // An earlier launcher's folder is never this one's to remove.
        for (old, _, _) in EARLIER {
            assert!(!owns_launcher_data_dir(&profile.join(old)), "{old}");
        }
        for unowned in [
            "launcher",
            "unrelated/launcher",
            "not-horizon-fork-arismoko/launcher",
            "horizon-launcher-backup",
            "sunrise/launcher-backup",
            "not-sunrise/launcher-backup",
        ] {
            assert!(!owns_launcher_data_dir(&profile.join(unowned)), "{unowned}");
        }
    }
}
