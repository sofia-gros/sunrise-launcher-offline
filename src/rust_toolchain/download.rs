//! Downloading, checking and unpacking the pinned toolchain into the launcher's data folder.
use super::*;
use std::io::Read;

// Check the byte after the pinned prefix before the shared writer can commit it.
struct PinnedArchive<R>(std::io::Take<R>);
impl<R: Read> Read for PinnedArchive<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let n = self.0.read(buffer)?;
        if n == 0 && self.0.limit() == 0 && self.0.get_mut().read(&mut [0])? != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                crate::i18n::tr("Rust toolchain archive exceeds its pinned size."),
            ));
        }
        Ok(n)
    }
}

enum TransferChunk {
    Ready,
    Data(Vec<u8>),
    Eof,
    Err(String),
}

struct CancellableHttpReader<'a> {
    rx: std::sync::mpsc::Receiver<TransferChunk>,
    buf: Vec<u8>,
    pos: usize,
    cancellation: &'a crate::native_mods::Cancellation,
}

impl<'a> CancellableHttpReader<'a> {
    const POLL: std::time::Duration = std::time::Duration::from_millis(50);
    const CHUNK_SIZE: usize = 64 * 1024;

    fn start(
        url: &str,
        cancellation: &'a crate::native_mods::Cancellation,
    ) -> Result<Self, String> {
        use std::sync::mpsc::{self, RecvTimeoutError};

        cancellation.check()?;
        let (tx, rx) = mpsc::sync_channel::<TransferChunk>(8);
        let url = url.to_owned();
        let stop = cancellation.clone();
        std::thread::Builder::new()
            .name("toolchain-download".into())
            .spawn(move || {
                let response = match backend::get(&url) {
                    Ok(response) => response,
                    Err(error) => {
                        let _ = tx.send(TransferChunk::Err(error));
                        return;
                    }
                };
                if tx.send(TransferChunk::Ready).is_err() {
                    return;
                }
                let mut reader = response.into_reader();
                loop {
                    if stop.is_stopped() {
                        return;
                    }
                    let mut buf = vec![0u8; Self::CHUNK_SIZE];
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            let _ = tx.send(TransferChunk::Eof);
                            return;
                        }
                        Ok(n) => {
                            buf.truncate(n);
                            if tx.send(TransferChunk::Data(buf)).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(TransferChunk::Err(error.to_string()));
                            return;
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())?;

        loop {
            cancellation.check()?;
            match rx.recv_timeout(Self::POLL) {
                Ok(TransferChunk::Ready) => {
                    return Ok(Self {
                        rx,
                        buf: Vec::new(),
                        pos: 0,
                        cancellation,
                    });
                }
                Ok(TransferChunk::Err(error)) => return Err(error),
                Ok(TransferChunk::Data(_) | TransferChunk::Eof) => {
                    return Err(crate::i18n::tr("Unexpected download state.").into());
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    cancellation.check()?;
                    return Err(crate::i18n::tr("Download worker ended unexpectedly.").into());
                }
            }
        }
    }
}

impl Read for CancellableHttpReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::mpsc::RecvTimeoutError;

        self.cancellation.check().map_err(std::io::Error::other)?;
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos < self.buf.len() {
            let n = (self.buf.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        loop {
            self.cancellation.check().map_err(std::io::Error::other)?;
            match self.rx.recv_timeout(Self::POLL) {
                Ok(TransferChunk::Data(data)) => {
                    let n = data.len().min(out.len());
                    out[..n].copy_from_slice(&data[..n]);
                    self.buf = data;
                    self.pos = n;
                    return Ok(n);
                }
                Ok(TransferChunk::Eof) => return Ok(0),
                Ok(TransferChunk::Err(error)) => return Err(std::io::Error::other(error)),
                Ok(TransferChunk::Ready) => {
                    return Err(std::io::Error::other(crate::i18n::tr(
                        "Unexpected download state.",
                    )));
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    self.cancellation.check().map_err(std::io::Error::other)?;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        crate::i18n::tr("Download worker ended unexpectedly."),
                    ));
                }
            }
        }
    }
}

pub(super) fn download_archive(
    url: &str,
    bytes: u64,
    sha256: &str,
    to: &Path,
    progress: &impl Fn(f32),
    cancellation: &crate::native_mods::Cancellation,
) -> Result<(), String> {
    cancellation.check()?;
    let reader = CancellableHttpReader::start(url, cancellation)?;
    save_archive(reader, url, bytes, sha256, to, progress)
}

