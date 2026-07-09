//! `rtr run <tool>` starts the MITM proxy, launches the tool with proxy/CA env
//! scoped to that child, and tears the proxy down when the child exits.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{oneshot, Mutex};

use crate::config::{Config, Profile, Tool};
use crate::paths::Paths;
use crate::proxy::{self, RewriteHandler};
use crate::rewrite::Rewrites;
use crate::selection;
use crate::state::State;
use crate::{ca, keychain, tool_specs, usage};

/// Environment injected into the child to scope interception to it alone.
///
/// Proxy vars route traffic; CA vars make OpenSSL/Node/Python/curl/git trust our
/// CA without touching the keychain. `NO_PROXY` is cleared so target hosts are
/// never excluded. Keychain-only verifiers (codex) still need `rtr trust`.
pub fn proxy_env(port: u16, ca_cert: &Path) -> Vec<(String, String)> {
    let proxy_url = format!("http://127.0.0.1:{port}");
    let ca = ca_cert.to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = Vec::new();
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        env.push((key.to_string(), proxy_url.clone()));
    }
    for key in ["NO_PROXY", "no_proxy"] {
        env.push((key.to_string(), String::new()));
    }
    for key in [
        "SSL_CERT_FILE",
        "NODE_EXTRA_CA_CERTS",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
        "GIT_SSL_CAINFO",
    ] {
        env.push((key.to_string(), ca.clone()));
    }
    env
}

/// Map a finished child's status to an exit code, mirroring shells: a
/// signal-terminated child becomes `128 + signal` rather than a generic 1.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

/// Human label for a tool's intercept scope. The wildcard/omitted case reads as
/// `all hosts (*)` rather than an empty string or a literal `*` join.
fn hosts_label(hosts: &[String]) -> String {
    if crate::rewrite::matches_all_hosts(hosts) {
        "all hosts (*)".to_string()
    } else {
        hosts.join(", ")
    }
}

struct PreparedSubscriptionRun {
    profile_name: String,
    child_args: Vec<String>,
    child_env: Vec<(String, std::ffi::OsString)>,
    rewrites: Rewrites,
}

struct PreparedToolRun {
    tool_name: String,
    tool: Tool,
    hosts: Vec<String>,
    profile: Option<String>,
    rewrites: Rewrites,
    child_args: Vec<String>,
    child_env: Vec<(String, std::ffi::OsString)>,
    log_output: bool,
}

#[derive(Debug)]
struct SkillsSource {
    path: PathBuf,
    explicit: bool,
}

/// Resolve the active profile's rewrites, bailing if the active name is unknown.
fn resolve_rewrites(
    cfg: &Config,
    st: &State,
    tool_name: &str,
) -> Result<(Option<String>, Rewrites)> {
    let tool = cfg.tool(tool_name)?;
    let active = st.active_for(tool_name, cfg);
    let profile =
        match &active {
            Some(name) => tool.profiles.get(name).cloned().with_context(|| {
                format!("active profile '{name}' not found for tool '{tool_name}'")
            })?,
            None => Profile::default(),
        };
    let rewrites = Rewrites::from_profile(&profile).with_context(|| {
        format!(
            "profile '{}' for tool '{tool_name}' has an invalid header",
            active.as_deref().unwrap_or("<none>")
        )
    })?;
    Ok((active, rewrites))
}

/// Build the selected profile's native-home env and immutable args for one run.
fn prepare_subscription_run(
    paths: &Paths,
    spec: &tool_specs::ToolSpec,
    tool: &crate::config::Tool,
    profile_name: &str,
    runtime_args: &[String],
) -> Result<PreparedSubscriptionRun> {
    let profile = tool.profiles.get(profile_name).with_context(|| {
        format!(
            "profile '{profile_name}' disappeared for tool '{}'",
            spec.name
        )
    })?;
    if !profile.enabled {
        bail!("profile '{}/{}' is disabled", spec.name, profile_name);
    }
    let child_env = prepare_native_profile_env(paths, spec, tool, profile_name)?;
    Ok(PreparedSubscriptionRun {
        profile_name: profile_name.to_string(),
        child_args: runtime_args.to_vec(),
        child_env,
        // Native homes are the first-class identity boundary; header rewrites
        // remain legacy/custom `rtr run` behavior.
        rewrites: Rewrites::default(),
    })
}

