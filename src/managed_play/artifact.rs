//! Exact identity of an installed executable and the files it reads beside itself.
use super::{digest_file, plain_file, require_dir, unsettled, validate_tag};
use crate::{backend, native_mods::sdk_cache::GameSlices};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub(super) tag: String,
    executable: [u8; 32],
    runtime: [u8; 32],
}

impl Artifact {
    pub(super) fn capture(tag: &str) -> Result<Self, String> {
        validate_tag(tag)?;
        Self::capture_at(tag, &backend::version_dir(tag))
    }

    pub(crate) fn capture_at(tag: &str, dir: &Path) -> Result<Self, String> {
        Self::installed(dir, tag, None)
    }

    pub(crate) const ARCHIVE_RECEIPT: &str = ".sunrise-archive.json";
    pub(crate) const SDK_RECEIPT: &str = ".sunrise-sdk.json";

    /// Record the extracted image before its cache name and completion marker are published.
    pub(crate) fn record_archive(dir: &Path, tag: &str, archive: &str) -> Result<(), String> {
        let image = Self::image(dir, tag, Some(archive), None)?;
        let path = dir.join(Self::ARCHIVE_RECEIPT);
        let file = fs::File::create_new(&path).map_err(|error| at(&path, error))?;
        serde_json::to_writer(
            file,
            &ArchiveReceipt {
                archive: archive.to_owned(),
                image,
            },
        )
        .map_err(|error| at(&path, error.into()))
    }

    pub(crate) fn record_sdk(dir: &Path, tag: &str, sdk: GameSlices) -> Result<(), String> {
        let image = Self::image(dir, tag, None, Some(&sdk))?;
        let path = dir.join(Self::SDK_RECEIPT);
        let file = fs::File::create_new(&path).map_err(|error| at(&path, error))?;
        serde_json::to_writer(file, &SdkReceipt { sdk, image })
            .map_err(|error| at(&path, error.into()))
    }

    /// Accept a deferred archive only when its digest and the current executable/runtime
    /// match the image recorded after verifying and extracting that archive.
    pub(crate) fn matches_archive(dir: &Path, tag: &str, archive: &str) -> bool {
        Self::from_archive(dir, tag, archive).is_ok()
    }

    pub(crate) fn from_archive(dir: &Path, tag: &str, archive: &str) -> Result<Self, String> {
        Self::installed(dir, tag, Some(Source::Archive(archive)))
    }

    pub(crate) fn from_sdk(dir: &Path, tag: &str, index: &str) -> Result<Self, String> {
        Self::installed(dir, tag, Some(Source::Sdk(index)))
    }

    fn installed(dir: &Path, tag: &str, expected: Option<Source<'_>>) -> Result<Self, String> {
        require_dir(dir)?;
        let complete = dir.join(".installed");
        if super::open_if_file(&complete)?.is_none() {
            return Err(at(&complete, io::ErrorKind::NotFound.into()));
        }
        let path = dir.join(Self::ARCHIVE_RECEIPT);
        let archive: Option<ArchiveReceipt> = receipt(&path)?;
        let sdk: Option<SdkReceipt> = receipt(&dir.join(Self::SDK_RECEIPT))?;
        let (source, saved) = match (&archive, &sdk) {
            (Some(_), Some(_)) => return Err(unsettled!()),
            (Some(receipt), None) => (
                Some(Source::Archive(&receipt.archive)),
                Some(&receipt.image),
            ),
            (None, Some(receipt)) => (Some(Source::Sdk(&receipt.sdk.index)), Some(&receipt.image)),
            (None, None) => (None, None),
        };
        if expected.is_some_and(|expected| source != Some(expected))
            || source.is_some_and(|source| {
                let (Source::Archive(digest) | Source::Sdk(digest)) = source;
                !backend::installation::valid_archive_digest(digest)
            })
        {
            return Err(unsettled!());
        }
        let image = Self::image(
            dir,
            tag,
            archive.as_ref().map(|receipt| receipt.archive.as_str()),
            sdk.as_ref().map(|receipt| &receipt.sdk),
        )?;
        if saved.is_some_and(|saved| *saved != image) {
            return Err(unsettled!());
        }
        Ok(image)
    }

