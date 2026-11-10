//! CEF spike for milestone M0 of `docs/adr/0005-embedded-chromium-cef.md`.
//!
//! One question, answered by running it: can CEF open a window and render a page
//! on this machine, and does it work when a *host* loop drives CEF with
//! `do_message_loop_work()` instead of handing CEF the thread?
//!
//! That is the fork in the ADR's design:
//!
//! * host-pumped (`do_message_loop_work` from our loop) — one loop, which is what
//!   Tauri needs. Default here.
//! * `--multi-threaded` — CEF owns a thread (`multi_threaded_message_loop = 1`).
//!   The fallback, kept as a flag so the two can be compared on the same machine.
//!
//! A heartbeat prints every 500 ms so a blocked loop is *seen*, not guessed:
//! pumps still climbing means the host loop is alive; a stall means it is not.
//!
//! The handler and app shapes are ported from cef-rs's own `cefsimple` example
//! (`examples/cefsimple/src/shared/`), because the wrapper macros are specific
//! about field names, `&self` receivers and return types. Guessing them does not
//! compile.
//!
//! Run (see `run-macos.sh` on macOS, which builds the bundle CEF requires):
//!     cargo run --release
//!     cargo run --release -- --multi-threaded

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cef::*;

static BROWSER_CREATED: AtomicBool = AtomicBool::new(false);
static PUMPS: AtomicU64 = AtomicU64::new(0);

wrap_display_handler! {
    struct SpikeDisplayHandler;

    impl DisplayHandler {
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: i32,
        ) -> i32 {
            if let Some(text) = message {
                println!("[page] {}", text.to_string());
            }
            0
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(title) = title {
                println!("[page] title={}", title.to_string());
            }
        }
    }
}

wrap_life_span_handler! {
    struct SpikeLifeSpanHandler;

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            BROWSER_CREATED.store(true, Ordering::SeqCst);
            if let Some(browser) = browser {
                // Only that the browser exists and has a main frame: the URL
                // accessor returns a userfree UTF-16 string with no Display or
                // Debug impl, and the spike has no use for the value here.
                println!(
                    "[spike] after_created, has main frame: {}",
                    browser.main_frame().is_some()
                );
            }
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            println!("[spike] browser closing");
            // Closing the last window is how the spike ends: without this the
            // process keeps pumping a loop with nothing left to paint.
            quit_message_loop();
        }
    }
}

wrap_load_handler! {
    struct SpikeLoadHandler;

    impl LoadHandler {
        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            http_status_code: i32,
        ) {
            println!("[spike] load_end status={http_status_code}");
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            println!(
                "[spike] load_error code={error_code:?} text={:?} url={:?}",
                error_text.map(|e| e.to_string()),
                failed_url.map(|u| u.to_string())
            );
        }
    }
}

wrap_client! {
    struct SpikeClient;

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(SpikeDisplayHandler::new())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(SpikeLifeSpanHandler::new())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(SpikeLoadHandler::new())
        }
    }
}

wrap_app! {
    struct SpikeApp {
        client: RefCell<Option<Client>>,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(SpikeBrowserProcessHandler::new())
        }
    }
}

wrap_browser_process_handler! {
    struct SpikeBrowserProcessHandler;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            // The browser must be created after this point: CEF has no browser
            // process context before it.
            println!("[spike] context initialized");
        }
    }
}