/// Prepare the profile's native home and return the env passed to the child.
fn prepare_native_profile_env(
    paths: &Paths,
    spec: &tool_specs::ToolSpec,
    tool: &crate::config::Tool,
    profile_name: &str,
) -> Result<Vec<(String, std::ffi::OsString)>> {
    let home = prepare_native_profile_home(paths, spec, tool, profile_name)?;
    Ok(native_profile_env_for_home(spec, home))
}

fn prepare_native_profile_home(
    paths: &Paths,
    spec: &tool_specs::ToolSpec,
    tool: &crate::config::Tool,
    profile_name: &str,
) -> Result<PathBuf> {
    let home = paths.ensure_profile_home_dir(spec.name, profile_name)?;
    let user_home = crate::home_dir()?;
    sync_profile_skills(spec, tool, &home, &paths.config_dir, &user_home)?;
    Ok(home)
}

fn native_profile_env_for_home(
    spec: &tool_specs::ToolSpec,
    home: PathBuf,
) -> Vec<(String, std::ffi::OsString)> {
    vec![(spec.native_home_env.to_string(), home.into_os_string())]
}

/// Refresh the selected native profile home with the tool's skills source.
fn sync_profile_skills(
    spec: &tool_specs::ToolSpec,
    tool: &crate::config::Tool,
    profile_home: &Path,
    config_dir: &Path,
    home: &Path,
) -> Result<()> {
    crate::file_lock::with_exclusive_lock(&profile_home.join(".skills-sync.lock"), || {
        sync_profile_skills_locked(spec, tool, profile_home, config_dir, home)
    })
}

fn sync_profile_skills_locked(
    spec: &tool_specs::ToolSpec,
    tool: &crate::config::Tool,
    profile_home: &Path,
    config_dir: &Path,
    home: &Path,
) -> Result<()> {
    let source = skills_source(spec, tool, home, config_dir)?;
    let destination = profile_home.join("skills");

    match std::fs::metadata(&source.path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            bail!(
                "skills source {} must be a directory",
                source.path.display()
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !source.explicit => {
            remove_existing_path(&destination)?;
            return Ok(());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "configured skills source {} does not exist",
                source.path.display()
            );
        }
        Err(err) => return Err(err).with_context(|| format!("stat {}", source.path.display())),
    }

    ensure_distinct_copy_paths(&source.path, &destination)?;
    replace_skills_dir(&source.path, &destination)
}

fn skills_source(
    spec: &tool_specs::ToolSpec,
    tool: &crate::config::Tool,
    home: &Path,
    config_dir: &Path,
) -> Result<SkillsSource> {
    if let Some(path) = &tool.skills_source {
        let path = expand_home_path(path, home)?;
        let path = if path.is_relative() {
            config_dir.join(path)
        } else {
            path
        };
        return Ok(SkillsSource {
            path,
            explicit: true,
        });
    }

    let mut path = home.to_path_buf();
    for segment in spec.default_skills_source {
        path.push(segment);
    }
    Ok(SkillsSource {
        path,
        explicit: false,
    })
}

fn expand_home_path(path: &Path, home: &Path) -> Result<PathBuf> {
    let raw = path.as_os_str().to_string_lossy();
    if raw == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home.join(rest));
    }
    if raw.starts_with('~') {
        bail!("configured paths support only '~' or '~/' home expansion: {raw}");
    }
    Ok(path.to_path_buf())
}

fn ensure_distinct_copy_paths(source: &Path, destination: &Path) -> Result<()> {
    let source = source
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", source.display()))?;
    let destination_parent = destination
        .parent()
        .with_context(|| format!("{} has no parent", destination.display()))?
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", destination.display()))?;
    if destination_parent.starts_with(&source) {
        bail!(
            "skills source {} must not contain destination {}",
            source.display(),
            destination.display()
        );
    }

    if let Ok(destination) = destination.canonicalize() {
        if source == destination || source.starts_with(&destination) {
            bail!(
                "skills source {} must not be inside destination {}",
                source.display(),
                destination.display()
            );
        }
    }
    Ok(())
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };

    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    } else {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
    }
}

