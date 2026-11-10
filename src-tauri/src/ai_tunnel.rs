//! Public reach for the AI Router: a Cloudflare quick tunnel and Tailscale.
//!
//! Both are shells around a binary the user installs. RustBox does not bundle
//! either, and pretending otherwise would produce a button that silently does
//! nothing — so a missing binary is reported by name, with the command that
//! installs it.
//!
//! # Why a quick tunnel
//!
//! `cloudflared tunnel --url` needs no Cloudflare account, no DNS record and no
//! configuration file: it prints a random `https://<name>.trycloudflare.com` and
//! proxies to a local port. That is exactly the shape of "show me the URL my
//! client should use" this feature needs. A named tunnel is a Cloudflare account
//! setup flow, which belongs in the Cloudflare dashboard rather than here.
//!
//! The URL changes on every start, so it is read from the process's own output
//! rather than saved. Nothing here is persisted except the fact that the user
//! turned it on.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// The running tunnel, when there is one.
static TUNNEL: Mutex<Option<Tunnel>> = Mutex::new(None);
static TAILSCALE_ON: AtomicBool = AtomicBool::new(false);

struct Tunnel {
    child: Child,
    url: String,
}

/// What the page shows about the tunnel.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TunnelState {
    pub running: bool,
    /// The public base URL, e.g. `https://calm-river-1234.trycloudflare.com`.
    pub url: String,
    /// The URL a client should put in its config: the base plus `/v1`.
    pub endpoint: String,
    /// Why it is not running, when it is not.
    pub error: String,
}

/// Whether `cloudflared` is on PATH, and where.
pub fn cloudflared_path() -> Option<String> {
    which("cloudflared")
}

/// Whether `tailscale` is on PATH.
pub fn tailscale_path() -> Option<String> {
    which("tailscale")
}

