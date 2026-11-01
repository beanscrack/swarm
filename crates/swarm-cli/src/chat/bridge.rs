//! `/bridge [agents]`: start the ChatGPT web bridge from inside the session.
//!
//! What the user used to do in a second terminal (`start-pool.ps1`, then
//! `node bridge.mjs --cdp ...`) happens here, the same way on Windows and
//! Linux (and macOS): open one plain Chrome per three agents, each with its own
//! profile and DevTools port, start the Node bridge attached to them, and ask
//! the bridge whether each browser is signed in. A browser that is not signed
//! in is already on chatgpt.com, so signing in is the only thing left to do.
//!
//! The Chrome windows are detached on purpose: they keep their login and the
//! next `/bridge` finds them already running. The bridge process belongs to the
//! session and ends with it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tokio::process::{Child, Command};

use super::Chat;
use crate::commands::apply_provider_overrides;

/// Chats one signed-in browser holds comfortably (the bridge allows 8).
const AGENTS_PER_BROWSER: u32 = 3;
const MAX_BROWSERS: u32 = 8;
/// First DevTools port; clear of `start-chrome.ps1`'s 9222.
const BASE_CDP_PORT: u16 = 9230;
const DEFAULT_BRIDGE_PORT: u16 = 8766;
const PROVIDER: &str = "chatgpt-web";

