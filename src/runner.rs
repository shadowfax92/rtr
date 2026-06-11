//! `rtr run <tool>`: start the MITM proxy, launch the tool with proxy/CA env
//! scoped to that child only, and tear the proxy down when the child exits.
//!
//! Output handling: by default the child inherits the terminal (so TUIs like
//! `codex` work) and only request captures are persisted. With `--log` the
//! child's stdout/stderr are piped and tee'd to `output.log`.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{oneshot, Mutex};

use crate::capture::{self, CaptureSink};
use crate::config::{Config, Profile};
use crate::paths::Paths;
use crate::proxy::{self, RewriteHandler};
use crate::rewrite::Rewrites;
use crate::state::State;
use crate::{ca, keychain};

/// Environment injected into the child to scope interception to it alone.
///
/// Proxy vars route traffic; CA vars make OpenSSL/Node/Python/curl/git trust our
/// CA without touching the keychain. `NO_PROXY` is cleared so target hosts are
/// never excluded. Keychain-only verifiers (codex) still need `rtr trust`.
pub fn proxy_env(port: u16, ca_cert: &Path) -> Vec<(String, String)> {
    let proxy_url = format!("http://127.0.0.1:{port}");
    let ca = ca_cert.to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = Vec::new();
    for key in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
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

/// Resolve the active profile's rewrites, bailing if the active name is unknown.
fn resolve_rewrites(cfg: &Config, st: &State, tool_name: &str) -> Result<(Option<String>, Rewrites)> {
    let tool = cfg.tool(tool_name)?;
    let active = st.active_for(tool_name, cfg);
    let profile = match &active {
        Some(name) => tool
            .profiles
            .get(name)
            .cloned()
            .with_context(|| format!("active profile '{name}' not found for tool '{tool_name}'"))?,
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

/// Launch the tool with interception. Returns the child's exit code.
pub async fn run_tool(
    paths: &Paths,
    tool_name: &str,
    extra_args: &[String],
    show_secrets: bool,
    capture_output: bool,
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

    // Include the pid so two same-second runs (possible only with port = 0)
    // don't share a dir and truncate each other's output.log.
    let stamp = format!("{}-{}", capture::file_stamp(), std::process::id());
    let run_dir = paths.run_dir(tool_name, &stamp);
    crate::paths::create_private_dir_all(&run_dir)?;
    // Send proxy/hudsucker logs to a file so the child's terminal stays clean.
    let log_path = run_dir.join("rtr.log");
    crate::init_file_tracing(&log_path);
    let capture_path = run_dir.join("capture.jsonl");
    let sink = CaptureSink::to_file(&capture_path)?;
    let handler = RewriteHandler::new(
        tool.hosts.clone(),
        rewrites,
        sink,
        show_secrets,
        capture_output,
    );

    eprintln!(
        "rtr: proxy on 127.0.0.1:{port} intercepting {}",
        hosts_label(&tool.hosts)
    );
    eprintln!("rtr: profile = {}", active.as_deref().unwrap_or("(none)"));
    eprintln!("rtr: captures -> {}", capture_path.display());
    eprintln!("rtr: logs     -> {}", log_path.display());

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let proxy_task = tokio::spawn(proxy::serve(listener, authority, handler, async move {
        let _ = shutdown_rx.await;
    }));

    let mut command = Command::new(&tool.command[0]);
    command.args(&tool.command[1..]).args(extra_args);
    for (k, v) in proxy_env(port, &ca.cert_path) {
        command.env(k, v);
    }
    // If rtr's future is dropped (error/panic/cancellation), don't leave the
    // child running against a proxy that's about to die.
    command.kill_on_drop(true);

    let code = if capture_output {
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
        Ok(Err(e)) => tracing::warn!("proxy stopped with error: {e:#}"),
        Err(e) => tracing::warn!("proxy task did not join cleanly: {e}"),
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

    #[test]
    fn proxy_env_sets_proxy_and_ca_vars() {
        let env: HashMap<String, String> =
            proxy_env(62888, Path::new("/c/ca.pem")).into_iter().collect();
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

    #[tokio::test]
    async fn run_with_tee_writes_output_log_and_returns_code() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output.log");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hello-from-child; echo errline 1>&2; exit 7");
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
        assert!(out.contains("codex  (active: codex-1)"), "{out}");
        assert!(out.contains("api.openai.com"), "{out}");
        assert!(out.contains("codex-2"), "{out}");

        // Unknown filter errors; valid filter narrows.
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
        let err = run_tool(&paths, "codex", &[], false, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("rtr init"), "got: {err}");
    }
}
