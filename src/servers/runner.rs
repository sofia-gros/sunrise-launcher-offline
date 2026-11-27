//! Running `sunrise-server`, here or under its supervisor, and what it says when it fails.

use super::*;

/// `sunrise-server --dir <dir> <args>`.
pub fn command(dir: &Path, args: &[&str]) -> Result<Command, String> {
    supervisor_command(dir, args, None)
}

fn supervisor_command(
    dir: &Path,
    args: &[&str],
    resources: Option<&Path>,
) -> Result<Command, String> {
    let dir = backend::server_root(dir)?;
    if args
        .iter()
        .any(|arg| *arg == "--dir" || arg.starts_with("--dir="))
    {
        return Err(tr("The server command cannot change its server folder.").into());
    }
    let program = program().ok_or(tr(
        "The server programs aren't set up yet: create a server, or press Prepare on its page.",
    ))?;
    if matches!(args.first(), Some(&"start" | &"restart" | &"run")) {
        publish_host_runtime(&dir, resources)?;
    }
    let mut cmd = crate::process::command(program);
    cmd.arg("--dir")
        .arg(&dir)
        .args(args)
        .env("SUNRISE_HOME", &dir);
    Ok(cmd)
}

/// The dedicated game starts directly, not through Play. Like Play it must be able to take
/// read leases on its install, resources and working directory. Publish them at explicit start:
/// this also covers older copies and new directories selected by a supervisor update.
fn publish_host_runtime(dir: &Path, resources: Option<&Path>) -> Result<(), String> {
    let root = runtime_root();
    let release = root
        .join("current")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let releases = backend::data_dir()
        .canonicalize()
        .map_err(|e| e.to_string())?
        .join("server-runtime/releases");
    // We own the completion marker only in the launcher's copy, never in a custom runtime.
    if release.parent() != Some(releases.as_path()) {
        return Ok(());
    }
    // Managed Start already asked the supervisor for its engine command before jar consent.
    // CLI starts use that same format-1 reply, never a guess based on the installed layout.
    let reported;
    let resources = match resources {
        Some(path) => path,
        None => {
            let probed = match engine_command(dir) {
                // A live supervisor handles the verb itself, even if the installed release
                // cannot describe a ready engine or still needs its own resources prepared.
                Ok(Some((_, resources))) if resources.pending && status(dir).running() => {
                    return Ok(());
                }
                Ok(probed) => probed,
                Err(_) if status(dir).running() => return Ok(()),
                Err(problem) => return Err(problem.message),
            };
            let Some(probed) = probed else {
                // Before engine-command the CLI forwarded these verbs directly. There is
                // no authoritative resource path to publish for those supervisors.
                return Ok(());
            };
            (_, reported) =
                prepare_host_resources(dir, probed).map_err(|problem| problem.message)?;
            &reported
        }
    };
    let _publication =
        crate::run_leases::installation().map_err(crate::native_mods::cache_error)?;
    for unit in [release.as_path(), resources, dir] {
        crate::run_leases::publish(unit).map_err(crate::native_mods::cache_error)?;
    }
    match fs::File::create_new(release.join(".installed")) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn supervisor_log(dir: &Path) -> PathBuf {
    dir.join("logs/supervisor.log")
}

fn log_length(dir: &Path) -> u64 {
    fs::metadata(supervisor_log(dir)).map_or(0, |m| m.len())
}

/// Runs one `sunrise-server` command to its end: what it printed, or the problem in plain words.
pub fn run(dir: &Path, args: &[&str]) -> Result<String, Problem> {
    let since = log_length(dir);
    let output = command(dir, args)
        .map_err(Problem::plain)?
        .stdin(Stdio::null())
        .output()
        .map_err(|e| unrun(&e))?;
    let out = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(out)
    } else {
        let err = String::from_utf8_lossy(&output.stderr);
        Err(Problem::explain(&format!("{out}\n{err}"), dir, since))
    }
}

/// Runs a long `sunrise-server` command, passing each line it prints to `line`.
pub fn run_streamed(dir: &Path, args: &[&str], line: &dyn Fn(String)) -> Result<(), Problem> {
    stream(dir, command(dir, args).map_err(Problem::plain)?, line)
}

