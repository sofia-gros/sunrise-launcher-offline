//! Running the game: preparing its command and runtime, spawning it under the preparation lock,
//! and watching it until it exits.
use super::{finish_preparation_cancellable, play_log, resources_owner, version_dir, Msg};
use crate::backend::lock::recover;
use crate::process::child::Child;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Job {
    Prepare,
    Client,
    Server,
}

/// A running process; its output arrives as `Msg::Line`, its end as `Msg::Exit`.
pub struct Running {
    pub child: Arc<Mutex<Child>>,
    pub stdin: Option<Box<dyn std::io::Write + Send>>,
}

/// Launcher-owned tree-lock primitives and optional Windows runtime state.
pub struct RuntimeGuard {
    _leases: Vec<crate::run_leases::Claim>,
    pub(crate) managed_session: Option<PathBuf>,
    #[cfg(windows)]
    _windows: Option<crate::windows_runtime::Guard>,
}

impl RuntimeGuard {
    /// Only a verified process exit retires a managed session early.
    pub fn game_exited(mut self) {
        crate::account::discard_launch_session(self.managed_session.take());
    }
}

#[cfg(windows)]
impl RuntimeGuard {
    pub(crate) fn windows(&self) -> Result<&crate::windows_runtime::Guard, String> {
        self._windows.as_ref().ok_or_else(|| {
            crate::i18n::tr("The profile's runtime copy was not prepared.").to_owned()
        })
    }
}

pub fn prepare_command(cmd: Command) -> Result<(Command, RuntimeGuard), String> {
    crate::developer_offline::validate(&cmd)?;
    let leases = crate::run_leases::shared(&cmd).map_err(crate::native_mods::cache_error)?;
    #[cfg(windows)]
    let cmd = crate::windows_runtime::redirect(cmd);
    #[cfg(windows)]
    let guard = crate::windows_runtime::prepare(&cmd).map_err(|e| {
        crate::i18n::trf(
            "Could not run {program}: {error}",
            &[
                ("program", &cmd.get_program().to_string_lossy()),
                ("error", &e.to_string()),
            ],
        )
    })?;
    Ok((
        cmd,
        RuntimeGuard {
            _leases: leases,
            managed_session: None,
            #[cfg(windows)]
            _windows: guard,
        },
    ))
}

/// Spawn a terminal-attached game through the same lifetime protocol as GUI launches.
pub fn spawn_attached(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    crate::run_leases::spawn(cmd, false)
}

pub fn spawn(job: Job, cmd: Command, tx: Sender<Msg>) -> Result<Running, String> {
    spawn_holding(job, cmd, tx, None)
}

/// Holds the preparation lock of the version that owns `version`'s resources (its base, for a
/// native build), so a second preparation of the same folder can't start while one runs; it is
/// released when the guard drops.
pub fn prepare_lock(version: &str) -> Result<crate::preparer::Preparation, String> {
    let owner = resources_owner(version);
    prepare_lock_at(&version_dir(&owner), &owner)
}

pub(crate) fn prepare_lock_at(
    dir: &Path,
    version: &str,
) -> Result<crate::preparer::Preparation, String> {
    let path = dir.join(".preparing.lock");
    let busy = || {
        crate::i18n::trf(
            "Sunrise {version} is already being prepared.",
            &[("version", version)],
        )
    };
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 0);
    let file = options.open(&path).map_err(|e| {
        #[cfg(windows)]
        if matches!(e.raw_os_error(), Some(32 | 33)) {
            return busy();
        }
        format!("{}: {e}", path.display())
    })?;
    #[cfg(unix)]
    file.try_lock().map_err(|e| match e {
        fs::TryLockError::WouldBlock => busy(),
        fs::TryLockError::Error(e) => format!("{}: {e}", path.display()),
    })?;
    Ok(crate::preparer::Preparation::new(file))
}

/// [`spawn`], keeping `hold` (a lock) until the program ends; a preparation's whole process group is stopped if the launcher quits first.
pub fn spawn_holding(
    job: Job,
    cmd: Command,
    tx: Sender<Msg>,
    prepare: Option<(String, crate::preparer::Preparation)>,
) -> Result<Running, String> {
    spawn_holding_cancellable(job, cmd, tx, prepare, None)
}

