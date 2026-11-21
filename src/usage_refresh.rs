//! Ask each native tool for usage under its own profile home. Each probe owns
//! one child and its stdio. Only normalized windows and failure classes reach
//! rtr's state. Claude remains responsible for writing its usage cache.

use std::{
    collections::BTreeMap,
    fs::{self, File, TryLockError},
    io::{self, BufRead, BufReader, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    file_lock::write_private_atomic,
    limits::{Meter, FRESHNESS_MINUTES},
    native_usage::{self, AppServerBucket, NativeRead},
    paths::{create_private_dir_all, Paths},
    tool_specs::{self, ToolSpec},
};

const PARALLELISM: usize = 4;
const PROFILE_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(8);
const WAIT_INTERVAL: Duration = Duration::from_millis(10);
const MAX_LINE_BYTES: usize = 1024 * 1024;

pub(crate) struct Target<'a> {
    pub tool: &'static ToolSpec,
    pub profile: &'a str,
    pub command: &'a [String],
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Failure {
    Timeout,
    Unavailable,
    Login,
    Error,
}

impl Failure {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Login => "login",
            Self::Error => "error",
        }
    }
}

/// Persist attempt status by tool and profile, and keep Codex observations
/// beside it. Claude's window data stays in its native cache.
#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct Store {
    tools: BTreeMap<String, BTreeMap<String, ProfileRefresh>>,
}

/// An attempt and a measurement have independent clocks. A failure advances
/// the status without discarding the last usable Codex windows.
#[derive(Clone, Default, Deserialize, Serialize)]
struct ProfileRefresh {
    #[serde(default)]
    home: Option<HomeIdentity>,
    attempt: Option<Attempt>,
    observation: Option<AppObservation>,
}

/// Profile names can be reused after `rtr rm`. Tie the saved evidence to the
/// directory behind the name using filesystem metadata alone.
#[derive(Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
struct HomeIdentity {
    device: u64,
    inode: u64,
    created_at: Option<DateTime<Utc>>,
}