    fn image(
        dir: &Path,
        tag: &str,
        archive: Option<&str>,
        sdk: Option<&GameSlices>,
    ) -> Result<Self, String> {
        let path = dir.join(backend::CLIENT);
        let Some(executable) = digest_file(&path)? else {
            return Err(at(&path, io::ErrorKind::NotFound.into()));
        };
        let mut hash = Sha256::new();
        item(
            &mut hash,
            Self::ARCHIVE_RECEIPT.as_bytes(),
            archive.map(|archive| Sha256::digest(archive).into()),
        );
        if let Some(sdk) = sdk {
            let bytes = serde_json::to_vec(sdk).map_err(|error| error.to_string())?;
            item(
                &mut hash,
                Self::SDK_RECEIPT.as_bytes(),
                Some(Sha256::digest(bytes).into()),
            );
        }
        // Keep existing static receipts and Play journals valid when there is no deps/ tree.
        if let Some(deps) = tree(&dir.join("deps"), false)? {
            item(&mut hash, b"deps", Some(deps));
        }
        for file in [
            backend::GAME_BUILD_ARCHITECTURE,
            "sunrise-release.json",
            "research-lock.json",
            "native-mods.json",
            crate::preparer::BINARY,
        ] {
            item(&mut hash, file.as_bytes(), digest_file(&dir.join(file))?);
        }
        for name in [
            "native-data",
            "native-content",
            sunrise_launcher_smod::mod_list::NATIVE_DIR,
        ] {
            item(&mut hash, name.as_bytes(), tree(&dir.join(name), false)?);
        }
        for entry in entries(dir)? {
            let name = entry.file_name();
            let lower = name.to_string_lossy().to_ascii_lowercase();
            if lower.ends_with(".dll")
                || lower.ends_with(".dylib")
                || lower.ends_with(".so")
                || lower.contains(".so.")
            {
                let path = entry.path();
                let image = digest_file(&path)?;
                if image.is_none() {
                    return Err(at(&path, io::ErrorKind::NotFound.into()));
                }
                item(&mut hash, name.as_encoded_bytes(), image);
            }
        }
        Ok(Self {
            tag: tag.into(),
            executable,
            runtime: hash.finalize().into(),
        })
    }

    pub(super) fn verify_program(&self, path: &Path) -> Result<(), String> {
        if digest_file(path)? != Some(self.executable) {
            return Err(unsettled!());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveReceipt {
    archive: String,
    image: Artifact,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source<'a> {
    Archive(&'a str),
    Sdk(&'a str),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SdkReceipt {
    sdk: GameSlices,
    image: Artifact,
}

fn receipt<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    super::optional_bytes(path)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|error| at(path, error.into())))
        .transpose()
}

pub(super) fn tree(path: &Path, resources: bool) -> Result<Option<[u8; 32]>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && plain_file(&meta) => {
            let device = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    meta.dev()
                }
                #[cfg(not(unix))]
                {
                    0
                }
            };
            directory(path, 0, device, resources).map(Some)
        }
        Ok(_) => Err(at(path, io::ErrorKind::InvalidData.into())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(at(path, e)),
    }
}

fn directory(path: &Path, depth: usize, _device: u64, resources: bool) -> Result<[u8; 32], String> {
    if depth > 64 {
        return Err(at(path, io::ErrorKind::InvalidData.into()));
    }
    let mut hash = Sha256::new();
    for entry in entries(path)? {
        let name = entry.file_name();
        if depth == 0 && resources && super::resources::generated_cache(&name) {
            continue;
        }
        let entry_path = entry.path();
        let meta = fs::symlink_metadata(&entry_path).map_err(|e| at(&entry_path, e))?;
        if !plain_file(&meta) {
            return Err(at(&entry_path, io::ErrorKind::InvalidData.into()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if _device != meta.dev() {
                return Err(at(&entry_path, io::ErrorKind::InvalidData.into()));
            }
        }
        let image = if meta.is_dir() {
            hash.update(b"d");
            Some(directory(&entry_path, depth + 1, _device, resources)?)
        } else if meta.is_file() {
            hash.update(b"f");
            let image = digest_file(&entry_path)?;
            if image.is_none() {
                return Err(at(&entry_path, io::ErrorKind::NotFound.into()));
            }
            image
        } else {
            return Err(at(&entry_path, io::ErrorKind::InvalidData.into()));
        };
        item(&mut hash, name.as_encoded_bytes(), image);
    }
    Ok(hash.finalize().into())
}

fn entries(path: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let read = || fs::read_dir(path)?.collect::<io::Result<Vec<_>>>();
    let mut entries = read().map_err(|e| at(path, e))?;
    entries.sort_unstable_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn item(hash: &mut Sha256, name: &[u8], image: Option<[u8; 32]>) {
    hash.update((name.len() as u64).to_le_bytes());
    hash.update(name);
    match image {
        None => hash.update([0]),
        Some(bytes) => {
            hash.update([1]);
            hash.update(bytes);
        }
    }
}

fn at(path: &Path, error: io::Error) -> String {
    format!("{}: {error}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_covers_library_native_payload_and_architecture_but_not_logs() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        fs::write(dir.join(backend::CLIENT), b"game").unwrap();
        fs::write(dir.join(".installed"), []).unwrap();
        fs::create_dir(dir.join("native-data")).unwrap();
        fs::create_dir_all(dir.join("deps/nested")).unwrap();
        let paths = [
            "native.dll",
            "deps/nested/libsdk.so",
            "native-data/data.json",
            backend::GAME_BUILD_ARCHITECTURE,
        ];
        for path in paths {
            fs::write(dir.join(path), b"original").unwrap();
        }
        let capture = || Artifact::capture_at("tag", dir).unwrap();
        let sealed = capture();
        fs::write(dir.join("latest.log"), b"mutable game log").unwrap();
        assert_eq!(sealed, capture());
        for path in paths {
            fs::write(dir.join(path), b"different").unwrap();
            assert_ne!(sealed, capture(), "{path}");
            fs::write(dir.join(path), b"original").unwrap();
            assert_eq!(sealed, capture(), "{path}");
        }
        fs::write(dir.join("extra.so.1"), b"extra library").unwrap();
        assert_ne!(sealed, capture());
    }
}
