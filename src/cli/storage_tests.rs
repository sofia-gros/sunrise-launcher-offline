//! The `storage` command and settings compatibility.
use super::*;

#[test]
fn storage_command_and_legacy_cache_limit_setting() {
    assert!(matches!(
        Cli::try_parse_from(["sunrise-launcher", "storage"])
            .unwrap()
            .command,
        Some(Cmd::Storage { clear: None })
    ));
    assert!(matches!(
        Cli::try_parse_from(["sunrise-launcher", "storage", "--clear", "all"])
            .unwrap()
            .command,
        Some(Cmd::Storage {
            clear: Some(ClearStorage::All)
        })
    ));
    assert!(Cli::try_parse_from(["sunrise-launcher", "storage", "--clear", "worlds"]).is_err());
    assert!(Cli::try_parse_from(["sunrise-launcher", "native-cache"]).is_err());
    assert!(
        Cli::try_parse_from(["sunrise-launcher", "storage", "--clear", "other-caches"]).is_ok()
    );
    assert!(
        Cli::try_parse_from(["sunrise-launcher", "settings", "cache-limit-gib", "25"]).is_err()
    );
    // Older settings.json files that still carry cache_limit_gib load cleanly and drop it on save.
    let old: backend::Settings =
        serde_json::from_str(r#"{"cache_limit_gib":25,"native_cache_gib":100,"cat":false}"#)
            .unwrap();
    assert!(!old.show_cat());
    let saved = serde_json::to_string(&old).unwrap();
    assert!(!saved.contains("cache_limit_gib"), "{saved}");
    assert!(!saved.contains("native_cache_gib"), "{saved}");
    let parsed = backend::Settings::parse(r#"{"cache_limit_gib":50,"cat":false}"#);
    assert!(!parsed.show_cat());
    assert!(!serde_json::to_string(&parsed)
        .unwrap()
        .contains("cache_limit_gib"));
}

#[test]
fn the_cat_is_on_until_the_player_switches_it_off() {
    let mut settings = backend::Settings::default();
    assert!(settings.show_cat());
    assert!(serde_json::from_str::<backend::Settings>("{}")
        .unwrap()
        .show_cat());
    let old: backend::Settings = serde_json::from_str(r#"{"native_cache_gib":100}"#).unwrap();
    assert!(old.show_cat());
    assert_eq!(settings_json(&settings)["cat"], true);
    set_setting(&mut settings, "cat", "off").unwrap();
    assert!(!settings.show_cat());
    assert_eq!(settings_json(&settings)["cat"], false);
    let loaded: backend::Settings =
        serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
    assert!(!loaded.show_cat());
    set_setting(&mut settings, "cat", "on").unwrap();
    assert!(settings.show_cat());
    assert!(set_setting(&mut settings, "cat", "maybe").is_err());
    assert!(Cli::try_parse_from(["sunrise-launcher", "settings", "cat", "off"]).is_ok());
}

#[test]
fn the_worlds_setting_picks_rows_or_tiles() {
    use backend::WorldLayout;
    let mut settings = backend::Settings::default();
    assert_eq!(settings_json(&settings)["world_layout"], "rows");
    set_setting(&mut settings, "worlds", "tiles").unwrap();
    assert_eq!(settings.world_layout, WorldLayout::Tiles);
    assert_eq!(settings_json(&settings)["world_layout"], "tiles");
    let loaded = backend::Settings::parse(&serde_json::to_string(&settings).unwrap());
    assert_eq!(loaded.world_layout, WorldLayout::Tiles);
    assert!(set_setting(&mut settings, "worlds", "grid").is_err());
    assert_eq!(settings.world_layout, WorldLayout::Tiles);
    set_setting(&mut settings, "worlds", "rows").unwrap();
    assert_eq!(settings.world_layout, WorldLayout::Rows);
    assert!(Cli::try_parse_from(["sunrise-launcher", "settings", "worlds", "tiles"]).is_ok());
}