pub fn spawn_holding_cancellable(
    job: Job,
    cmd: Command,
    tx: Sender<Msg>,
    prepare: Option<(String, crate::preparer::Preparation)>,
    stop: Option<crate::native_mods::Cancellation>,
) -> Result<Running, String> {
    if let Some(stop) = &stop {
        stop.check()?;
    }
    let (cmd, runtime) = prepare_command(cmd)?;
    spawn_prepared(job, cmd, runtime, tx, prepare, stop)
}

pub(super) fn spawn_prepared(
    job: Job,
    mut cmd: Command,
    runtime: RuntimeGuard,
    tx: Sender<Msg>,
    prepare: Option<(String, crate::preparer::Preparation)>,
    stop: Option<crate::native_mods::Cancellation>,
) -> Result<Running, String> {
    if let Some((_, preparation)) = &prepare {
        let child = preparation
            .handoff(None)
            .spawn(cmd, true)
            .map_err(|e| e.to_string())?;
        return Ok(monitor(job, child, tx, runtime, prepare, stop));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    crate::process::hide_console(&mut cmd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = play_log::spawn(&mut cmd)?.into();
    Ok(monitor(job, child, tx, runtime, None, stop))
}

/// Use the same streams, process monitor and crash reporting for an admitted game's child.
pub fn monitor_game(child: Child, tx: Sender<Msg>, runtime: RuntimeGuard) -> Running {
    monitor(Job::Client, child, tx, runtime, None, None)
}

fn monitor(
    job: Job,
    mut child: Child,
    tx: Sender<Msg>,
    runtime: RuntimeGuard,
    prepare: Option<(String, crate::preparer::Preparation)>,
    stop: Option<crate::native_mods::Cancellation>,
) -> Running {
    let pid = child.id();
    let group = matches!(job, Job::Prepare);
    if group {
        crate::process::owned::CHILDREN.track_group(pid);
    }
    let tracked = stop
        .as_ref()
        .map(|stop| stop.track_process(pid))
        .transpose()
        .ok()
        .flatten();
    let stdin = child.stdin.take();
    let relayed = child.is_relayed();
    let forwarding = Arc::new(Mutex::new(true));
    // This channel closes once both pumps have reached EOF. The monitor may instead stop
    // forwarding at a bounded deadline if wrapper diagnostics still own the same pipes.
    let (reader_live, readers_done) = std::sync::mpsc::channel::<()>();
    let mut readers = Vec::new();
    for stream in [child.stdout.take(), child.stderr.take()]
        .into_iter()
        .flatten()
    {
        let tx = tx.clone();
        let forwarding = Arc::clone(&forwarding);
        let reader_live = reader_live.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let live = recover(&forwarding);
                if !*live || tx.send(Msg::Line(job, line)).is_err() {
                    break;
                }
            }
            drop(reader_live);
        }));
    }
    drop(reader_live);
    let child = Arc::new(Mutex::new(child));
    let waited = child.clone();
    std::thread::spawn(move || {
        let tracked = tracked;
        if !relayed {
            for r in readers {
                let _ = r.join();
            }
        }
        // Wrapper output can stay open after the game itself has exited.
        let status = loop {
            if let Some(s) = recover(&waited).try_wait().transpose() {
                break s;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        if relayed {
            // Exit can overtake the pumps. Give buffered final diagnostics time to reach
            // the log, but don't wait for the wrapper or its descendants to close a pipe.
            let _ = readers_done.recv_timeout(std::time::Duration::from_millis(200));
        }
        *recover(&forwarding) = false;
        if relayed {
            if let Err(error) = status {
                let _ = tx.send(Msg::ClientObservationLost(
                    crate::process::child::unconfirmed_exit(error),
                ));
                return;
            }
        }
        if group {
            crate::process::owned::CHILDREN.release(pid);
        }
        drop(tracked);
        let cancelled = stop.as_ref().is_some_and(|stop| stop.is_stopped());
        let result = if cancelled {
            runtime.game_exited();
            Err(crate::native_mods::CANCELLED.into())
        } else {
            status.map_err(|e| e.to_string()).and_then(|s| {
                let result = match prepare {
                    Some((tag, preparation)) if s.success() => {
                        finish_preparation_cancellable(&tag, &preparation, stop.as_ref())
                    }
                    _ if s.success() => Ok(()),
                    _ => Err(format!("Exited with {s}")),
                };
                runtime.game_exited();
                result
            })
        };
        let _ = tx.send(Msg::Exit(job, result));
    });
    Running { child, stdin }
}