fn main() {
    let multi_threaded = std::env::args().any(|a| a == "--multi-threaded");
    let url = std::env::args()
        .skip(1)
        .find(|a| a.starts_with("http"))
        .unwrap_or_else(|| "https://example.com".to_string());

    // Not a preference: CEF's own header says multi_threaded_message_loop is
    // "only supported on Windows and Linux". The flag is kept so the same command
    // can be compared across platforms, but on macOS it is refused rather than
    // attempted, because attempting it produces a run that hangs and reads exactly
    // like "the host-pumped loop failed too".
    if multi_threaded && !multi_threaded_supported(cfg!(target_os = "macos")) {
        eprintln!("[spike] --multi-threaded is not usable on macOS.");
        eprintln!("        CEF documents multi_threaded_message_loop as Windows/Linux");
        eprintln!("        only; here it does not give a working loop, it gives a");
        eprintln!("        process that never paints. That would look like a loop");
        eprintln!("        failure and must not be filed as one. Re-run without it.");
        std::process::exit(2);
    }

    // Decided from the raw argv, before any CEF call: on macOS the framework has
    // to be loaded before CEF is touched, and where to load it from depends on
    // whether this process is a helper.
    let helper = is_helper_process(std::env::args());
    // macOS loads CEF from the bundle; the other platforms link it directly.
    let _library = load_cef(helper);

    let args = cef::args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        eprintln!("[spike] cannot read the command line");
        std::process::exit(1);
    };

    let switch = CefString::from("type");
    let is_browser_process = cmd_line.has_switch(Some(&switch)) != 1;
    let main_args = args.as_main_args();

    let ret = execute_process(Some(main_args), None, std::ptr::null_mut());
    if !is_browser_process {
        // A CEF sub-process (renderer, GPU, utility). It must not initialise CEF
        // or run a loop; returning here is the whole job.
        std::process::exit(if ret >= 0 { 0 } else { 1 });
    }

    let mut app = SpikeApp::new(RefCell::new(None));
    let settings = Settings {
        no_sandbox: 1,
        // The fork. The host pumps unless asked otherwise.
        external_message_pump: if multi_threaded { 0 } else { 1 },
        multi_threaded_message_loop: if multi_threaded { 1 } else { 0 },
        windowless_rendering_enabled: 0,
        log_severity: LogSeverity::INFO,
        ..Default::default()
    };

    assert_eq!(
        initialize(Some(main_args), Some(&settings), Some(&mut app), std::ptr::null_mut()),
        1,
        "cef initialize failed"
    );

    let started = Instant::now();
    let heartbeat = std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            println!(
                "[spike] t={:>5}ms pumps={} browser={}",
                started.elapsed().as_millis(),
                PUMPS.load(Ordering::Relaxed),
                BROWSER_CREATED.load(Ordering::Relaxed)
            );
            if started.elapsed() > Duration::from_secs(60) {
                println!("[spike] 60s elapsed, stopping");
                quit_message_loop();
                return;
            }
        }
    });

    create_browser(&url);

    if multi_threaded {
        println!("[spike] CEF owns its loop thread; the host loop is not used");
        // Nothing to pump: the browser closes the loop when its window closes.
        while BROWSER_CREATED.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    } else {
        println!("[spike] host pumps CEF with do_message_loop_work()");
        // Stand in for tao's loop: pump, do a little host work, repeat.
        loop {
            do_message_loop_work();
            PUMPS.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(5));
            if started.elapsed() > Duration::from_secs(60) {
                break;
            }
        }
    }

    shutdown();
    let _ = heartbeat.join();
    println!("[spike] clean shutdown");
}

/// Create one windowed browser showing `url`.
///
/// Alloy style, so CEF makes an ordinary window rather than the Chrome-UI shell;
/// that is what an embedded tab inside RustBox needs.
fn create_browser(url: &str) {
    let mut client = SpikeClient::new();

    let window_info = WindowInfo {
        runtime_style: RuntimeStyle::ALLOY,
        ..Default::default()
    };

    #[cfg(target_os = "windows")]
    let window_info = window_info.set_as_popup(Default::default(), "RustBox CEF spike");

    let settings = BrowserSettings::default();
    let created = browser_host_create_browser(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from(url)),
        Some(&settings),
        None,
        None,
    );
    println!("[spike] create_browser returned {created}");
}

#[cfg(target_os = "macos")]
fn load_cef(helper: bool) -> library_loader::LibraryLoader {
    let exe = std::env::current_exe().unwrap();
    // The loader the crate installs derives the framework path from the executable
    // and panics inside `canonicalize()` when it is wrong, which on a helper looks
    // like a page that never loads. Checking first turns that into a sentence.
    if find_framework(&exe).is_none() {
        eprintln!("[spike] no CEF framework above {}", exe.display());
        eprintln!("        expected a Contents/Frameworks/{FW_NAME}/");
        eprintln!("        run tools/cef-spike/run-macos.sh, which builds the bundle");
        std::process::exit(1);
    }
    // A helper runs two levels deeper (inside its own .app), so its framework is
    // three levels up instead of one.
    let loader = library_loader::LibraryLoader::new(&exe, helper);
    assert!(loader.load(), "failed to load the CEF framework");
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    loader
}

const FW_NAME: &str = "Chromium Embedded Framework.framework";
const FW_BINARY: &str = "Chromium Embedded Framework";

/// Find the CEF framework binary for an executable inside a macOS `.app` bundle.
///
/// The main binary and CEF's helpers live under the same `.app` at different
/// depths: `Contents/MacOS/<bin>` versus
/// `Contents/Frameworks/<name>.app/Contents/MacOS/<bin>`. Walking up and accepting
/// the first directory that actually holds the framework covers both without
/// hard-coding either depth.
///
/// Kept pure and free of `cfg` so the rule can be tested on any platform; only the
/// actual framework load needs a Mac.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn find_framework(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    for dir in exe.ancestors().skip(1).take(6) {
        // `dir/Frameworks/…` is the layout macOS code signing expects and the one
        // run-macos.sh builds; the bare `dir/…` form covers a framework placed
        // directly beside the executable.
        for candidate in [dir.join("Frameworks").join(FW_NAME), dir.join(FW_NAME)] {
            let binary = candidate.join(FW_BINARY);
            if binary.is_file() {
                return Some(binary);
            }
        }
    }
    None
}