pub(super) fn save_archive(
    reader: impl Read,
    name: &str,
    bytes: u64,
    sha256: &str,
    to: &Path,
    progress: &impl Fn(f32),
) -> Result<(), String> {
    backend::save_checked(
        PinnedArchive(reader.take(bytes)),
        bytes,
        name,
        sha256,
        to,
        progress,
    )?;
    let actual = fs::metadata(to).map_err(|error| error.to_string())?.len();
    if actual != bytes {
        let _ = fs::remove_file(to);
        return Err(crate::i18n::trf(
            "Rust toolchain archive has {actual} bytes; expected {expected}.",
            &[
                ("actual", &actual.to_string()),
                ("expected", &bytes.to_string()),
            ],
        ));
    }
    Ok(())
}

pub(crate) fn unsupported_host() -> String {
    if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        crate::i18n::tr("Native mods are not supported by the Windows ARM64 launcher yet; its managed compiler is not available.").into()
    } else {
        crate::i18n::trf(
            "Native mods need Rust, and the launcher has none for {os} {arch}.",
            &[
                ("os", std::env::consts::OS),
                ("arch", std::env::consts::ARCH),
            ],
        )
    }
}

/// Downloads, checks and unpacks the pinned toolchain (once); `progress` gets the fraction done.
pub fn install(progress: impl Fn(f32)) -> Result<Toolchain, String> {
    install_cancellable(progress, &crate::native_mods::Cancellation::default())
}

pub(crate) fn install_cancellable(
    progress: impl Fn(f32),
    cancellation: &crate::native_mods::Cancellation,
) -> Result<Toolchain, String> {
    cancellation.check()?;
    if let Some(done) = installed() {
        return Ok(done);
    }
    let host = host().ok_or_else(unsupported_host)?;
    let dir = dir_for(host);
    let root = root();
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let scratch = tempfile::Builder::new()
        .prefix("toolchain-")
        .tempdir_in(&root)
        .map_err(|e| e.to_string())?;
    let partial = scratch.path().join("unpacked");
    fs::create_dir(&partial).map_err(|e| e.to_string())?;
    let total = host
        .components
        .iter()
        .map(|&(_, _, bytes)| bytes)
        .sum::<u64>()
        + host.c_toolchain.map_or(0, |compiler| compiler.bytes);
    let mut done = 0_u64;
    progress(0.0);
    for &(name, sha256, bytes) in host.components {
        let file = format!("{name}-{VERSION}-{}.tar.gz", host.target);
        let archive = scratch.path().join(&file);
        // save_checked accounts for 90% before verifying the checksum; only count received bytes.
        let step = |f: f32| {
            progress(
                (done as f64 + f64::from((f / 0.9).min(1.0)) * bytes as f64) as f32 / total as f32,
            )
        };
        download_archive(
            &format!("{DIST}/{file}"),
            bytes,
            sha256,
            &archive,
            &step,
            cancellation,
        )
        .map_err(|e| {
            crate::i18n::trf(
                "Rust {version} ({name}): {error}",
                &[
                    ("version", VERSION),
                    ("name", name),
                    ("error", &e.to_string()),
                ],
            )
        })?;
        let unpacked = unpack(&archive, &partial, cancellation);
        let _ = fs::remove_file(&archive);
        unpacked.map_err(|e| {
            crate::i18n::trf(
                "Rust {version} ({name}): {error}",
                &[
                    ("version", VERSION),
                    ("name", name),
                    ("error", &e.to_string()),
                ],
            )
        })?;
        done += bytes;
    }
    if let Some(compiler) = host.c_toolchain {
        let archive = scratch.path().join(format!(".{}.zip", compiler.directory));
        let step = |f: f32| {
            progress(
                (done as f64 + f64::from((f / 0.9).min(1.0)) * compiler.bytes as f64) as f32
                    / total as f32,
            )
        };
        download_archive(
            compiler.url,
            compiler.bytes,
            compiler.sha256,
            &archive,
            &step,
            cancellation,
        )
        .map_err(|e| format!("{}: {e}", compiler.version))?;
        let unpacked = match compiler.kind {
            CToolchainKind::WinLibs => unpack_mingw(&archive, &partial, cancellation),
            CToolchainKind::LlvmMingw => unpack_llvm_mingw(&archive, &partial, cancellation),
        };
        let _ = fs::remove_file(&archive);
        unpacked.map_err(|e| format!("{}: {e}", compiler.version))?;
        let toolchain = Toolchain {
            dir: partial.clone(),
            host,
        };
        if !toolchain.compiler_available() {
            return Err(crate::i18n::trf(
                "{compiler} did not contain the compiler and linker files the launcher needs.",
                &[("compiler", compiler.version)],
            ));
        }
    }
    cancellation.check()?;
    fs::write(partial.join(COMPLETE), VERSION).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&dir);
    fs::rename(&partial, &dir).map_err(|e| e.to_string())?;
    progress(1.0);
    Ok(Toolchain { dir, host })
}

