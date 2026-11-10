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

    // macOS loads CEF from the bundle; the other platforms link it directly.
    let _library = load_cef();

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
fn load_cef() -> library_loader::LibraryLoader {
    let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), false);
    assert!(loader.load(), "failed to load the CEF framework");
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    loader
}

#[cfg(not(target_os = "macos"))]
fn load_cef() {
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    // Keep the type symmetrical with the macOS branch without pulling the
    // loader into a platform that links CEF directly.
    let _ = std::marker::PhantomData::<Arc<()>>;
}
