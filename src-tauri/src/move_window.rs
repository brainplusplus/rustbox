//! The window a launch shows while a queued data-folder move runs.
//!
//! Why this is a separate startup mode rather than a pane in the app: the move
//! has to happen before any profile, cookie database or engine DLL is open (see
//! `data_root`), and the shell window itself is a webview whose data lives in
//! the folder being copied. So the move cannot run "inside" the normal app.
//!
//! What it can do is run in a window that keeps *nothing* in either data folder:
//! this session's webview profile goes to a scratch directory under the temp
//! folder, which is also the root `paths` reports for the duration (`init_root`
//! below), so an accidental path lookup cannot reach the tree under the copy.
//!
//! When the move ends - moved, failed or cancelled - the only way forward is a
//! fresh process: `paths::root` is frozen for the lifetime of a process on
//! purpose, and the moving session's root is the scratch directory. `restart`
//! re-runs the same executable, which then finds no queued move and starts the
//! app normally on the new folder.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::data_root::{self, End, Job, Progress};

/// Where the moving session keeps its own webview profile.
///
/// Outside both data folders by construction. A fixed name rather than a
/// per-process one so a session that was killed leaves one directory to clean
/// up instead of one per crash.
fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join("rustbox-move-session")
}

/// Opt-in WebView2 debugging port for this window, in debug builds only.
///
/// The move window is the one surface a user cannot click past to reach a
/// console, so when it misbehaves there is nothing to read. A port is the only
/// way to ask it what it loaded - but an open debugging port in a shipped
/// binary is a way to drive the app, so release builds never open one.
fn debug_port() -> Option<u16> {
    #[cfg(debug_assertions)]
    {
        std::env::var("RB_MOVE_DEBUG_PORT").ok()?.parse().ok()
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// What the page is shown. One struct rather than a set of atomics: every field
/// has to agree with the others for the bar to make sense.
struct Status {
    state: &'static str,
    phase: &'static str,
    files: u64,
    bytes: u64,
    total_files: u64,
    total_bytes: u64,
    seconds: u64,
    from: String,
    to: String,
    error: Option<String>,
    warning: Option<String>,
}

impl Status {
    fn queued(job: &Job) -> Self {
        Self {
            state: "running",
            phase: "start",
            files: 0,
            bytes: 0,
            total_files: 0,
            total_bytes: 0,
            seconds: 0,
            from: job.from.display().to_string(),
            to: job.to.display().to_string(),
            error: None,
            warning: None,
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "state": self.state,
            "phase": self.phase,
            "files": self.files,
            "bytes": self.bytes,
            "totalFiles": self.total_files,
            "totalBytes": self.total_bytes,
            "seconds": self.seconds,
            "from": self.from,
            "to": self.to,
            "error": self.error,
            "warning": self.warning,
        })
    }
}

struct Moving {
    state: Mutex<Status>,
    /// Read between files by the copy loop. A plain atomic because the loop
    /// asks thousands of times and a lock there would be the expensive part.
    cancel: AtomicBool,
    running: AtomicBool,
}

impl Moving {
    fn new(job: Job) -> Self {
        Self {
            state: Mutex::new(Status::queued(&job)),
            cancel: AtomicBool::new(false),
            running: AtomicBool::new(false),
        }
    }

    fn snapshot(&self) -> serde_json::Value {
        match self.state.lock() {
            Ok(state) => state.json(),
            Err(e) => e.into_inner().json(),
        }
    }

    fn on_progress(&self, p: Progress) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // A late reading must not overwrite a finished state: the copy thread
        // can report once more on its way out.
        if state.state != "running" {
            return;
        }
        state.phase = p.phase;
        state.files = p.files;
        state.bytes = p.bytes;
        state.total_files = p.total_files;
        state.total_bytes = p.total_bytes;
    }

    fn finish(&self, end: End, seconds: u64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.seconds = seconds;
        match end {
            End::Moved {
                warning,
                files,
                bytes,
                ..
            } => {
                state.state = "done";
                state.phase = "finish";
                state.files = files;
                state.bytes = bytes;
                state.warning = warning;
            }
            End::Cancelled => {
                state.state = "cancelled";
                state.phase = "start";
            }
            End::Failed(why) => {
                state.state = "failed";
                state.error = Some(why);
            }
            // Only reachable if the request vanished under us, which would be a
            // different bug; saying so beats a window that spins for ever.
            End::NoRequest => {
                state.state = "failed";
                state.error = Some("the queued move was no longer there".to_string());
            }
        }
    }
}

/// Whether this launch is the moving session.
pub fn take() -> Option<Job> {
    data_root::pending()
}