/// Launcher-managed Start/Restart: prepare data using the supervisor's own command description.
/// The standalone supervisor still has no jar downloader or consent policy; a missing jar listed
/// by a registry mod of the server's folder downloads here without asking, like a client's start.
pub fn run_managed(
    dir: &Path,
    args: &[&str],
    line: &dyn Fn(String),
    permission: crate::jars::Permission<'_>,
) -> Result<(), Problem> {
    let probed = engine_command(dir)?.ok_or_else(|| {
        Problem::plain(tr(
            "This server's Sunrise build is too old to start from the launcher: update the server to a newer build.",
        ))
    })?;
    let (mut engine, resources) = prepare_host_resources(dir, probed)?;
    crate::jars::prepare_launch(&mut engine, &crate::launch::Launch::default(), permission)
        .map_err(|error| Problem::plain(error.to_string()))?;
    let supervisor = supervisor_command(dir, args, Some(&resources)).map_err(Problem::plain)?;
    stream(dir, supervisor, line)
}

/// The `format` of `sunrise-server engine-command --json` this launcher reads (sunrise-org/sunrise's docs/api/engine-command.md).
const ENGINE_COMMAND_FORMAT: u64 = 1;

#[derive(Debug)]
pub(super) struct EngineResources {
    pub(super) path: PathBuf,
    pub(super) pending: bool,
}

/// The engine and resources the supervisor would use, or `None` for a pre-protocol supervisor.
fn engine_command(dir: &Path) -> Result<Option<(Command, EngineResources)>, Problem> {
    let output = command(dir, &["engine-command", "--json"])
        .map_err(Problem::plain)?
        .stdin(Stdio::null())
        .output()
        .map_err(|e| unrun(&e))?;
    engine_from_reply(&output.stdout)
}

fn prepare_host_resources(
    dir: &Path,
    (engine, resources): (Command, EngineResources),
) -> Result<(Command, PathBuf), Problem> {
    if !resources.pending {
        return Ok((engine, resources.path));
    }
    let expected = resources.path;
    // For a pending managed install, init repairs its resources without changing server.toml
    // or recording a new EULA agreement (sunrise-org/sunrise#4620).
    run(dir, &["init", "--yes"])?;
    match engine_command(dir)? {
        Some((engine, resources)) if !resources.pending && resources.path == expected => {
            Ok((engine, resources.path))
        }
        _ => Err(Problem::plain(tr(
            "The server's game files are still not prepared.",
        ))),
    }
}

/// Why `sunrise-server` couldn't be run, in the launcher's language.
fn unrun(error: &std::io::Error) -> Problem {
    Problem::plain(trf(
        "Couldn't run sunrise-server: {error}",
        &[("error", &error.to_string())],
    ))
}

/// [`engine_command`]'s reply as a command, or the problem it names: missing game files are [`Fix::Prepare`].
pub(super) fn engine_from_reply(
    stdout: &[u8],
) -> Result<Option<(Command, EngineResources)>, Problem> {
    // A build from before `engine-command` answers with its usage, never with a JSON object, whatever its exit code.
    let Some(reply) = serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .filter(serde_json::Value::is_object)
    else {
        return Ok(None);
    };
    if reply["format"]
        .as_u64()
        .is_none_or(|format| format > ENGINE_COMMAND_FORMAT)
    {
        return Err(Problem::plain(tr(
            "This server's Sunrise build is newer than the launcher understands: update the launcher.",
        )));
    }
    if let Some(error) = reply.get("error") {
        let mut problem = Problem::plain(error["message"].as_str().unwrap_or_default());
        if error["kind"] == "resources" {
            problem.fix = Some(Fix::Prepare);
        }
        return Err(problem);
    }
    let unreadable = || {
        Problem::plain(tr(
            "This server's Sunrise build answered with a start command the launcher can't read: update the launcher.",
        ))
    };
    let text = |key: &str| reply[key].as_str().ok_or_else(unreadable);
    let mut engine = crate::process::command(text("program")?);
    for arg in reply["args"].as_array().into_iter().flatten() {
        engine.arg(arg.as_str().ok_or_else(unreadable)?);
    }
    engine.current_dir(text("cwd")?);
    for (key, value) in reply["env"].as_object().into_iter().flatten() {
        match value.as_str() {
            Some(value) => engine.env(key, value),
            None => engine.env_remove(key),
        };
    }
    let resources = EngineResources {
        path: PathBuf::from(text("resources")?),
        pending: Option::<bool>::deserialize(&reply["resources_pending"])
            .map_err(|_| unreadable())?
            .unwrap_or_default(),
    };
    Ok(Some((engine, resources)))
}