impl HomeIdentity {
    fn read(home: &Path) -> Option<Self> {
        let metadata = home.symlink_metadata().ok()?;
        if !metadata.is_dir() {
            return None;
        }
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            created_at: metadata.created().ok().map(DateTime::from),
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Attempt {
    started_at: DateTime<Utc>,
    failure: Option<Failure>,
}

#[derive(Clone, Deserialize, Serialize)]
struct AppObservation {
    observed_at: DateTime<Utc>,
    source: AppSource,
    windows: Vec<AppServerBucket>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum AppSource {
    AppServer,
}

impl Store {
    pub(crate) fn load(paths: &Paths) -> Self {
        load_store(paths).unwrap_or_else(|_| {
            eprintln!("rtr: could not read usage refresh state");
            Self::default()
        })
    }

    pub(crate) fn failure(&self, tool: &str, profile: &str) -> Option<Failure> {
        self.tools
            .get(tool)?
            .get(profile)?
            .attempt
            .as_ref()?
            .failure
    }

    /// App Server and rollout snapshots can cover different windows. Replace
    /// just the older readings for the same meter and keep rollout-only meters.
    pub(crate) fn merge_codex(&self, profile: &str, meters: &mut Vec<Meter>, now: DateTime<Utc>) {
        let Some(observation) = self
            .tools
            .get(tool_specs::CODEX.name)
            .and_then(|profiles| profiles.get(profile))
            .and_then(|profile| profile.observation.as_ref())
            .filter(|observation| observation.observed_at <= now)
        else {
            return;
        };
        let mut replacements = Vec::new();
        for meter in observation
            .windows
            .iter()
            .flat_map(|bucket| bucket.meters(observation.observed_at))
        {
            if !meters
                .iter()
                .any(|prior| prior.key == meter.key && prior.observed_at > meter.observed_at)
            {
                replacements.push(meter);
            }
        }
        meters.retain(|prior| !replacements.iter().any(|meter| meter.key == prior.key));
        meters.extend(replacements);
    }

    fn record(&mut self, updates: &[Update<'_>], keep_observations: bool) {
        for update in updates {
            let state = self
                .tools
                .entry(update.tool.into())
                .or_default()
                .entry(update.profile.into())
                .or_default();
            if state.home != update.home {
                *state = ProfileRefresh {
                    home: update.home,
                    ..ProfileRefresh::default()
                };
            }
            let failure = match &update.result {
                Ok(Probe::Codex(_)) if !keep_observations => Some(Failure::Error),
                result => result.as_ref().err().copied(),
            };
            if state
                .attempt
                .as_ref()
                .is_none_or(|prior| update.started_at >= prior.started_at)
            {
                state.attempt = Some(Attempt {
                    started_at: update.started_at,
                    failure,
                });
            }
            match &update.result {
                Ok(Probe::Codex(observation))
                    if keep_observations
                        && state
                            .observation
                            .as_ref()
                            .is_none_or(|prior| observation.observed_at >= prior.observed_at) =>
                {
                    state.observation = Some(observation.clone());
                }
                _ => {}
            }
        }
    }
}

/// Routing sees saved Codex evidence and Claude's verified cache reads. A
/// later reservation can use those same observations after a limit exit.
pub(crate) struct Batch<'a> {
    pub store: Store,
    pub claude: BTreeMap<&'a str, NativeRead>,
}

struct Update<'a> {
    tool: &'static str,
    profile: &'a str,
    home: Option<HomeIdentity>,
    started_at: DateTime<Utc>,
    result: Result<Probe, Failure>,
}

enum Probe {
    Claude(NativeRead),
    Codex(AppObservation),
}

pub(crate) fn refresh<'a>(paths: &Paths, targets: &[Target<'a>]) -> Batch<'a> {
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let next = AtomicUsize::new(0);
    let mut updates: Vec<_> = thread::scope(|scope| {
        let workers: Vec<_> = (0..targets.len().min(PARALLELISM))
            .map(|_| {
                scope.spawn(|| {
                    let mut updates = Vec::new();
                    while let Some(target) = targets.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let started_at = Utc::now();
                        let end = deadline.min(Instant::now() + PROFILE_TIMEOUT);
                        let home = paths.profile_home_dir(target.tool.name, target.profile);
                        updates.push(Update {
                            tool: target.tool.name,
                            profile: target.profile,
                            home: HomeIdentity::read(&home),
                            started_at,
                            result: probe(&home, target, end),
                        });
                    }
                    updates
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("usage refresh worker panicked"))
            .collect()
    });

    let path = store_path(paths);
    let lock = (!updates.is_empty()).then(|| lock_store(&path, deadline));
    if lock.as_ref().is_some_and(Result::is_err) {
        eprintln!("rtr: could not lock usage refresh state");
    }
    // A profile can be removed while the native process is answering. Its
    // result belongs to the directory that existed when the probe started.
    updates.retain(|update| {
        HomeIdentity::read(&paths.profile_home_dir(update.tool, update.profile)) == update.home
    });
    // Keep the lock through read, merge and rename. If persistence fails,
    // preserve the readings that the first reservation and rerun can both see.
    let saved = load_store(paths);
    let can_save = lock.as_ref().is_some_and(Result::is_ok) && saved.is_ok();
    let mut store = saved.unwrap_or_else(|_| {
        eprintln!("rtr: could not read usage refresh state");
        Store::default()
    });
    let mut wrote = false;
    if can_save {
        let mut candidate = store.clone();
        candidate.record(&updates, true);
        let result = serde_json::to_string_pretty(&candidate)
            .map_err(anyhow::Error::from)
            .and_then(|contents| write_private_atomic(&path, &contents));
        if result.is_ok() {
            store = candidate;
            wrote = true;
        } else {
            eprintln!("rtr: could not save usage refresh state");
        }
    }
    if !wrote {
        store.record(&updates, false);
    }
    let claude = updates
        .into_iter()
        .filter_map(|update| match update.result {
            Ok(Probe::Claude(read)) => Some((update.profile, read)),
            _ => None,
        })
        .collect();
    Batch { store, claude }
}

pub(crate) fn is_fresh(meter: &Meter, now: DateTime<Utc>) -> bool {
    meter.observed_at <= now
        && now - meter.observed_at <= chrono::Duration::minutes(FRESHNESS_MINUTES)
        && meter.reset_at.is_none_or(|reset| reset > now)
}

fn store_path(paths: &Paths) -> PathBuf {
    paths.state_dir.join("usage-refresh.json")
}

fn load_store(paths: &Paths) -> anyhow::Result<Store> {
    let bytes = match fs::read(store_path(paths)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(error) => return Err(error.into()),
    };
    let mut store: Store = serde_json::from_slice(&bytes)?;
    for (tool, profiles) in &mut store.tools {
        profiles.retain(|profile, entry| {
            let current = HomeIdentity::read(&paths.profile_home_dir(tool, profile));
            if entry.home.is_some() {
                return entry.home == current;
            }
            if current.is_none() {
                return entry.observation.is_none();
            }
            // State written before home identity was recorded can still be
            // used when the directory predates the saved evidence.
            let Some(created) = current.and_then(|home| home.created_at) else {
                return false;
            };
            entry.home = current;
            entry.attempt = entry
                .attempt
                .take()
                .filter(|attempt| attempt.started_at >= created);
            entry.observation = entry
                .observation
                .take()
                .filter(|obs| obs.observed_at >= created);
            entry.attempt.is_some() || entry.observation.is_some()
        });
    }
    Ok(store)
}

fn lock_store(path: &Path, deadline: Instant) -> anyhow::Result<File> {
    let parent = path.parent().expect("usage state has a parent");
    create_private_dir_all(parent)?;
    // Lock the directory inode so replacing the JSON file keeps the same
    // lock and a refresh needs no extra state file.
    let file = File::open(parent)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(error)) => return Err(error.into()),
            Err(TryLockError::WouldBlock) if Instant::now() >= deadline => {
                anyhow::bail!("usage state lock timed out");
            }
            Err(TryLockError::WouldBlock) => sleep_until(deadline),
        }
    }
}