/// Whether this process is one of CEF's sub-processes (renderer, GPU, utility).
///
/// CEF launches them with `--type=<kind>`; the browser process has no such switch.
/// Kept pure and free of `cfg` so it can be tested anywhere, since the only thing
/// checkable off a Mac is this decision, not the framework load that follows it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_helper_process<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == "--type" || a.starts_with("--type="))
}

/// Whether `--multi-threaded` can work on a given platform.
///
/// CEF's `cef_settings_t` doc comment says `multi_threaded_message_loop` is *"only
/// supported on Windows and Linux"*. Pure and separated from `cfg!` so the rule is
/// testable on every platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn multi_threaded_supported(is_macos: bool) -> bool {
    !is_macos
}

#[cfg(not(target_os = "macos"))]
fn load_cef(_helper: bool) {
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    // Keep the type symmetrical with the macOS branch without pulling the
    // loader into a platform that links CEF directly.
    let _ = std::marker::PhantomData::<Arc<()>>;
}

#[cfg(test)]
mod tests {
    use super::{find_framework, is_helper_process, multi_threaded_supported};

    #[test]
    fn the_browser_process_has_no_type_switch() {
        assert!(!is_helper_process(["--multi-threaded".to_string()]));
        assert!(!is_helper_process(Vec::new()));
    }

    /// The spike refuses this flag on macOS instead of trying it. Attempting it
    /// there yields a process that never paints, which would be filed as "the
    /// host-pumped loop failed" when the cause is the unsupported flag.
    #[test]
    fn multi_threaded_is_refused_where_cef_does_not_support_it() {
        assert!(!multi_threaded_supported(true), "macOS must refuse it");
        assert!(multi_threaded_supported(false), "Windows/Linux keep it");
    }

    #[test]
    fn a_sub_process_is_recognised_by_its_type_switch() {
        // The forms CEF actually uses when it spawns the renderer and the GPU
        // process. Getting this wrong on macOS loads the framework from the wrong
        // place and the page load hangs with nothing in the log.
        for kind in [
            "--type=renderer",
            "--type=gpu-process",
            "--type=utility",
            "--type",
        ] {
            assert!(
                is_helper_process(["--no-sandbox".to_string(), kind.to_string()]),
                "{kind} should be recognised"
            );
        }
    }

    /// The macOS binary and its helpers sit at different depths, and the framework
    /// lookup is the one part of that path logic that can be checked off a Mac.
    #[test]
    fn the_framework_is_found_from_both_bundle_depths() {
        let root = std::env::temp_dir().join(format!("cefspike-fw-{}", std::process::id()));
        let app = root.join("RustBoxCefSpike.app");

        let make_framework = |frameworks: &std::path::Path| {
            let binary = frameworks
                .join("Chromium Embedded Framework.framework")
                .join("Chromium Embedded Framework");
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, b"stub").unwrap();
            binary
        };

        // Main binary: Contents/Frameworks, one level up from Contents/MacOS.
        let main_dir = app.join("Contents").join("MacOS");
        std::fs::create_dir_all(&main_dir).unwrap();
        let expected = make_framework(&app.join("Contents").join("Frameworks"));

        let main_exe = main_dir.join("RustBoxCefSpike");
        std::fs::write(&main_exe, b"stub").unwrap();
        assert_eq!(
            find_framework(&main_exe).map(|p| p.canonicalize().unwrap()),
            Some(expected.canonicalize().unwrap()),
            "the main binary did not resolve the framework beside it"
        );

        // Helper binary: two levels deeper, under Frameworks/<name>.app.
        let helper_dir = app
            .join("Contents")
            .join("Frameworks")
            .join("RustBoxCefSpike Helper (Renderer).app")
            .join("Contents")
            .join("MacOS");
        std::fs::create_dir_all(&helper_dir).unwrap();
        let helper_exe = helper_dir.join("RustBoxCefSpike Helper (Renderer)");
        std::fs::write(&helper_exe, b"stub").unwrap();
        assert_eq!(
            find_framework(&helper_exe).map(|p| p.canonicalize().unwrap()),
            Some(expected.canonicalize().unwrap()),
            "a helper did not find the app's framework from its nested .app"
        );

        // Nothing at all: the caller prints a sentence instead of panicking.
        let bare = root.join("bare").join("elsewhere");
        std::fs::create_dir_all(&bare).unwrap();
        let bare_exe = bare.join("cef-spike");
        std::fs::write(&bare_exe, b"stub").unwrap();
        assert_eq!(find_framework(&bare_exe), None);

        std::fs::remove_dir_all(&root).ok();
    }
}