fn replace_skills_dir(source: &Path, destination: &Path) -> Result<()> {
    let temp = temporary_skills_path(destination);
    remove_existing_path(&temp)?;

    let result = (|| -> Result<()> {
        crate::paths::create_private_dir_all(&temp)?;
        copy_dir_contents(source, &temp)?;
        remove_existing_path(destination)?;
        std::fs::rename(&temp, destination)
            .with_context(|| format!("renaming {} to {}", temp.display(), destination.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = remove_existing_path(&temp);
    }
    result
}

fn temporary_skills_path(destination: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    destination.with_file_name(format!(".skills.{}-{}.tmp", std::process::id(), stamp))
}

fn copy_dir_contents(source: &Path, destination: &Path) -> Result<()> {
    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", source.display()))?;
        copy_path(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(source).with_context(|| format!("stat {}", source.display()))?;
    if meta.file_type().is_symlink() {
        let target =
            std::fs::read_link(source).with_context(|| format!("readlink {}", source.display()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, destination).with_context(|| {
            format!("symlink {} -> {}", destination.display(), target.display())
        })?;
        #[cfg(not(unix))]
        bail!(
            "copying symlinked skills is not supported on this platform: {}",
            source.display()
        );
        return Ok(());
    }
    if meta.is_dir() {
        crate::paths::create_private_dir_all(destination)?;
        return copy_dir_contents(source, destination);
    }
    if meta.is_file() {
        std::fs::copy(source, destination).with_context(|| {
            format!("copying {} to {}", source.display(), destination.display())
        })?;
        return Ok(());
    }
    bail!("unsupported skills entry type: {}", source.display());
}

/// Launch the tool with interception. Returns the child's exit code.
pub async fn run_tool(
    paths: &Paths,
    tool_name: &str,
    extra_args: &[String],
    log_output: bool,
) -> Result<i32> {
    let cfg_path = paths.config_file();
    if !cfg_path.exists() {
        bail!("no config at {} — run `rtr init` first", cfg_path.display());
    }
    let cfg = Config::load(&cfg_path)?;
    let tool = cfg.tool(tool_name)?.clone();
    if tool.command.is_empty() {
        bail!("tool '{tool_name}' has an empty command");
    }
    let st = State::load(&paths.state_file())?;
    let (active, rewrites) = resolve_rewrites(&cfg, &st, tool_name)?;

    let hosts = tool.hosts.clone();
    run_prepared_tool(
        paths,
        &cfg,
        PreparedToolRun {
            tool_name: tool_name.to_string(),
            tool,
            hosts,
            profile: active,
            rewrites,
            child_args: extra_args.to_vec(),
            child_env: Vec::new(),
            log_output,
        },
    )
    .await
}

/// Launch a first-class subscription tool, selecting a profile for this run and recording usage.
pub async fn run_subscription_tool(
    paths: &Paths,
    tool_name: &str,
    forced_profile: Option<&str>,
    runtime_args: &[String],
    log_output: bool,
) -> Result<i32> {
    let spec = tool_specs::get(tool_name)?;
    let cfg_path = paths.config_file();
    if !cfg_path.exists() {
        bail!("no config at {} — run `rtr init` first", cfg_path.display());
    }
    let cfg = Config::load(&cfg_path)?;
    let tool = cfg.tool(spec.name)?.clone();
    if tool.command.is_empty() {
        bail!("tool '{}' has an empty command", spec.name);
    }

    let state_path = paths.state_file();
    let prepared = if let Some(profile_name) = forced_profile {
        prepare_subscription_run(paths, spec, &tool, profile_name, runtime_args)?
    } else {
        State::update_locked(&state_path, |state| {
            let profile_name = selection::select_profile(spec.name, &tool, state, None)?;
            prepare_subscription_run(paths, spec, &tool, &profile_name, runtime_args)
        })?
    };

    let result = run_prepared_tool(
        paths,
        &cfg,
        PreparedToolRun {
            tool_name: spec.name.to_string(),
            tool,
            hosts: tool_specs::runtime_hosts(spec),
            profile: Some(prepared.profile_name.clone()),
            rewrites: prepared.rewrites,
            child_args: prepared.child_args,
            child_env: prepared.child_env,
            log_output,
        },
    )
    .await;

    let exit_code = result.as_ref().ok().copied();
    usage::append_event(
        &paths.usage_file(),
        &usage::new_event(spec.name, &prepared.profile_name, exit_code),
    )?;
    result
}

/// Launch one fully resolved tool run through the scoped proxy.
async fn run_prepared_tool(paths: &Paths, cfg: &Config, prepared: PreparedToolRun) -> Result<i32> {
    let PreparedToolRun {
        tool_name,
        tool,
        hosts,
        profile,
        rewrites,
        child_args,
        child_env,
        log_output,
    } = prepared;
    let ca = ca::load_or_generate(&paths.ca_cert(), &paths.ca_key())?;
    let authority = ca.authority()?;

    if !keychain::is_trusted(&ca.cert_path) {
        eprintln!("rtr: CA is not trusted in your keychain yet.");
        eprintln!("     Keychain-verifying tools (e.g. codex) need a one-time: rtr trust");
    }

    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.proxy.port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding proxy on {addr} (another rtr already running?)"))?;
    let port = listener.local_addr()?.port();

    let run_dir = if log_output {
        let stamp = format!(
            "{}-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            std::process::id()
        );
        let run_dir = paths.run_dir(&tool_name, &stamp);
        crate::paths::create_private_dir_all(&run_dir)?;
        let log_path = run_dir.join("rtr.log");
        crate::init_file_tracing(&log_path);
        eprintln!("rtr: output -> {}", run_dir.join("output.log").display());
        eprintln!("rtr: logs   -> {}", log_path.display());
        Some(run_dir)
    } else {
        None
    };
    let handler = RewriteHandler::new(hosts.clone(), rewrites);

    eprintln!(
        "rtr: proxy on 127.0.0.1:{port} intercepting {}",
        hosts_label(&hosts)
    );
    eprintln!("rtr: profile = {}", profile.as_deref().unwrap_or("(none)"));

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let proxy_task = tokio::spawn(proxy::serve(listener, authority, handler, async move {
        let _ = shutdown_rx.await;
    }));

    let mut command = Command::new(&tool.command[0]);
    command.args(&tool.command[1..]).args(&child_args);
    for (k, v) in proxy_env(port, &ca.cert_path) {
        command.env(k, v);
    }
    for (k, v) in child_env {
        command.env(k, v);
    }
    // If rtr's future is dropped (error/panic/cancellation), don't leave the
    // child running against a proxy that's about to die.
    command.kill_on_drop(true);

    let code = if let Some(run_dir) = run_dir {
        run_with_tee(&mut command, &run_dir.join("output.log")).await?
    } else {
        let status = command
            .status()
            .await
            .with_context(|| format!("spawning '{}'", tool.command[0]))?;
        exit_code(status)
    };

    let _ = shutdown_tx.send(());
    match proxy_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("rtr: proxy stopped with error: {e:#}"),
        Err(e) => eprintln!("rtr: proxy task did not join cleanly: {e}"),
    }
    Ok(code)
}

/// Spawn the child with piped stdio, tee'ing both streams to the terminal and a
/// shared `output.log`.
async fn run_with_tee(command: &mut Command, output_path: &Path) -> Result<i32> {
    use std::os::unix::fs::OpenOptionsExt;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().context("spawning child")?;

    // 0600: the transcript may contain secrets the tool prints.
    let std_log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(output_path)
        .with_context(|| format!("creating {}", output_path.display()))?;
    let log = Arc::new(Mutex::new(tokio::fs::File::from_std(std_log)));

    let stdout = child.stdout.take().context("child stdout missing")?;
    let stderr = child.stderr.take().context("child stderr missing")?;
    let mut t_out = tokio::spawn(tee(stdout, true, log.clone()));
    let mut t_err = tokio::spawn(tee(stderr, false, log.clone()));

    let status = child.wait().await.context("waiting for child")?;

    // Drain remaining buffered output, but don't hang forever if a grandchild
    // inherited the pipe and outlives the child (EOF would never arrive).
    let drained = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = tokio::join!(&mut t_out, &mut t_err);
    })
    .await;
    if drained.is_err() {
        t_out.abort();
        t_err.abort();
        tracing::warn!("child exited but its output pipe stayed open; stopped tee after 2s");
    }

    Ok(exit_code(status))
}

/// `rtr status`: gather config/state/CA/trust and print a human summary.
pub fn print_status(paths: &Paths, tool_filter: Option<&str>) -> Result<()> {
    let cfg = Config::load(&paths.config_file())?;
    let st = State::load(&paths.state_file())?;
    let ca = ca::load_or_generate(&paths.ca_cert(), &paths.ca_key())?;
    let fingerprint = ca.fingerprint()?;
    let trusted = keychain::is_trusted(&ca.cert_path);
    let out = render_status(
        &cfg,
        &st,
        &ca.cert_path.display().to_string(),
        &fingerprint,
        trusted,
        tool_filter,
    )?;
    print!("{out}");
    Ok(())
}

/// Pure renderer for `status`, kept separate so it can be asserted on.
pub fn render_status(
    cfg: &Config,
    st: &State,
    ca_path: &str,
    fingerprint: &str,
    trusted: bool,
    tool_filter: Option<&str>,
) -> Result<String> {
    use std::fmt::Write as _;

    if let Some(name) = tool_filter {
        if !cfg.tools.contains_key(name) {
            bail!("no tool named '{name}' in config.toml");
        }
    }

    let mut s = String::new();
    let _ = writeln!(s, "rtr status");
    let _ = writeln!(s, "  proxy:          127.0.0.1:{}", cfg.proxy.port);
    let _ = writeln!(s, "  CA cert:        {ca_path}");
    let _ = writeln!(s, "  CA fingerprint: {fingerprint}");
    let _ = writeln!(
        s,
        "  keychain trust: {}",
        if trusted {
            "trusted"
        } else {
            "NOT trusted — run `rtr trust`"
        }
    );
    let _ = writeln!(s, "\ntools:");
    for (name, tool) in &cfg.tools {
        if tool_filter.is_some_and(|f| f != name) {
            continue;
        }
        let active = st.active_for(name, cfg).unwrap_or_else(|| "(none)".into());
        let profiles: Vec<&str> = tool.profiles.keys().map(String::as_str).collect();
        let _ = writeln!(s, "  {name}  (active: {active})");
        let _ = writeln!(s, "    command:  {}", tool.command.join(" "));
        let _ = writeln!(s, "    hosts:    {}", hosts_label(&tool.hosts));
        let _ = writeln!(s, "    profiles: {}", profiles.join(", "));
    }
    Ok(s)
}

async fn tee<R>(mut reader: R, to_stdout: bool, log: Arc<Mutex<tokio::fs::File>>) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        {
            let mut f = log.lock().await;
            f.write_all(chunk).await?;
            f.flush().await?;
        }
        if to_stdout {
            let mut w = io::stdout();
            w.write_all(chunk).await?;
            w.flush().await?;
        } else {
            let mut w = io::stderr();
            w.write_all(chunk).await?;
            w.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn toml_path(path: &Path) -> String {
        toml::Value::String(path.display().to_string()).to_string()
    }

    #[test]
    fn proxy_env_sets_proxy_and_ca_vars() {
        let env: HashMap<String, String> = proxy_env(62888, Path::new("/c/ca.pem"))
            .into_iter()
            .collect();
        assert_eq!(env["HTTPS_PROXY"], "http://127.0.0.1:62888");
        assert_eq!(env["https_proxy"], "http://127.0.0.1:62888");
        assert_eq!(env["ALL_PROXY"], "http://127.0.0.1:62888");
        assert_eq!(env["NO_PROXY"], "");
        assert_eq!(env["SSL_CERT_FILE"], "/c/ca.pem");
        assert_eq!(env["NODE_EXTRA_CA_CERTS"], "/c/ca.pem");
        assert_eq!(env["CURL_CA_BUNDLE"], "/c/ca.pem");
        assert_eq!(env["GIT_SSL_CAINFO"], "/c/ca.pem");
    }

    #[test]
    fn resolve_rewrites_errors_on_unknown_active_profile() {
        let cfg = Config::parse(
            "[tools.t]\ncommand=[\"t\"]\nactive=\"ghost\"\n[tools.t.profiles.real]\nset={}\n",
        )
        .unwrap();
        let st = State::default();
        let err = resolve_rewrites(&cfg, &st, "t").unwrap_err().to_string();
        assert!(err.contains("ghost"), "got: {err}");
    }

    #[test]
    fn skills_source_defaults_to_tool_home_skills() {
        let cfg = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();
        let tool = cfg.tool("codex").unwrap();
        let spec = tool_specs::get("codex").unwrap();
        let source =
            skills_source(spec, tool, Path::new("/home/me"), Path::new("/config")).unwrap();
        assert!(!source.explicit);
        assert_eq!(source.path, PathBuf::from("/home/me/.codex/skills"));

        let cfg = Config::parse("[tools.claude]\ncommand=[\"claude\"]\n").unwrap();
        let tool = cfg.tool("claude").unwrap();
        let spec = tool_specs::get("claude").unwrap();
        let source =
            skills_source(spec, tool, Path::new("/home/me"), Path::new("/config")).unwrap();
        assert!(!source.explicit);
        assert_eq!(source.path, PathBuf::from("/home/me/.claude/skills"));
    }

    #[test]
    fn skills_source_expands_configured_home_path() {
        let cfg =
            Config::parse("[tools.codex]\ncommand=[\"codex\"]\nskills_source=\"~/.skills\"\n")
                .unwrap();
        let source = skills_source(
            tool_specs::get("codex").unwrap(),
            cfg.tool("codex").unwrap(),
            Path::new("/home/me"),
            Path::new("/config"),
        )
        .unwrap();
        assert!(source.explicit);
        assert_eq!(source.path, PathBuf::from("/home/me/.skills"));
    }

    #[test]
    fn skills_source_resolves_relative_configured_path_from_config_dir() {
        let cfg = Config::parse("[tools.codex]\ncommand=[\"codex\"]\nskills_source=\"skills\"\n")
            .unwrap();
        let source = skills_source(
            tool_specs::get("codex").unwrap(),
            cfg.tool("codex").unwrap(),
            Path::new("/home/me"),
            Path::new("/config/rtr"),
        )
        .unwrap();
        assert!(source.explicit);
        assert_eq!(source.path, PathBuf::from("/config/rtr/skills"));
    }

    #[test]
    fn skills_source_rejects_user_home_expansion() {
        let cfg = Config::parse(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source=\"~someone/skills\"\n",
        )
        .unwrap();
        let err = skills_source(
            tool_specs::get("codex").unwrap(),
            cfg.tool("codex").unwrap(),
            Path::new("/home/me"),
            Path::new("/config"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("~"), "got: {err}");
    }

    #[test]
    fn sync_profile_skills_overwrites_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::create_dir_all(profile_home.join("skills")).unwrap();
        std::fs::write(source.join("root.md"), "root").unwrap();
        std::fs::write(source.join("nested").join("child.md"), "child").unwrap();
        std::fs::write(profile_home.join("skills").join("stale.md"), "stale").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("root.md", source.join("root-link")).unwrap();

        let cfg = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        sync_profile_skills(
            tool_specs::get("codex").unwrap(),
            cfg.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            Path::new("/home/me"),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills").join("root.md")).unwrap(),
            "root"
        );
        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/nested/child.md")).unwrap(),
            "child"
        );
        assert!(!profile_home.join("skills/stale.md").exists());
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(profile_home.join("skills/root-link")).unwrap(),
            PathBuf::from("root.md")
        );
    }

    #[cfg(unix)]
    #[test]
    fn sync_profile_skills_keeps_destination_when_copy_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(profile_home.join("skills")).unwrap();
        std::fs::write(profile_home.join("skills").join("stale.md"), "stale").unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(source.join("unsupported"))
            .status()
            .unwrap()
            .success());

        let cfg = Config::parse(&format!(
            "[tools.codex]\ncommand=[\"codex\"]\nskills_source={}\n",
            toml_path(&source)
        ))
        .unwrap();
        let err = sync_profile_skills(
            tool_specs::get("codex").unwrap(),
            cfg.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            Path::new("/home/me"),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("unsupported skills entry"), "got: {err}");
        assert_eq!(
            std::fs::read_to_string(profile_home.join("skills/stale.md")).unwrap(),
            "stale"
        );
    }

    #[test]
    fn sync_profile_skills_removes_destination_when_default_source_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let profile_home = dir.path().join("profile");
        std::fs::create_dir_all(profile_home.join("skills")).unwrap();
        std::fs::write(profile_home.join("skills").join("stale.md"), "stale").unwrap();

        let cfg = Config::parse("[tools.codex]\ncommand=[\"codex\"]\n").unwrap();
        sync_profile_skills(
            tool_specs::get("codex").unwrap(),
            cfg.tool("codex").unwrap(),
            &profile_home,
            dir.path(),
            &home,
        )
        .unwrap();

        assert!(!profile_home.join("skills").exists());
    }

    #[test]
    fn prepared_subscription_rewrites_leave_runtime_host_headers_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("empty-skills");
        std::fs::create_dir_all(&source).unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            state_dir: dir.path().join("state"),
        };
        let tool = Config::parse(&format!(
            r#"
[tools.codex]
command = ["codex"]
skills_source = {}

[tools.codex.profiles.personal]
set = {{ Authorization = "Bearer stale", chatgpt-account-id = "stale-account" }}
"#,
            toml_path(&source)
        ))
        .unwrap()
        .tool("codex")
        .unwrap()
        .clone();
        let spec = tool_specs::get("codex").unwrap();
        let prepared = prepare_subscription_run(&paths, spec, &tool, "personal", &[]).unwrap();
        assert!(prepared.rewrites.is_empty());

        let handler = RewriteHandler::new(tool_specs::runtime_hosts(spec), prepared.rewrites);
        let req = hudsucker::hyper::Request::builder()
            .method("POST")
            .uri("https://chatgpt.com/backend-api/codex/session")
            .header("authorization", "Bearer child")
            .header("chatgpt-account-id", "child-account")
            .body(hudsucker::Body::empty())
            .unwrap();

        let req = handler.apply(req);
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer child");
        assert_eq!(
            req.headers().get("chatgpt-account-id").unwrap(),
            "child-account"
        );
    }

    #[tokio::test]
    async fn run_with_tee_writes_output_log_and_returns_code() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output.log");
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo hello-from-child; echo errline 1>&2; exit 7");
        let code = run_with_tee(&mut cmd, &out).await.unwrap();
        assert_eq!(code, 7);
        let log = std::fs::read_to_string(&out).unwrap();
        assert!(log.contains("hello-from-child"), "log: {log}");
        assert!(log.contains("errline"), "log: {log}");

        // output.log holds tool output and must not be world-readable.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "output.log perms {mode:o}");
    }

    #[tokio::test]
    async fn signal_terminated_child_reports_128_plus_signal() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output.log");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("kill -TERM $$");
        let code = run_with_tee(&mut cmd, &out).await.unwrap();
        assert_eq!(code, 128 + 15, "SIGTERM should map to 143, got {code}");
    }

    #[test]
    fn render_status_shows_tools_and_trust() {
        let cfg = Config::parse(crate::config::STARTER_CONFIG).unwrap();
        let st = State::default();
        let out = render_status(&cfg, &st, "/c/ca.pem", "AA:BB", false, None).unwrap();
        assert!(out.contains("127.0.0.1:62888"), "{out}");
        assert!(out.contains("AA:BB"), "{out}");
        assert!(out.contains("NOT trusted"), "{out}");
        assert!(out.contains("claude  (active: (none))"), "{out}");
        assert!(out.contains(".anthropic.com"), "{out}");
        assert!(out.contains("codex  (active: (none))"), "{out}");
        assert!(out.contains("chatgpt.com"), "{out}");
        assert!(out.contains("profiles:"), "{out}");

        assert!(render_status(&cfg, &st, "/c/ca.pem", "AA", true, Some("ghost")).is_err());
        let only = render_status(&cfg, &st, "/c/ca.pem", "AA", true, Some("codex")).unwrap();
        assert!(only.contains("trusted"), "{only}");
    }

    #[test]
    fn render_status_labels_wildcard_and_omitted_hosts_as_all() {
        let cfg = Config::parse(
            "[tools.star]\ncommand=[\"s\"]\nhosts=[\"*\"]\n[tools.bare]\ncommand=[\"b\"]\n",
        )
        .unwrap();
        let st = State::default();
        // Explicit "*" and an omitted hosts list both read as "all hosts (*)".
        let star = render_status(&cfg, &st, "/c/ca.pem", "AA", false, Some("star")).unwrap();
        assert!(star.contains("all hosts (*)"), "{star}");
        let bare = render_status(&cfg, &st, "/c/ca.pem", "AA", false, Some("bare")).unwrap();
        assert!(bare.contains("all hosts (*)"), "{bare}");
    }

    #[tokio::test]
    async fn run_tool_requires_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            state_dir: dir.path().join("state"),
        };
        let err = run_tool(&paths, "codex", &[], false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("rtr init"), "got: {err}");
    }
}