fn probe(home: &Path, target: &Target<'_>, deadline: Instant) -> Result<Probe, Failure> {
    check_deadline(deadline)?;
    // A missing home is unprovisioned. Let the real launch create it. Check
    // the profile path components so a refresh cannot follow a sibling home.
    for dir in home.ancestors().take(3) {
        match dir.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(Failure::Unavailable);
            }
            _ => return Err(Failure::Error),
        }
    }
    let (program, prefix) = target.command.split_first().ok_or(Failure::Error)?;
    let mut command = Command::new(program);
    command
        .args(prefix)
        .env_clear()
        .env(target.tool.native_home_env, home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    // Only ordinary process settings cross this boundary. The tool resolves
    // its own login, even when rtr was invoked from another native session.
    for key in [
        "HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "LANG", "LC_ALL",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if let Some(key) = target.tool.native_secure_storage_env {
        command.env(key, home);
        command.env("DISABLE_AUTOUPDATER", "1");
    }
    let args: &[_] = if target.tool == &tool_specs::CLAUDE {
        &[
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--safe-mode",
            "--no-session-persistence",
        ]
    } else {
        &["app-server", "--listen", "stdio://"]
    };
    check_deadline(deadline)?;
    let mut child = command.args(args).spawn().map_err(|_| Failure::Error)?;
    let mut input = child.stdin.take().expect("refresh child has stdin");
    let mut output = BufReader::new(child.stdout.take().expect("refresh child has stdout"));
    let result = if target.tool == &tool_specs::CLAUDE {
        claude_request(&mut input, &mut output, deadline).map(|()| None)
    } else {
        codex_request(&mut input, &mut output, deadline).map(Some)
    };
    drop(input);
    let exit = wait_for_exit(&mut child, &mut output, deadline);
    let status = match exit {
        Ok(status) => status,
        Err(error) => {
            // The child is the leader of a fresh process group. npm and
            // other wrappers can have a native process beneath that PID.
            let pid = child.id() as libc::pid_t;
            // SAFETY: the unreaped child still owns the group ID we assigned.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
                let _ = child.kill();
            }
            let _ = child.wait();
            return Err(error);
        }
    };
    let response = result?;
    if !status.success() {
        return Err(Failure::Error);
    }
    match response {
        Some(observation) => Ok(Probe::Codex(observation)),
        None => {
            // Claude can answer from a fallback snapshot. Trust the cache's
            // fetch time, since the reply omits when usage was observed.
            let now = Utc::now();
            native_usage::claude(home, now)
                .filter(|read| read.meters.iter().any(|meter| is_fresh(meter, now)))
                .map(Probe::Claude)
                .ok_or(Failure::Unavailable)
        }
    }
}