/// A child process that never shows a console window on Windows.
///
/// This binary is built with `windows_subsystem = "windows"`, so it owns no
/// console. A console program started from it therefore gets a brand-new console
/// window from the OS — which is the black box that flashes over the app when a
/// page asks whether `cloudflared` or `tailscale` exists. Every process started
/// here is a console tool whose output is captured, so none should be visible.
fn hidden(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

fn which(binary: &str) -> Option<String> {
    // `where` on Windows, `which` elsewhere. Both print one path per line and
    // exit non-zero when nothing matches.
    let (program, arg) = if cfg!(windows) {
        ("where", binary)
    } else {
        ("which", binary)
    };
    let output = hidden(program).arg(arg).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

/// Current state, without touching the process.
pub fn state() -> TunnelState {
    let guard = match TUNNEL.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.as_ref() {
        Some(tunnel) => TunnelState {
            running: true,
            url: tunnel.url.clone(),
            endpoint: format!("{}/v1", tunnel.url.trim_end_matches('/')),
            error: String::new(),
        },
        None => TunnelState::default(),
    }
}

/// Start a quick tunnel to `port`.
///
/// The URL is read from the child's output, which is where `cloudflared` prints
/// its banner. Reading it here rather than asking the user to copy it is the
/// whole point: a tunnel whose address the page does not know is a tunnel the
/// user has to go find.
pub fn start(port: u16) -> Result<TunnelState, String> {
    let mut guard = match TUNNEL.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(existing) = guard.as_ref() {
        // Already up: report the live URL rather than starting a second tunnel
        // that would leak the first process.
        return Ok(TunnelState {
            running: true,
            url: existing.url.clone(),
            endpoint: format!("{}/v1", existing.url.trim_end_matches('/')),
            error: String::new(),
        });
    }
    let Some(binary) = cloudflared_path() else {
        return Err(
            "cloudflared is not installed. Install it with \
             `winget install --id Cloudflare.cloudflared -e`, then try again."
                .to_string(),
        );
    };

    let mut child = hidden(&binary)
        .arg("tunnel")
        .arg("--url")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg("--no-autoupdate")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {binary}: {e}"))?;

    // Both streams are scanned: the banner goes to stderr on the Windows build
    // and to stdout on some Linux packages.
    let url = read_url(&mut child).ok_or_else(|| {
        let _ = child.kill();
        format!(
            "cloudflared started but never printed a public URL. Run \
             `cloudflared tunnel --url http://127.0.0.1:{port}` in a terminal to see why."
        )
    })?;

    crate::ai_router::log(&format!("cloudflare tunnel up at {url}"));
    *guard = Some(Tunnel {
        child,
        url: url.clone(),
    });
    Ok(TunnelState {
        running: true,
        url: url.clone(),
        endpoint: format!("{}/v1", url.trim_end_matches('/')),
        error: String::new(),
    })
}

/// Wait for the child to print a `trycloudflare.com` URL.
///
/// Bounded: `cloudflared` prints its banner within a few seconds when it works,
/// and a reader that waits forever would freeze the command that called it.
fn read_url(child: &mut Child) -> Option<String> {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<String>();
    let streams: Vec<Streams> = [
        child.stdout.take().map(Streams::Out),
        child.stderr.take().map(Streams::Err),
    ]
    .into_iter()
    .flatten()
    .collect();
    for (index, stream) in streams.into_iter().enumerate() {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name(format!("cloudflared-reader-{index}"))
            .spawn(move || {
                let reader: Box<dyn BufRead> = match stream {
                    Streams::Out(out) => Box::new(BufReader::new(out)),
                    Streams::Err(err) => Box::new(BufReader::new(err)),
                };
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Some(url) = url_in(&line)
                        && tx.send(url).is_ok()
                    {
                        return;
                    }
                }
            })
            .ok();
    }
    rx.recv_timeout(std::time::Duration::from_secs(30)).ok()
}

enum Streams {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

/// The first `https://…trycloudflare.com` in one line of output.
fn url_in(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|' || c == ']' || c == ')')
        .unwrap_or(rest.len());
    let candidate = rest[..end].trim_end_matches(['.', ',']);
    if candidate.contains("trycloudflare.com") {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// Stop the tunnel and wait for the process to exit.
pub fn stop() -> TunnelState {
    let mut guard = match TUNNEL.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(mut tunnel) = guard.take() {
        let _ = tunnel.child.kill();
        let _ = tunnel.child.wait();
        crate::ai_router::log("cloudflare tunnel stopped");
    }
    TunnelState::default()
}

// ---- tailscale ------------------------------------------------------------

/// What the page shows about Tailscale.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TailscaleState {
    pub installed: bool,
    /// The machine's tailnet address, when Tailscale is up.
    pub address: String,
    /// MagicDNS name, when there is one.
    pub hostname: String,
    pub enabled: bool,
    /// Why the address is not known, when it is not.
    pub error: String,
}

/// Read the tailnet address.
///
/// `tailscale ip -4` is the cheap question with a precise answer; it fails when
/// the daemon is not running, which is exactly the state the page needs to
/// distinguish from "not installed".
pub fn tailscale_state() -> TailscaleState {
    let installed = tailscale_path().is_some();
    let enabled = TAILSCALE_ON.load(Ordering::SeqCst);
    if !installed {
        return TailscaleState {
            installed: false,
            enabled: false,
            error: "Tailscale is not installed.".to_string(),
            ..TailscaleState::default()
        };
    }
    let mut state = TailscaleState {
        installed: true,
        enabled,
        ..TailscaleState::default()
    };
    match hidden("tailscale").arg("ip").arg("-4").output() {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            state.address = text.lines().next().unwrap_or_default().trim().to_string();
        }
        Ok(output) => {
            state.error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if state.error.is_empty() {
                state.error = "Tailscale is not connected.".to_string();
            }
        }
        Err(e) => state.error = format!("could not run tailscale: {e}"),
    }
    // `tailscale status --json` carries the DNS name; a failure here is not
    // fatal because the IP is enough to build a URL. Only asked when there is an
    // address to name: a machine whose daemon is down gains nothing from a second
    // process launch that would report the same failure.
    if !state.address.is_empty()
        && let Ok(output) = hidden("tailscale")
            .arg("status")
            .arg("--json")
            .output()
        && output.status.success()
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(name) = value
            .get("Self")
            .and_then(|s| s.get("DNSName"))
            .and_then(|d| d.as_str())
    {
        state.hostname = name.trim_end_matches('.').to_string();
    }
    state
}

/// Turn the tailnet route on or off.
///
/// Enabling does not start anything: the router already listens on every
/// interface when publishing is on, and Tailscale is the transport. What this
/// flag decides is whether the page reports the tailnet address as a supported
/// way in, and whether the router rebinds to include it.
pub fn set_tailscale(on: bool) -> TailscaleState {
    if on {
        let state = tailscale_state();
        if !state.installed || state.address.is_empty() {
            TAILSCALE_ON.store(false, Ordering::SeqCst);
            return TailscaleState {
                enabled: false,
                ..state
            };
        }
    }
    TAILSCALE_ON.store(on, Ordering::SeqCst);
    let mut state = tailscale_state();
    state.enabled = on;
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trycloudflare_url_is_picked_out_of_the_banner() {
        let line = "2024-01-01 INF |  https://calm-river-1234.trycloudflare.com  |";
        assert_eq!(
            url_in(line).as_deref(),
            Some("https://calm-river-1234.trycloudflare.com")
        );
    }

    #[test]
    fn an_unrelated_https_line_is_not_a_tunnel_url() {
        assert!(url_in("https://api.cloudflare.com/client/v4").is_none());
        assert!(url_in("no link here").is_none());
    }

    #[test]
    fn the_endpoint_appends_the_version_segment() {
        let state = TunnelState {
            running: true,
            url: "https://x.trycloudflare.com/".to_string(),
            endpoint: String::new(),
            error: String::new(),
        };
        assert_eq!(
            format!("{}/v1", state.url.trim_end_matches('/')),
            "https://x.trycloudflare.com/v1"
        );
    }
}