/// Unpacks one component archive into `prefix`, as its installer would: an archive holds
/// `<top>/<component>/<files>` per component, and each component's files go under `prefix`. Its
/// installer script, lists and `manifest.in` stay out.
pub(super) fn unpack(
    archive: &Path,
    prefix: &Path,
    cancellation: &crate::native_mods::Cancellation,
) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut tar = tar::Archive::new(cancellation.reader(flate2::read::GzDecoder::new(file)));
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let Some(relative) = component_path(&path) else {
            continue;
        };
        let to = prefix.join(&relative);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        entry
            .unpack(&to)
            .map_err(|e| format!("{}: {e}", relative.display()))?;
    }
    Ok(())
}

/// Where a file of a component archive goes, relative to the prefix: `rustc-1.99.1-x/rustc/bin/rustc`
/// → `bin/rustc`. None for the archive's own files and anything that would leave the prefix.
pub(super) fn component_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let parts: Vec<&std::ffi::OsStr> = path
        .components()
        .map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [_, _, rest @ ..] if !rest.is_empty() && rest != ["manifest.in"] => {
            Some(rest.iter().collect())
        }
        _ => None,
    }
}

/// Unpacks MinGW-w64's zip into `prefix`: its `mingw64/` folder, documentation left out.
pub(super) fn unpack_mingw(
    archive: &Path,
    prefix: &Path,
    cancellation: &crate::native_mods::Cancellation,
) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        cancellation.check()?;
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(relative) = entry.enclosed_name().and_then(|p| mingw_path(&p)) else {
            continue;
        };
        let to = prefix.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&to).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = fs::File::create(&to).map_err(|e| format!("{}: {e}", relative.display()))?;
        std::io::copy(&mut cancellation.reader(&mut entry), &mut out)
            .map_err(|e| format!("{}: {e}", relative.display()))?;
    }
    Ok(())
}

/// Unpacks LLVM-MinGW under `llvm-mingw/`. Release archives have either a versioned top folder or
/// the toolchain folders at their root; both layouts are accepted, but paths may never escape it.
pub(super) fn unpack_llvm_mingw(
    archive: &Path,
    prefix: &Path,
    cancellation: &crate::native_mods::Cancellation,
) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        cancellation.check()?;
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(relative) = entry.enclosed_name().and_then(|p| llvm_mingw_path(&p)) else {
            continue;
        };
        let to = prefix.join(LLVM_MINGW_DIR).join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&to).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = fs::File::create(&to).map_err(|e| format!("{}: {e}", relative.display()))?;
        std::io::copy(&mut cancellation.reader(&mut entry), &mut out)
            .map_err(|e| format!("{}: {e}", relative.display()))?;
    }
    Ok(())
}

pub(super) fn llvm_mingw_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let parts: Vec<&std::ffi::OsStr> = path
        .components()
        .map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if parts.is_empty() {
        return None;
    }
    let contents = if parts[0].to_string_lossy().starts_with("llvm-mingw") {
        if parts.len() == 1 {
            return None;
        }
        &parts[1..]
    } else {
        &parts[..]
    };
    (!contents.is_empty()).then(|| contents.iter().collect())
}

/// Where a file of MinGW-w64's zip goes, relative to the prefix: `mingw64/bin/gcc.exe` as it is.
/// None for its documentation (`mingw64/share/`, 50 MB of the 925), the folder itself and anything
/// outside `mingw64/`.
pub(super) fn mingw_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let parts: Vec<&std::ffi::OsStr> = path
        .components()
        .map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [top, first, ..] if *top == MINGW_DIR && *first != "share" => Some(parts.iter().collect()),
        _ => None,
    }
}
