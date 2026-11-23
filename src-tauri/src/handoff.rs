//! Which executable runs the app, and how control moves between the two.
//!
//! RustBox ships as two builds of one source tree (see `engine.rs`): the default
//! one on the OS webview, and the opt-in one on Chromium. They cannot be one
//! process - `libcef.dll` is in the Chromium build's import table, so the Windows
//! loader needs the payload before `main()` runs - but they can be one
//! *application*: both read the same `config.json`, and the launcher starts
//! whichever one the config asks for.
//!
//! # The shape of a hand-off
//!
//! 1. The user starts `rustbox.exe` (the default build) from the usual shortcut.
//! 2. It reads `config.json`. When `runtime` is `chromium` and an engine is
//!    installed, it starts `engines\cef\<version>\rustbox-cef.exe` and exits.
//! 3. The Chromium build writes a ready marker once its window exists, and the
//!    launcher stops waiting.
//!
//! The launcher stays alive until that marker appears, and that is deliberate.
//! Exiting immediately would turn a Chromium build that cannot start into an app
//! that does nothing when clicked - no window, no message, no log. Instead the
//! launcher watches the child: if it dies, or does not signal readiness inside
//! [`READY_TIMEOUT`], the choice is reverted, the reason is written down, and the
//! process carries on as the OS-webview build. The user gets a working app and a
//! sentence explaining why it is not the one they picked.
//!
//! # Why the Chromium build never hands back
//!
//! It has no idea where the launcher lives: it is installed inside
//! `engines\cef\<version>\`, and the launcher may be in Program Files, a portable
//! folder, or a dev `target\debug`. Rather than guess a path, the hand-off is
//! one-way. Switching back writes `runtime: os-webview` and restarts, which lands
//! on the launcher again. That also means there is exactly one place that can
//! start a process, so a misconfiguration cannot produce a fork bomb of two
//! builds starting each other.

#[cfg(feature = "wry")]
use crate::config::{Config, Runtime};
#[cfg(feature = "wry")]
use crate::engine_dist;
use crate::paths;

/// File the Chromium build writes once its window exists.
///
/// Named for the hand-off rather than the engine so a stale file from an older
/// install is obviously stale. The launcher deletes it before spawning, and
/// matches the pid it finds against the child it started, so a leftover from a
/// previous run cannot be mistaken for this one.
const READY_FILE: &str = "engine-ready.json";

/// Set in the child's environment. Not required for correctness - the Chromium
/// build contains no hand-off code at all - but it makes a stray copy of
/// `rustbox.exe` inside the engine directory fail closed instead of recursing.
#[cfg(feature = "wry")]
pub const CHILD_ENV: &str = "RUSTBOX_ENGINE_CHILD";

/// How long a Chromium start is given to produce its window.
///
/// Chromium's first paint on a cold profile is under two seconds on the machine
/// this was measured on, and the engine payload is on local disk. Thirty seconds
/// is not a performance target; it is the point past which waiting longer is
/// worse than starting the fallback, because the user is staring at nothing.
#[cfg(feature = "wry")]
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Where the launcher records why a hand-off was abandoned.
///
/// A file, not an event: the process that would emit the event is the one that
/// just decided not to start. The UI reads it on next launch and clears it.
pub fn error_path() -> std::path::PathBuf {
    paths::root().join("engine-handoff-error.txt")
}

/// The last hand-off failure, if one was recorded.
pub fn take_error() -> Option<String> {
    let path = error_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(feature = "wry")]
fn record_error(reason: &str) {
    let path = error_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{reason}\n"));
}

/// Called by the Chromium build once its window is up.
///
/// Best effort by design: failing to write this marker must not stop the app from
/// running. The only consequence is that the launcher falls back after its
/// timeout, which is the safe direction - the user ends up with two windows at
/// worst, not zero.
#[cfg(feature = "cef")]
pub fn mark_ready() {
    let payload = serde_json::json!({
        "pid": std::process::id(),
        "at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    let path = paths::root().join(READY_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, payload.to_string());
}

/// Whether this launch should hand over to the Chromium build.
///
/// Returns the exit code the caller should use when it has, and `None` when this
/// process is the one that should run the app. Every failure path inside returns
/// `None` after reverting the setting, because a hand-off that cannot happen must
/// end in a running app.
#[cfg(feature = "wry")]
pub fn take_over(config_path: &std::path::Path) -> Option<i32> {
    // A child never hands off. See [`CHILD_ENV`].
    if std::env::var_os(CHILD_ENV).is_some() {
        return None;
    }

    let mut config = match Config::load(config_path) {
        Ok(c) => c,
        // An unreadable config is not this module's problem: `AppState::new`
        // reports it properly a moment later.
        Err(_) => return None,
    };
    if config.runtime != Runtime::Chromium {
        return None;
    }

    match start_chromium() {
        Ok(()) => Some(0),
        Err(reason) => {
            // Revert, so the next launch does not repeat a hand-off that just
            // failed. The reason is kept where the UI can show it, because a
            // setting that silently changed itself is its own bug report.
            config.runtime = Runtime::OsWebview;
            let _ = config.save(config_path);
            record_error(&reason);
            eprintln!("[rustbox] Chromium engine not started: {reason}");
            None
        }
    }
}

/// Start the installed Chromium build and wait until it is up.
#[cfg(feature = "wry")]
fn start_chromium() -> Result<(), String> {
    let version = engine_dist::installed_versions()
        .pop()
        .ok_or_else(|| "the Chromium engine is not installed".to_string())?;
    let exe = engine_dist::binary(&version).ok_or_else(|| {
        format!(
            "engine {version} is installed but has no rustbox-cef binary; \
             reinstall it from Settings"
        )
    })?;

    let ready = paths::root().join(READY_FILE);
    let _ = std::fs::remove_file(&ready);

    let mut child = std::process::Command::new(&exe)
        .env(CHILD_ENV, "1")
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", exe.display()))?;

    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    loop {
        if ready_marker_matches(&ready, child.id()) {
            // The child owns the window now; nothing here should hold it.
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "the Chromium engine exited during startup ({status})"
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("could not watch the Chromium engine: {e}")),
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            return Err(format!(
                "the Chromium engine did not open a window within {}s",
                READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Whether the marker on disk was written by `pid`.
#[cfg(any(feature = "wry", test))]
fn ready_marker_matches(path: &std::path::Path, pid: u32) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    value["pid"].as_u64() == Some(u64::from(pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_is_only_accepted_from_the_child_that_wrote_it() {
        let dir = std::env::temp_dir().join(format!(
            "rb-handoff-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("engine-ready.json");

        assert!(!ready_marker_matches(&marker, 42), "no marker yet");

        std::fs::write(&marker, r#"{"pid":42,"at":1}"#).unwrap();
        assert!(ready_marker_matches(&marker, 42));
        assert!(
            !ready_marker_matches(&marker, 43),
            "a marker from another process is not this child's"
        );

        std::fs::write(&marker, "not json").unwrap();
        assert!(!ready_marker_matches(&marker, 42), "garbage is not a marker");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