fn stream(dir: &Path, mut command: Command, line: &dyn Fn(String)) -> Result<(), Problem> {
    use std::io::{BufRead, BufReader, Read};
    let since = log_length(dir);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| unrun(&e))?;
    let (tx_err, rx_err) = std::sync::mpsc::channel();
    let mut err = child.stderr.take();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(e) = &mut err {
            let _ = e.read_to_string(&mut text);
        }
        let _ = tx_err.send(text);
    });
    let (tx_line, rx_line) = std::sync::mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx_line.send(l).is_err() {
                    break;
                }
            }
        });
    }
    let mut out = String::new();
    let status = loop {
        while let Ok(l) = rx_line.try_recv() {
            let shown = l
                .trim_start_matches(['.', ' '])
                .trim_start_matches("Starting");
            let shown = shown.trim_start_matches('.').trim();
            if !shown.is_empty() {
                line(shown.to_string());
            }
            out.push_str(&l);
            out.push('\n');
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|e| Problem::plain(e.to_string()))?
        {
            std::thread::sleep(Duration::from_millis(100));
            while let Ok(l) = rx_line.try_recv() {
                let shown = l
                    .trim_start_matches(['.', ' '])
                    .trim_start_matches("Starting");
                let shown = shown.trim_start_matches('.').trim();
                if !shown.is_empty() {
                    line(shown.to_string());
                }
                out.push_str(&l);
                out.push('\n');
            }
            break status;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let err = rx_err
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(Problem::explain(&format!("{out}\n{err}"), dir, since))
    }
}

/// What can be done about a problem, with one button.
#[derive(Clone, Debug, PartialEq)]
pub enum Fix {
    /// The port is taken: use this free one.
    UsePort(u16),
    /// The game files are missing: prepare them (Play page) and copy them for the servers.
    Prepare,
    /// The EULA isn't agreed to.
    AgreeEula,
    /// The game's own log says more.
    ShowLog,
}

/// A problem, in plain words.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub message: String,
    pub fix: Option<Fix>,
}

impl Problem {
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fix: None,
        }
    }

    /// Reads what `sunrise-server` (and the game log it points at) said.
    /// `since` is the background server's log length before the command: only what it logged after counts.
    pub fn explain(output: &str, dir: &Path, since: u64) -> Self {
        let log = fs::read(supervisor_log(dir)).unwrap_or_default();
        let new = log.get(since as usize..).unwrap_or_default();
        Self::explain_with(output, &String::from_utf8_lossy(new), dir)
    }

    pub(super) fn explain_with(output: &str, log: &str, dir: &Path) -> Self {
        let both = format!("{output}\n{log}");
        let has = |s: &str| both.contains(s);
        // Vanilla says FAILED TO BIND TO PORT for any failure while starting; only these are a busy port.
        let busy = [
            "already in use",
            "Address in use",
            "Only one usage of each socket address",
        ];
        if busy.iter().any(|b| both.contains(b)) {
            let port = config(dir)
                .and_then(|c| c["server"]["port"].as_u64())
                .unwrap_or(25565) as u16;
            return Self {
                message: trf(
                    "Port {port} is already used by another program (maybe another server). Use a free port instead: friends then add it to the address.",
                    &[("port", &port.to_string())],
                ),
                fix: Some(Fix::UsePort(free_port(port + 1, &[]))),
            };
        }
        if has("no prepared game files") || has("not found next to the resources") {
            return Self {
                message: tr(
                    "The server needs the game files from your own Minecraft. Prepare them: it takes a few minutes, once.",
                )
                .into(),
                fix: Some(Fix::Prepare),
            };
        }
        if has("EULA") && !has("eula=true") {
            return Self {
                message: tr("Running a server needs your agreement to the Minecraft EULA.").into(),
                fix: Some(Fix::AgreeEula),
            };
        }
        if has("isn't up after") || has("the server stopped") {
            return Self {
                message: tr("The game didn't start. Its log below says why.").into(),
                fix: Some(Fix::ShowLog),
            };
        }
        let said = both
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("sunrise-server: "))
            .map(str::to_string)
            .or_else(|| {
                output
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| tr("sunrise-server failed without saying why").into());
        Self::plain(capitalised(&said))
    }
}

pub(super) fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}
