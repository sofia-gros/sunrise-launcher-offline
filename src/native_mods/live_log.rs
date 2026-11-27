//! The log a running native build writes, for the build window's live output. Launcher-only:
//! `build_log.rs` is a copy of the game's file, so this stays apart from it.
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The most of a log's end that is read for its last lines.
const LIMIT: u64 = 64 << 10;

/// The SDK workspace and the `build.log` in it while a build runs; empty between builds.
static LIVE: Slot = Slot(Mutex::new(None));

/// Where a running build's log is. The process has one ([`LIVE`]); tests make their own.
struct Slot(Mutex<Option<Named>>);

#[derive(Clone, PartialEq)]
struct Named {
    workspace: PathBuf,
    log: PathBuf,
}

impl Slot {
    fn set(&self, named: Option<Named>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = named;
    }

    fn get(&self) -> Option<Named> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn name(&self, workspace: PathBuf, log: PathBuf) -> LiveLog<'_> {
        let named = Named { workspace, log };
        self.set(Some(named.clone()));
        LiveLog { slot: self, named }
    }

    fn tail(&self, lines: usize) -> Option<String> {
        let named = self.get()?;
        // The compiler runs mod code in this workspace, which can swap the log for a link or a
        // pipe; read it only as the regular file the launcher's own import accepts.
        super::workspace::regular_under(&named.workspace, &named.log).ok()?;
        tail(&named.log, lines)
    }
}

/// Names the log a running build writes for as long as it is held, and clears it on any exit
/// path when dropped. Native builds are serialized (one at a time per launcher, under the build
/// lock); a guard still clears only the name it set, never a later build's.
#[must_use = "the live log is cleared when the guard drops"]
pub(crate) struct LiveLog<'a> {
    slot: &'a Slot,
    named: Named,
}

impl LiveLog<'static> {
    /// Names `log`, which must be a regular file under `workspace` to be read.
    pub(crate) fn name(workspace: PathBuf, log: PathBuf) -> Self {
        LIVE.name(workspace, log)
    }
}

impl Drop for LiveLog<'_> {
    fn drop(&mut self) {
        let mut slot = self.slot.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref() == Some(&self.named) {
            *slot = None;
        }
    }
}

/// The last `lines` non-empty lines the running build has written so far, read from the end of
/// its log (at most 64 KiB). `None` when no build names a log or it holds no text yet: never an
/// earlier build's log, so the pane shows only this attempt's lines. It reads the disk, so call
/// it off the UI thread.
pub fn live_tail(lines: usize) -> Option<String> {
    LIVE.tail(lines)
}

/// Opens `path` for reading without following a final link or waiting on a pipe, and only if
/// the opened file is a regular one: a check before opening alone leaves a moment to swap it.
fn open_regular(path: &Path) -> Option<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

fn tail(path: &Path, lines: usize) -> Option<String> {
    let mut file = open_regular(path)?;
    let skip = file.metadata().ok()?.len().saturating_sub(LIMIT);
    file.seek(SeekFrom::Start(skip)).ok()?;
    let mut bytes = Vec::new();
    file.take(LIMIT).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // Drop a potentially partial first line only if there is newer non-blank output.
    let text = match text.split_once('\n') {
        Some((_, rest)) if skip > 0 && !rest.trim().is_empty() => rest,
        _ => &text[..],
    };
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    (!kept.is_empty()).then(|| kept[kept.len().saturating_sub(lines)..].join("\n"))
}

#[cfg(test)]
mod tests;