fn claude_request(
    mut input: impl Write,
    output: &mut BufReader<impl io::Read + AsRawFd>,
    deadline: Instant,
) -> Result<(), Failure> {
    send(
        &mut input,
        json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}}),
    )?;
    claude_reply(output, deadline, "init")?;
    send(
        &mut input,
        json!({"type":"control_request","request_id":"usage","request":{
            "subtype":"get_usage","skip_behaviors":true
        }}),
    )?;
    let reply = claude_reply(output, deadline, "usage")?;
    let usage = &reply["response"];
    if usage["rate_limits_available"].as_bool() != Some(true) || !usage["rate_limits"].is_object() {
        return Err(Failure::Unavailable);
    }
    Ok(())
}

fn claude_reply(
    output: &mut BufReader<impl io::Read + AsRawFd>,
    deadline: Instant,
    id: &str,
) -> Result<Value, Failure> {
    let reply = read_reply(output, deadline, |row| {
        row["type"] == "control_response" && row["response"]["request_id"] == id
    })?;
    let response = &reply["response"];
    match response["subtype"].as_str() {
        Some("success") => Ok(reply["response"].clone()),
        Some("error") => Err(native_error(&response["error"], Failure::Error)),
        _ => Err(Failure::Error),
    }
}

fn codex_request(
    mut input: impl Write,
    output: &mut BufReader<impl io::Read + AsRawFd>,
    deadline: Instant,
) -> Result<AppObservation, Failure> {
    send(
        &mut input,
        json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"rtr","version":env!("CARGO_PKG_VERSION")}
        }}),
    )?;
    codex_reply(output, deadline, 1, Failure::Error)?;
    send(&mut input, json!({"method":"initialized"}))?;
    send(
        &mut input,
        json!({"id":2,"method":"account/rateLimits/read","params":{"excludeResetCreditDetails":true}}),
    )?;
    let reply = codex_reply(output, deadline, 2, Failure::Unavailable)?;
    let observed_at = Utc::now();
    let windows = AppServerBucket::parse_reply(&reply["result"]);
    if windows.is_empty() {
        return Err(Failure::Unavailable);
    }
    Ok(AppObservation {
        observed_at,
        source: AppSource::AppServer,
        windows,
    })
}

fn codex_reply(
    output: &mut BufReader<impl io::Read + AsRawFd>,
    deadline: Instant,
    id: u32,
    failure: Failure,
) -> Result<Value, Failure> {
    let reply = read_reply(output, deadline, |row| {
        row["id"] == id && row["method"].is_null()
    })?;
    if !reply["error"].is_null() {
        return Err(native_error(&reply["error"]["message"], failure));
    }
    reply["result"].is_object().then_some(reply).ok_or(failure)
}