/// Point this session's window at `move.html` and its profile at the scratch
/// directory.
///
/// Mutating the generated config is the same hook `main` already uses for
/// `app_directories_override`; a second window declared in `tauri.conf.json`
/// would otherwise be built for every launch, including the ones that never
/// move anything.
pub fn configure(context: &mut tauri::Context) {
    context.config_mut().app.app_directories_override = Some(
        tauri::utils::config::AppDirectoriesOverride::Directories(
            tauri::utils::config::AppDirectoryOverrides {
                local_data: Some(scratch_dir()),
                ..Default::default()
            },
        ),
    );
    let windows = &mut context.config_mut().app.windows;
    if let Some(window) = windows.first_mut() {
        window.label = "main".to_string();
        window.url = tauri::WebviewUrl::App("move.html".into());
        window.title = "RustBox - moving your data".to_string();
        window.width = 560.0;
        window.height = 300.0;
        window.maximized = false;
        window.resizable = false;
        window.maximizable = false;
        window.center = true;
        window.visible = true;
        // The same switches as every other window. They have to be restated
        // rather than inherited: a window's `additional_browser_args` replaces
        // the shared list, and WebView2 refuses a second environment whose
        // options differ for the same user data folder. Chromium has no
        // per-webview argument string - its switches are process-wide (see
        // `engine::apply`) - so the Chromium build does not set one.
        #[cfg(feature = "wry")]
        {
            window.additional_browser_args = Some(match debug_port() {
                Some(port) => format!("{} --remote-debugging-port={port}", crate::webview_args::BASE),
                None => crate::webview_args::BASE.to_string(),
            });
        }
    }
}

/// Run the moving session. Never returns: the app either restarts or exits.
pub fn run(context: tauri::Context, job: Job) -> ! {
    // A session that was killed leaves its webview profile behind, and WebView2
    // refuses a profile a live browser is using - the previous one is gone, but
    // a lock file can outlive it.
    let _ = std::fs::remove_dir_all(scratch_dir());
    // Freeze `paths` at the scratch directory for this process. Everything that
    // asks where the data lives gets an answer that cannot touch the tree being
    // copied; the move itself is handed its two folders explicitly.
    crate::paths::init_root(scratch_dir(), None);

    let moving = Arc::new(Moving::new(job));
    let watching = moving.clone();
    let app = crate::engine::apply(tauri::Builder::default(), debug_port())
        .manage(moving.clone())
        .invoke_handler(tauri::generate_handler![
            move_status,
            move_cancel,
            move_start,
            move_continue
        ])
        .setup(move |_app| {
            start_copy(moving.clone());
            Ok(())
        })
        .build(context)
        .expect("error while building rustbox");

    app.run(move |_app, event| {
        // Closing the window mid-copy would kill the process with a half-copied
        // target. The first close is turned into the same cancel the button
        // sends, so the page can say what happened and the leftovers are rolled
        // back; a close that arrives while nothing is running goes through.
        if let tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } = event
        {
            if !watching.running.load(Ordering::SeqCst) {
                return;
            }
            api.prevent_close();
            watching.cancel.store(true, Ordering::SeqCst);
        }
    });
    std::process::exit(0)
}

/// Remove the profile this session used, on the launches that never move.
///
/// A session that was killed leaves its webview profile in the temp folder, and
/// nothing else uses that name. Best effort: a locked file is not worth a
/// failed launch.
pub fn cleanup() {
    let _ = std::fs::remove_dir_all(scratch_dir());
}

fn start_copy(moving: Arc<Moving>) {
    // Retry comes through here too, so the guard is what keeps a double click
    // from running two copies into the same folder.
    if moving.running.swap(true, Ordering::SeqCst) {
        return;
    }
    moving.cancel.store(false, Ordering::SeqCst);
    {
        let Ok(mut state) = moving.state.lock() else {
            return;
        };
        let job = Job {
            from: PathBuf::from(&state.from),
            to: PathBuf::from(&state.to),
        };
        *state = Status::queued(&job);
    }

    std::thread::spawn(move || {
        let job = {
            let Ok(state) = moving.state.lock() else {
                return;
            };
            Job {
                from: PathBuf::from(&state.from),
                to: PathBuf::from(&state.to),
            }
        };
        let started = std::time::Instant::now();
        // The move's own answer, kept where the next launch reads it - this
        // session cannot report it, because it ends by restarting.
        let (_, end) = data_root::run(
            &job.from,
            &job.to,
            &crate::paths::locator_dir(),
            &mut |p| moving.on_progress(p),
            &|| moving.cancel.load(Ordering::SeqCst),
        );
        moving.finish(end, started.elapsed().as_secs());
        moving.running.store(false, Ordering::SeqCst);
    });
}

/// Polled by the page. Cheap on purpose: it is asked four times a second.
#[tauri::command]
fn move_status(moving: tauri::State<'_, Arc<Moving>>) -> serde_json::Value {
    moving.snapshot()
}

/// Ask the copy to stop. The answer is asynchronous: the loop notices between
/// files and the page sees `state == "cancelled"` when the copy is gone.
#[tauri::command]
fn move_cancel(moving: tauri::State<'_, Arc<Moving>>) {
    moving.cancel.store(true, Ordering::SeqCst);
}

/// Run the move again after a failure or a cancel.
#[tauri::command]
fn move_start(moving: tauri::State<'_, Arc<Moving>>) {
    start_copy(moving.inner().clone());
}

/// Leave this window for the real app.
///
/// A restart, not a window swap: the root is frozen per process, and this
/// process is pinned to the scratch directory.
#[tauri::command]
fn move_continue(app: tauri::AppHandle, moving: tauri::State<'_, Arc<Moving>>) {
    if moving.running.load(Ordering::SeqCst) {
        return;
    }
    app.restart();
}
