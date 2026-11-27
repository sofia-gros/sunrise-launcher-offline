//! Names of the launcher's own files, so the same name is spelled once whether it is read, moved or
//! exported. File names from the game or from another launcher are not here: those are that
//! program's format, not ours.

/// The launcher's settings, beside the launcher's data.
pub const SETTINGS: &str = "settings.json";

/// The pointer that says which launcher and game folders are in use.
pub const LOCATION: &str = "location.json";

/// The game's video and key settings inside an instance.
pub const OPTIONS: &str = "options.txt";
