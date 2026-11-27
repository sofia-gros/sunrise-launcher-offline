//! The SHA-1 and SHA-256 the launcher checks downloads with live in
//! `sunrise-launcher-paths::hashing`, so every caller agrees on lowercase hex and on what a
//! missing file means.
pub(crate) use sunrise_launcher_paths::hashing::*;
