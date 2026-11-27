//! The running server's control API: its live status, the log and console commands.

use super::*;

/// The running supervisor's control API (`.sunrise/api.json`: its address and token).
#[derive(Clone, Debug)]
pub struct Api {
    address: String,
    token: String,
}

impl Api {
    /// The API of the server running in `dir`, if one runs.
    pub fn open(dir: &Path) -> Option<Self> {
        let text = fs::read_to_string(dir.join(".sunrise/api.json")).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        Some(Self {
            address: v["address"].as_str()?.to_string(),
            token: v["token"].as_str()?.to_string(),
        })
    }

    pub fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        // No proxy here on purpose: the server is on this computer, and this is asked every
        // second (`proxy` never sends a loopback address through one either).
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build();
        let request = agent
            .request(method, &format!("http://{}{path}", self.address))
            .set("Authorization", &format!("Bearer {}", self.token));
        let response = match body {
            Some(b) => request.send_json(b),
            None => request.call(),
        };
        match response {
            Ok(r) => r.into_json().map_err(|e| e.to_string()),
            Err(ureq::Error::Status(_, r)) => {
                let v: Value = r.into_json().unwrap_or(Value::Null);
                Err(v["error"]
                    .as_str()
                    .unwrap_or("the server refused")
                    .to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn status(&self) -> Option<Status> {
        let reply = self.call("GET", "/api/v1/status", None).ok()?;
        reply["phase"].as_str()?;
        Some(Status::parse(&reply))
    }

    /// The log lines after `since`, and the number to ask from next time.
    pub fn logs(&self, since: u64) -> Option<(u64, Vec<String>)> {
        let v = self
            .call("GET", &format!("/api/v1/logs?since={since}"), None)
            .ok()?;
        let lines = v["lines"]
            .as_array()?
            .iter()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect();
        Some((v["next"].as_u64().unwrap_or(since), lines))
    }

    /// One console command; what the game answered.
    pub fn command(&self, line: &str) -> Result<Vec<String>, String> {
        let v = self.call("POST", "/api/v1/command", Some(json!({ "line": line })))?;
        Ok(v["output"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect())
    }

    /// A one-time login link for the web panel.
    pub fn panel_link(&self) -> Result<String, String> {
        let v = self.call("POST", "/api/v1/panel/login", None)?;
        v["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| tr("the panel gave no link").into())
    }
}

/// A player online.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Player {
    pub name: String,
    pub uuid: String,
    pub ping: Option<u64>,
}

/// What the supervisor reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// starting, running, stopping or restarting (stopped when nothing answers).
    pub phase: String,
    pub listener: Option<std::net::SocketAddr>,
    pub processes: Option<(u64, u64)>,
    pub uptime: Option<u64>,
    pub crashes: u64,
    pub last_crash: Option<String>,
    pub tps: Option<f64>,
    pub mspt: Option<f64>,
    pub players: Vec<Player>,
    pub max_players: Option<u64>,
    pub ram_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
    /// Paused because nobody is online (vanilla's pause-when-empty).
    pub paused: bool,
    /// The release in use and a newer one, when the supervisor found one.
    pub release: Option<String>,
    pub update_available: Option<String>,
    /// An update under way: what it is doing now.
    pub update_busy: Option<String>,
    /// How the last update went (its message, and whether it worked).
    pub update_last: Option<(String, bool)>,
}

impl Status {
    pub fn stopped() -> Self {
        Self {
            phase: "stopped".into(),
            ..Self::default()
        }
    }

    pub fn unknown() -> Self {
        Self {
            phase: "unknown".into(),
            ..Self::default()
        }
    }

    pub fn parse(v: &Value) -> Self {
        let game = &v["sample"]["game"];
        let update = &v["update"];
        Self {
            phase: v["phase"].as_str().unwrap_or("stopped").to_string(),
            listener: None,
            processes: v["pid"].as_u64().zip(v["engine_pid"].as_u64()),
            uptime: v["engine_uptime_seconds"].as_u64(),
            crashes: v["crashes"].as_u64().unwrap_or(0),
            last_crash: v["last_crash"].as_str().map(str::to_string),
            tps: game["tps"].as_f64(),
            mspt: game["mspt"]["average"].as_f64(),
            players: game["players"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| Player {
                    name: p["name"].as_str().unwrap_or_default().to_string(),
                    uuid: p["uuid"].as_str().unwrap_or_default().to_string(),
                    ping: p["ping"].as_u64(),
                })
                .collect(),
            max_players: game["max_players"].as_u64(),
            ram_bytes: v["sample"]["rss_bytes"].as_u64(),
            cpu_percent: v["sample"]["cpu_percent"].as_f64(),
            paused: game["paused"] == true,
            release: update["installed"].as_str().map(str::to_string),
            update_available: update["available"].as_str().map(str::to_string),
            update_busy: update["busy"].as_str().map(str::to_string),
            update_last: update["last"]
                .as_str()
                .map(|m| (m.to_string(), update["ok"] != false)),
        }
    }

    pub fn running(&self) -> bool {
        !matches!(self.phase.as_str(), "stopped" | "unknown")
    }
}

/// The status of the server in `dir`: what its API says, or stopped.
pub fn status(dir: &Path) -> Status {
    Api::open(dir)
        .and_then(|api| api.status())
        .unwrap_or_else(Status::stopped)
}

/// Polls the server selected in Hosting. An unavailable supervisor is distinct from one
/// that has stopped; the latter can show its saved settings for a future start.
pub fn poll_hosting(
    dir: &Path,
    previous: Option<&Status>,
    since: u64,
) -> (Status, Option<(u64, Vec<String>)>) {
    // A live supervisor holds its lock: with the lock free, nobody is running the server,
    // whatever endpoint a supervisor that died left behind in `.sunrise/api.json`.
    let unanswered = || {
        let lock_free = match fs::File::open(dir.join(".sunrise/lock")) {
            Ok(lock) => lock.try_lock_shared().is_ok(),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        if lock_free {
            Status::stopped()
        } else {
            Status::unknown()
        }
    };
    let Some((api, mut status)) = Api::open(dir).and_then(|api| {
        let status = api.status()?;
        Some((api, status))
    }) else {
        return (unanswered(), None);
    };
    let mut logs = api.logs(since);
    if status.phase != "running" || status.processes.is_none() {
        return (status, logs);
    }

    let same_engine =
        |other: &Status| other.phase == "running" && other.processes == status.processes;
    if let Some(listener) = previous
        .filter(|other| same_engine(other) && status.uptime >= other.uptime)
        .and_then(|other| other.listener)
    {
        status.listener = Some(listener);
        return (status, logs);
    }

    // The game announces the actual bind it starts with. The supervisor's start marker
    // and the game's Done line bracket the startup in its log. Read that section once
    // per engine and verify that a restart did not overtake the log request.
    if since != 0 {
        let finished_startup = logs
            .as_ref()
            .is_some_and(|(_, lines)| lines.iter().any(|line| network::startup_done(line)));
        if previous.is_some_and(|other| same_engine(other) && status.uptime >= other.uptime)
            && !finished_startup
        {
            return (status, logs);
        }
        logs = api.logs(0).or(logs);
    }
    let listener = logs
        .as_ref()
        .and_then(|(_, lines)| network::listener_from_startup(lines));
    if let Some(listener) = listener {
        let Some(verified) = api.status() else {
            return (Status::unknown(), None);
        };
        if !same_engine(&verified) || verified.uptime < status.uptime {
            return (verified, None);
        }
        status = verified;
        status.listener = Some(listener);
    }
    // Keep showing the incremental console rather than replaying the startup at every poll.
    if since != 0 {
        if let Some((next, lines)) = &mut logs {
            let first = next.saturating_sub(lines.len() as u64);
            let skip = if since <= *next {
                since.saturating_sub(first).min(lines.len() as u64) as usize
            } else {
                0
            };
            lines.drain(..skip);
        }
    }
    (status, logs)
}
