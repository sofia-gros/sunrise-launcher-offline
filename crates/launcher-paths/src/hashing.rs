//! The SHA-1 and SHA-256 the launcher checks downloads with, in one place: the same algorithms
//! are needed by the installer, the mod browser and GitHub imports, and must agree on lowercase
//! hex and on what a missing file means.
use sha1::{Digest, Sha1 as RawSha1};
use std::fs;
use std::io::Read;
use std::path::Path;

/// SHA-1 over data that arrives in pieces (a download).
pub struct Sha1(RawSha1);

impl Sha1 {
    pub fn new() -> Self {
        Self(RawSha1::new())
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn hex(self) -> String {
        self.0
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

/// The SHA-1 of `bytes`, in lowercase hex.
pub fn sha1_hex(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(bytes);
    hash.hex()
}

/// The SHA-1 of the file at `path`, in lowercase hex; `None` when it cannot be read.
pub fn sha1_file(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hash = Sha1::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        match file.read(&mut buffer).ok()? {
            0 => break,
            n => hash.update(&buffer[..n]),
        }
    }
    Some(hash.hex())
}

/// The SHA-256 of `bytes`, in lowercase hex. The implementation is the game's release verifier's,
/// so a launcher download and a signed release agree byte for byte.
pub fn sha256_hex(bytes: &[u8]) -> String {
    sunrise_launcher_release_verify::sha256_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_file_is_hashed_and_a_missing_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("abc");
        fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha1_file(&file),
            Some("a9993e364706816aba3e25717850c26c9cd0d89d".to_owned())
        );
        assert_eq!(sha1_file(&dir.path().join("missing")), None);
    }
}