/// The bridge process this session started.
pub(crate) struct Bridge {
    child: Child,
    port: u16,
    log: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
enum Verb {
    Start(Option<u32>),
    Status,
    Stop,
}

/// `3`, `--3`, `--agents 3`, `--agents=3`, `-n 3`, `status`, `stop`.
fn parse(args: &str) -> Result<Verb> {
    let mut count = None;
    for token in args.split_whitespace() {
        let word = token.trim_start_matches('-').to_ascii_lowercase();
        match word.as_str() {
            "status" => return Ok(Verb::Status),
            "stop" | "off" => return Ok(Verb::Stop),
            "agents" | "agent" | "n" => continue,
            _ => {}
        }
        let number = word.strip_prefix("agents=").or_else(|| word.strip_prefix("n=")).unwrap_or(&word);
        match number.parse::<u32>() {
            Ok(n) if n >= 1 => count = Some(n),
            _ => bail!("usage: /bridge [agents]   /bridge status   /bridge stop"),
        }
    }
    Ok(Verb::Start(count))
}

/// One browser per three agents, at least one.
fn browsers_for(agents: u32) -> usize {
    agents.div_ceil(AGENTS_PER_BROWSER).clamp(1, MAX_BROWSERS) as usize
}

/// The `bridges/chatgpt-web` directory: `$SWARM_BRIDGE_DIR`, then beside the
/// running binary (`target/debug/swarm` inside a checkout), then the workspace,
/// then where this crate was built.
fn find_dir(workspace: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("SWARM_BRIDGE_DIR") {
        candidates.push(dir.into());
    }
    if let Ok(exe) = std::env::current_exe() {
        candidates.extend(exe.ancestors().skip(1).take(6).map(|up| up.join("bridges").join("chatgpt-web")));
    }
    candidates.extend(workspace.ancestors().take(4).map(|up| up.join("bridges").join("chatgpt-web")));
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bridges/chatgpt-web"));
    candidates.into_iter().find(|dir| dir.join("bridge.mjs").is_file()).map(|dir| dir.canonicalize().unwrap_or(dir))
}

/// An executable on `PATH`, trying each name (and `.exe` on Windows).
fn in_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let suffixes: &[&str] = if cfg!(windows) { &[".exe", ".cmd"] } else { &[""] };
    for dir in std::env::split_paths(&path) {
        for name in names {
            for suffix in suffixes {
                let candidate = dir.join(format!("{name}{suffix}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// A Chrome (or Chromium) to open: `$SWARM_CHROME`, then the usual install
/// locations for this OS, then `PATH`.
fn find_chrome() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["SWARM_CHROME", "CHROME_PATH"] {
        if let Some(path) = std::env::var_os(var) {
            candidates.push(path.into());
        }
    }
    if cfg!(windows) {
        for var in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(var) {
                candidates.push(Path::new(&root).join(r"Google\Chrome\Application\chrome.exe"));
            }
        }
    } else if cfg!(target_os = "macos") {
        candidates.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        candidates.push("/Applications/Chromium.app/Contents/MacOS/Chromium".into());
    } else {
        candidates.extend(["/opt/google/chrome/chrome", "/usr/bin/google-chrome", "/snap/bin/chromium"].map(PathBuf::from));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .or_else(|| in_path(&["google-chrome", "google-chrome-stable", "chrome", "chromium", "chromium-browser"]))
}

/// A browser window needs a display; over SSH on Linux there is none.
fn has_display() -> bool {
    !cfg!(all(unix, not(target_os = "macos")))
        || std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

/// A blocking GET to a loopback port; `(status, body)`.
fn http_get_blocking(port: u16, path: &str, timeout: Duration) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    // One write: a formatted `write!` goes out in pieces, and a server that
    // answers after its first read would then close on unread bytes (a reset).
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    // A reset or timeout after the response arrived must not lose it.
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    while let Ok(n) = stream.read(&mut buffer) {
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..n]);
    }
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
    Some((status, if chunked { dechunk(body) } else { body.to_string() }))
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, after)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else { break };
        if size == 0 || after.len() < size {
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].trim_start_matches("\r\n");
    }
    out
}

async fn get(port: u16, path: &'static str, seconds: u64) -> Option<(u16, String)> {
    tokio::task::spawn_blocking(move || http_get_blocking(port, path, Duration::from_secs(seconds))).await.ok().flatten()
}

async fn get_json(port: u16, path: &'static str, seconds: u64) -> Option<serde_json::Value> {
    let (status, body) = get(port, path, seconds).await?;
    (status == 200).then(|| serde_json::from_str(&body).ok()).flatten()
}

/// Poll until `done` returns a value or `seconds` pass.
async fn until<T, F, Fut>(seconds: u64, mut done: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = done().await {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `signed_in` of each access point: `Some(true)`, `Some(false)` or unknown.
async fn login_states(port: u16) -> Option<Vec<Option<bool>>> {
    let value = get_json(port, "/login", 20).await?;
    Some(value["access_points"].as_array()?.iter().map(|point| point["signed_in"].as_bool()).collect())
}

fn port_of(base_url: Option<&str>) -> Result<u16> {
    let Some(url) = base_url else { return Ok(DEFAULT_BRIDGE_PORT) };
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = authority.rsplit_once(':').unwrap_or((authority, ""));
    if !matches!(host, "127.0.0.1" | "localhost") {
        bail!("{PROVIDER} points at {url}, which is not on this machine; /bridge only starts a local bridge");
    }
    Ok(port.parse().unwrap_or(DEFAULT_BRIDGE_PORT))
}

fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn spawn_chrome(chrome: &Path, port: u16, profile: &Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new(chrome);
    command
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "https://chatgpt.com/"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group: Ctrl-C or closing this terminal must not close the
    // browsers (and their sign-in) with it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().map(drop)
}

impl Chat {
    /// `/bridge [agents|status|stop]`.
    pub(super) async fn cmd_bridge(&mut self, args: &str) {
        let result = match parse(args) {
            Ok(Verb::Start(count)) => self.bridge_start(count.unwrap_or(self.cfg.agents)).await,
            Ok(Verb::Status) => self.bridge_status().await,
            Ok(Verb::Stop) => self.bridge_stop().await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            self.say(format!("bridge: {error}"));
        }
    }

    fn bridge_port(&self) -> Result<u16> {
        let base = self.cfg.providers.iter().find(|p| p.name.as_str() == PROVIDER).and_then(|p| p.base_url.clone());
        port_of(base.as_deref())
    }

    async fn bridge_start(&mut self, agents: u32) -> Result<()> {
        let browsers = browsers_for(agents);
        let port = self.bridge_port()?;
        let dir = find_dir(&self.workspace)
            .ok_or_else(|| anyhow!("bridges/chatgpt-web was not found; set SWARM_BRIDGE_DIR to that directory"))?;

        // A bridge that is already up: ours with the right size is reused, ours
        // with the wrong size is restarted, anyone else's is left alone.
        if let Some(health) = get_json(port, "/health", 3).await {
            let have = health["access_points"].as_u64().unwrap_or(0) as usize;
            let ours = self.bridge.is_some();
            if !ours {
                self.say(format!(
                    "a bridge is already answering on port {port} ({have} browser(s)); using it as it is. Stop that process to let /bridge manage it"
                ));
                return self.finish_start(agents, have.max(1)).await;
            }
            if have == browsers {
                self.say(format!("the bridge is already running with {have} browser(s)"));
                return self.finish_start(agents, browsers).await;
            }
            self.say(format!("restarting the bridge: {have} browser(s) \u{2192} {browsers}"));
            self.bridge_stop_quietly().await;
        }

        let node = in_path(&["node", "nodejs"]).ok_or_else(|| anyhow!("Node.js was not found on PATH (the bridge is a Node program)"))?;
        self.install_bridge_dependencies(&dir).await?;

        let chrome = find_chrome().ok_or_else(|| anyhow!("Chrome was not found; install it or set SWARM_CHROME to its path"))?;
        if !has_display() {
            bail!("no display (DISPLAY / WAYLAND_DISPLAY) to open a browser window on; run swarm in a desktop session, or start the bridge by hand");
        }

        // One Chrome per access point. Profiles sit beside the bridge so the
        // sign-ins from `start-pool.ps1` are reused.
        let root = dir.join(".chrome-pool");
        let mut endpoints = Vec::new();
        let mut opened = 0;
        for index in 0..browsers {
            let cdp = BASE_CDP_PORT + index as u16;
            endpoints.push(format!("http://127.0.0.1:{cdp}"));
            if get(cdp, "/json/version", 2).await.is_some_and(|(status, _)| status == 200) {
                continue;
            }
            let profile = root.join(format!("browser-{}", index + 1));
            std::fs::create_dir_all(&profile)?;
            spawn_chrome(&chrome, cdp, &profile).map_err(|error| anyhow!("could not start {}: {error}", chrome.display()))?;
            opened += 1;
        }
        self.say(format!("opening {opened} browser window(s), {} already open", browsers - opened));
        for index in 0..browsers {
            let cdp = BASE_CDP_PORT + index as u16;
            until(30, || async move { get(cdp, "/json/version", 2).await.filter(|(status, _)| *status == 200) })
                .await
                .ok_or_else(|| anyhow!("Chrome on debugging port {cdp} did not come up; close any Chrome using {} and retry", root.join(format!("browser-{}", index + 1)).display()))?;
            // Let the landing tab get its address, so the bridge adopts it
            // instead of opening a second one.
            until(8, || async move {
                get(cdp, "/json/list", 2).await.filter(|(_, body)| body.contains("chatgpt.com"))
            })
            .await;
        }

        let log = std::env::temp_dir().join("swarm-chatgpt-bridge.log");
        let log_file = std::fs::File::create(&log)?;
        let mut command = Command::new(node);
        command
            .current_dir(&dir)
            .args(["bridge.mjs", "--no-api", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file))
            .kill_on_drop(true);
        for endpoint in &endpoints {
            command.args(["--cdp", endpoint]);
        }
        #[cfg(unix)]
        command.process_group(0);
        // No console window flashing up behind the session on Windows.
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let mut child = command.spawn().map_err(|error| anyhow!("could not start node: {error}"))?;

        let up = until(45, || async {
            get_json(port, "/health", 2).await
        })
        .await;
        if up.is_none() {
            let exited = child.try_wait().ok().flatten();
            let _ = child.kill().await;
            bail!(
                "the bridge did not come up{}\n{}\n(full log: {})",
                exited.map(|status| format!(" (it exited with {status})")).unwrap_or_default(),
                tail(&log, 6),
                log.display()
            );
        }
        self.bridge = Some(Bridge { child, port, log });
        self.finish_start(agents, browsers).await
    }

    /// `npm ci` the first time, so a fresh checkout needs nothing else.
    async fn install_bridge_dependencies(&self, dir: &Path) -> Result<()> {
        if dir.join("node_modules").join("playwright-core").is_dir() {
            return Ok(());
        }
        let npm = in_path(&["npm"]).ok_or_else(|| anyhow!("npm was not found on PATH (needed once to install the bridge's dependencies)"))?;
        self.say("installing the bridge's dependencies (npm ci, first run only)...");
        let output = tokio::time::timeout(
            Duration::from_secs(300),
            Command::new(npm)
                .args(["ci", "--no-audit", "--no-fund"])
                .current_dir(dir)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| anyhow!("npm ci took more than five minutes"))??;
        if !output.status.success() {
            let text = String::from_utf8_lossy(&output.stderr);
            let lines: Vec<&str> = text.lines().collect();
            bail!("npm ci failed:\n{}", lines[lines.len().saturating_sub(6)..].join("\n"));
        }
        Ok(())
    }

    /// The bridge is up: point the session at it and say what is left to do.
    async fn finish_start(&mut self, agents: u32, browsers: usize) -> Result<()> {
        if self.cfg.provider.as_str() != PROVIDER {
            apply_provider_overrides(&mut self.cfg, Some(PROVIDER), None, None, None, &self.workspace, false)?;
            self.say(format!("provider \u{2192} {PROVIDER}"));
        }
        self.cfg.agents = agents;
        self.cfg.budget.max_inference_calls = self.cfg.budget.max_inference_calls.max(u64::from(agents) * 40);
        if let Some(provider) = self.cfg.providers.iter_mut().find(|p| p.name.as_str() == PROVIDER) {
            provider.max_concurrency = Some(agents);
        }
        self.push_info();

        let port = self.bridge_port()?;
        // Pages are still loading right after a start; give them a moment to
        // show whether they are signed in.
        let settled = until(10, || async {
            let states = login_states(port).await?;
            states.iter().all(Option::is_some).then_some(states)
        })
        .await;
        let states = match settled {
            Some(states) => states,
            None => login_states(port).await.unwrap_or_default(),
        };
        let signed_out = states.iter().filter(|state| **state == Some(false)).count();
        let unknown = states.iter().filter(|state| state.is_none()).count();
        let mut rows = vec![format!(
            "{PROVIDER}: {browsers} browser(s) for {agents} agent(s) \u{00b7} bridge on port {port}"
        )];
        if signed_out > 0 {
            rows.push(format!(
                "sign in to ChatGPT in the {signed_out} browser window(s) that need it (the login is kept for next time), then /bridge status"
            ));
        } else if unknown > 0 {
            rows.push(format!("could not tell yet whether {unknown} browser(s) are signed in; /bridge status checks again"));
        } else if !states.is_empty() {
            rows.push("every browser is signed in \u{2014} ready".to_string());
        }
        self.say_lines(rows);
        Ok(())
    }

    async fn bridge_status(&mut self) -> Result<()> {
        let port = self.bridge_port()?;
        let Some(health) = get_json(port, "/health", 3).await else {
            self.say(format!("no bridge on port {port} \u{00b7} /bridge [agents] starts one"));
            return Ok(());
        };
        let alive = self.bridge.as_mut().is_some_and(|bridge| !matches!(bridge.child.try_wait(), Ok(Some(_))));
        let owner = match (&self.bridge, alive) {
            (Some(bridge), true) => format!("started by this session, log {}", bridge.log.display()),
            _ => "not started by this session".to_string(),
        };
        let load = health["access_point_sessions"].as_array().cloned().unwrap_or_default();
        let states = login_states(port).await.unwrap_or_default();
        let mut rows = vec![format!(
            "bridge on port {port} ({owner}) \u{00b7} {} chat(s) open \u{00b7} reply source {}",
            health["sessions"].as_u64().unwrap_or(0),
            health["reply_source"].as_str().unwrap_or("?")
        )];
        for index in 0..health["access_points"].as_u64().unwrap_or(0) as usize {
            let sign_in = match states.get(index).copied().flatten() {
                Some(true) => "signed in",
                Some(false) => "NOT signed in \u{2014} sign in in that window",
                None => "sign-in unknown",
            };
            let chats = load.get(index).and_then(|n| n.as_u64()).unwrap_or(0);
            rows.push(format!("browser {} \u{00b7} {sign_in} \u{00b7} {chats} chat(s)", index + 1));
        }
        self.say_lines(rows);
        Ok(())
    }

    async fn bridge_stop_quietly(&mut self) -> bool {
        let Some(mut bridge) = self.bridge.take() else { return false };
        let _ = bridge.child.kill().await;
        // The port frees a moment later; a restart must not race it.
        let port = bridge.port;
        until(5, || async move { get(port, "/health", 1).await.is_none().then_some(()) }).await;
        true
    }

    async fn bridge_stop(&mut self) -> Result<()> {
        if self.bridge_stop_quietly().await {
            self.say("bridge stopped \u{00b7} the browser windows stay open and signed in for the next /bridge");
        } else if get_json(self.bridge_port()?, "/health", 2).await.is_some() {
            self.say("a bridge is running, but this session did not start it, so it is left alone");
        } else {
            self.say("no bridge is running");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_count_is_read_however_it_is_typed() {
        for text in ["3", "--3", "--agents 3", "--agents=3", "-n 3", "agents=3"] {
            assert_eq!(parse(text).unwrap(), Verb::Start(Some(3)), "{text}");
        }
        assert_eq!(parse("").unwrap(), Verb::Start(None));
        assert_eq!(parse("status").unwrap(), Verb::Status);
        assert_eq!(parse("--stop").unwrap(), Verb::Stop);
        assert!(parse("0").is_err());
        assert!(parse("lots").is_err());
    }

    #[test]
    fn one_browser_serves_three_agents() {
        assert_eq!(browsers_for(1), 1);
        assert_eq!(browsers_for(3), 1);
        assert_eq!(browsers_for(4), 2);
        assert_eq!(browsers_for(9), 3);
        assert_eq!(browsers_for(500), MAX_BROWSERS as usize);
    }

    #[test]
    fn the_bridge_port_comes_from_a_local_base_url() {
        assert_eq!(port_of(None).unwrap(), 8766);
        assert_eq!(port_of(Some("http://127.0.0.1:9100")).unwrap(), 9100);
        assert_eq!(port_of(Some("http://localhost:9100/v1")).unwrap(), 9100);
        assert!(port_of(Some("http://example.com:8766")).is_err());
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        assert_eq!(dechunk("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), "hello world");
    }

    #[test]
    fn a_loopback_server_is_read_back() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 512];
            let _ = stream.read(&mut request);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ready\":1}").unwrap();
        });
        let (status, body) = http_get_blocking(port, "/health", Duration::from_secs(2)).unwrap();
        server.join().unwrap();
        assert_eq!((status, body.as_str()), (200, "{\"ready\":1}"));
        assert!(http_get_blocking(1, "/", Duration::from_millis(200)).is_none(), "nothing listens on port 1");
    }
}
