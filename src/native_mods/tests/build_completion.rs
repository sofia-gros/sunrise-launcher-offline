use super::*;
use crate::storage::{entries_calls, walks};

#[test]
fn build_completion_walks_and_stamps_nothing_on_every_outcome() {
    const CHILD: &str = "SUNRISE_BUILD_COMPLETION_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_mods::tests::build_completion::build_completion_walks_and_stamps_nothing_on_every_outcome",
            ])
            .env(CHILD, "1")
            .env("SUNRISE_LAUNCHER_HOME", home.path().join("launcher"))
            .env("SUNRISE_HOME", home.path().join("game"))
            .env("XDG_DATA_HOME", home.path().join("xdg-data"))
            .env("XDG_CONFIG_HOME", home.path().join("xdg-config"))
            .env("SUNRISE_LAUNCHER_LOCATION_FILE", home.path().join("location"))
            .env("SUNRISE_LAUNCHER_TRASH", home.path().join("trash"))
            .env("SUNRISE_LAUNCHER_CLIPBOARD", home.path().join("clipboard"))
            .env("SUNRISE_LAUNCHER_DESKTOP", home.path().join("desktop"))
            .env("SUNRISE_LAUNCHER_NO_SELF_UPDATE", "1")
            .env("SUNRISE_NO_AUDIO", "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    let home = backend::build_dir();
    let toolchain = format!("rust/{}-fixture", crate::rust_toolchain::VERSION);
    let profile = instances::create("Stamp fixture", None).unwrap();
    let profile_cache = profile.home.join(".sunrise-cache");
    let caches = [
        home.join("cargo"),
        home.join("tmp"),
        home.join(toolchain),
        home.join("rust/old"),
        home.join("native/source"),
        profile_cache,
    ];
    for path in &caches {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("payload"), b"keep").unwrap();
    }
    let cancellation = Cancellation::default();
    let version = "native-stamp-fixture";
    for outcome in [Ok(Plan::Prebuilt), Ok(Plan::UpToDate), Err("failed")] {
        let profile = instances::find(&profile.id).unwrap();
        let before = (walks(), entries_calls());
        let actual = logged(&cancellation, &|| (), |cache| {
            cache.map_err(|error| Failed::Other(error.into()))?;
            let built = outcome
                .as_ref()
                .map(|plan| Built {
                    version: version.into(),
                    plan: plan.clone(),
                    profile: Profile::Optimized,
                    secs: 0,
                    left_out: Vec::new(),
                })
                .map_err(|error| Failed::Other((*error).into()));
            finish_profile_build(
                "fixture-base",
                &profile,
                &cancellation,
                built,
                &|_| {},
                || None,
            )
        });
        assert_eq!((walks(), entries_calls()), before);
        match outcome {
            Ok(_) => {
                assert!(matches!(actual, Ok(ClientBuild::Native(selected)) if selected == version))
            }
            Err(message) => {
                assert!(matches!(actual, Err(Failed::Other(error)) if error == message))
            }
        }
        assert_eq!(
            instances::find(&profile.id).unwrap().version.as_deref(),
            Some(version)
        );
        for path in &caches {
            assert_eq!(fs::read(path.join("payload")).unwrap(), b"keep");
            assert!(!path.join(".last-used").exists());
        }
    }
}