fn native_error(error: &Value, fallback: Failure) -> Failure {
    let message = error.as_str().unwrap_or_default().to_ascii_lowercase();
    if [
        "authentication required",
        "authentication is required",
        "not logged in",
        "log in again",
        "login required",
        "unauthorized",
    ]
    .iter()
    .any(|reason| message.contains(reason))
    {
        Failure::Login
    } else {
        fallback
    }
}

fn send(mut input: impl Write, message: Value) -> Result<(), Failure> {
    // Each request is smaller than an empty pipe's capacity, and we send
    // at most two before waiting for the corresponding response.
    serde_json::to_writer(&mut input, &message).map_err(|_| Failure::Error)?;
    input.write_all(b"\n").map_err(|_| Failure::Error)
}

/// Correlate replies on stdout and discard startup notifications. Poll the
/// one pipe reader before filling its buffer, so a stalled or partial line
/// never prevents the worker from killing and reaping its child.
fn read_reply(
    output: &mut BufReader<impl io::Read + AsRawFd>,
    deadline: Instant,
    matches: impl Fn(&Value) -> bool,
) -> Result<Value, Failure> {
    let mut line = Vec::new();
    loop {
        check_deadline(deadline)?;
        if output.buffer().is_empty() {
            poll_stdout(output.get_ref().as_raw_fd(), deadline)?;
        }
        let chunk = output.fill_buf().map_err(|_| Failure::Error)?;
        if chunk.is_empty() {
            return Err(Failure::Error);
        }
        let newline = memchr::memchr(b'\n', chunk);
        let len = newline.map_or(chunk.len(), |index| index + 1);
        if line.len() + len > MAX_LINE_BYTES {
            return Err(Failure::Error);
        }
        line.extend_from_slice(&chunk[..len]);
        output.consume(len);
        if newline.is_some() {
            if let Ok(message) = serde_json::from_slice(&line) {
                if matches(&message) {
                    return Ok(message);
                }
            }
            line.clear();
        }
    }
}

fn poll_stdout(fd: RawFd, deadline: Instant) -> Result<(), Failure> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        check_deadline(deadline)?;
        let millis = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1) as libc::c_int;
        // SAFETY: descriptor is valid for one pollfd, with one live child pipe.
        match unsafe { libc::poll(&mut descriptor, 1, millis) } {
            count if count > 0 => return Ok(()),
            0 => {}
            _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return Err(Failure::Error),
        }
    }
}

fn wait_for_exit(
    child: &mut Child,
    output: &mut BufReader<impl io::Read + AsRawFd>,
    deadline: Instant,
) -> Result<ExitStatus, Failure> {
    // Discard trailing output after closing stdin. A wrapper or native tool
    // can otherwise block on a full pipe while we're waiting for its exit.
    let mut stdout_closed = false;
    loop {
        if let Some(status) = child.try_wait().map_err(|_| Failure::Error)? {
            return Ok(status);
        }
        check_deadline(deadline)?;
        if stdout_closed {
            sleep_until(deadline);
            continue;
        }
        if output.buffer().is_empty() {
            let tick = deadline.min(Instant::now() + WAIT_INTERVAL);
            match poll_stdout(output.get_ref().as_raw_fd(), tick) {
                Ok(()) => {}
                Err(Failure::Timeout) => continue,
                Err(error) => return Err(error),
            }
        }
        let len = output.fill_buf().map_err(|_| Failure::Error)?.len();
        output.consume(len);
        stdout_closed = len == 0;
    }
}

fn check_deadline(deadline: Instant) -> Result<(), Failure> {
    (Instant::now() < deadline)
        .then_some(())
        .ok_or(Failure::Timeout)
}

fn sleep_until(deadline: Instant) {
    thread::sleep(WAIT_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
}
