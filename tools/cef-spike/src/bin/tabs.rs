//! Head-to-head benchmark harness: N CEF tabs in one client.
//!
//! Sibling of `docs/adr/0005-embedded-chromium-cef.md` M0 spike, but answering a
//! different question: when RustBox runs ten app tabs, how much RSS and idle CPU
//! does the CEF side actually hold?
//!
//! Modes:
//!   --mode shared    every tab uses the default request context (data shared)
//!   --mode contexts  every tab gets its own request context (data isolated,
//!                    process tree still shared — this is CEF's `Isolated` ceiling)
//!
//! True process isolation is N processes, so run this binary N times with
//! `--tabs 1 --cache <own dir>` instead; that is what `--cache` is for.
//!
//! Prints only machine-readable lines: READY {json} then DONE.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cef::*;

static CREATED: AtomicUsize = AtomicUsize::new(0);
static LOADED: AtomicUsize = AtomicUsize::new(0);

/// Script injected into every main frame once it finishes loading.
///
/// Exists to ask a real page what it decided — the question a memory benchmark
/// cannot answer. The script reports by setting `document.title` to a string
/// starting with `PROBE:`, which `on_title_change` prints, so no V8 callback
/// plumbing is needed.
static PROBE_JS: Mutex<Option<String>> = Mutex::new(None);

wrap_display_handler! {
    struct TabsDisplayHandler;

    impl DisplayHandler {
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: i32,
        ) -> i32 {
            if level == LogSeverity::ERROR {
                if let Some(text) = message {
                    println!("[page] console: {}", text.to_string());
                }
            }
            0
        }

        /// Probe results arrive here. The injected script cannot print, so it
        /// writes its finding into `document.title` with a `PROBE:` prefix and
        /// this handler forwards it. Page-set titles are ignored, which keeps
        /// the output machine-readable.
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(t) = title {
                let text = t.to_string();
                if text.starts_with("PROBE:") {
                    println!("{text}");
                }
            }
        }
    }
}

wrap_life_span_handler! {
    struct TabsLifeSpanHandler;

    impl LifeSpanHandler {
        fn on_after_created(&self, _browser: Option<&mut Browser>) {
            CREATED.fetch_add(1, Ordering::SeqCst);
        }
    }
}

wrap_load_handler! {
    struct TabsLoadHandler;

    impl LoadHandler {
        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: i32,
        ) {
            // Inject into the main frame only: sub-frames finish loading many
            // times per page and would multiply the probe output.
            if let Some(f) = frame {
                if f.is_main() == 1 {
                    if let Some(js) = PROBE_JS.lock().unwrap().as_ref() {
                        let code = CefString::from(js.as_str());
                        f.execute_java_script(Some(&code), None, 0);
                    }
                }
            }
            LOADED.fetch_add(1, Ordering::SeqCst);
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
                "[page] load_error code={error_code:?} text={:?} url={:?}",
                error_text.map(|e| e.to_string()),
                failed_url.map(|u| u.to_string())
            );
        }
    }
}

wrap_client! {
    struct TabsClient;

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(TabsDisplayHandler::new())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(TabsLifeSpanHandler::new())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(TabsLoadHandler::new())
        }
    }
}

wrap_browser_process_handler! {
    struct TabsBrowserProcessHandler;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            println!("[cef] context initialized");
        }
    }
}

wrap_app! {
    struct TabsApp {
        client: RefCell<Option<Client>>,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(TabsBrowserProcessHandler::new())
        }
    }
}

/// A host UI thread with its own standard message loop.
///
/// This is the shape Tauri has: the main thread owns a window and pumps it with
/// GetMessage/DispatchMessage. With `multi_threaded_message_loop=true` CEF runs
/// its own UI thread, so the two loops never service each other's queues. The
/// question this settles is whether that coexistence is clean, because a hang
/// here would mean the "opt-in Chromium next to WebView2" design is not
/// implementable on Windows at all.
#[cfg(target_os = "windows")]
mod hostui {
    use std::ffi::c_void;

    type Hwnd = *mut c_void;
    type Hinstance = *mut c_void;

    #[repr(C)]
    struct WndClassW {
        style: u32,
        lpfn_wnd_proc: Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: Hinstance,
        h_icon: *mut c_void,
        h_cursor: *mut c_void,
        hbr_background: *mut c_void,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
    }

    #[repr(C)]
    struct Msg {
        hwnd: Hwnd,
        message: u32,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt_x: i32,
        pt_y: i32,
    }

    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> Hinstance;
        fn RegisterClassW(c: *const WndClassW) -> u16;
        fn CreateWindowExW(
            ex: u32,
            class: *const u16,
            title: *const u16,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: Hwnd,
            menu: *mut c_void,
            inst: Hinstance,
            param: *mut c_void,
        ) -> Hwnd;
        fn DefWindowProcW(h: Hwnd, msg: u32, wp: usize, lp: isize) -> isize;
        fn ShowWindow(h: Hwnd, cmd: i32) -> i32;
        fn UpdateWindow(h: Hwnd) -> i32;
        fn GetMessageW(m: *mut Msg, h: Hwnd, min: u32, max: u32) -> i32;
        fn TranslateMessage(m: *const Msg) -> i32;
        fn DispatchMessageW(m: *const Msg) -> isize;
        fn SetTimer(h: Hwnd, id: usize, ms: u32, f: *mut c_void) -> usize;
        fn PostQuitMessage(code: i32);
        fn DestroyWindow(h: Hwnd) -> i32;
    }

    const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    const SW_SHOW: i32 = 5;
    const WM_DESTROY: u32 = 0x0002;
    const WM_TIMER: u32 = 0x0113;
    const CS_HREDRAW: u32 = 0x0002;
    const CS_VREDRAW: u32 = 0x0001;

    unsafe extern "system" fn wnd_proc(h: Hwnd, msg: u32, wp: usize, lp: isize) -> isize {
        match msg {
            WM_TIMER | WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn run(seconds: u64) {
        unsafe {
            let inst = GetModuleHandleW(std::ptr::null());
            let class_name = wide("RustBoxHostLoopProbe");
            let wc = WndClassW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfn_wnd_proc: Some(wnd_proc),
                cb_cls_extra: 0,
                cb_wnd_extra: 0,
                h_instance: inst,
                h_icon: std::ptr::null_mut(),
                h_cursor: std::ptr::null_mut(),
                hbr_background: std::ptr::null_mut(),
                lpsz_menu_name: std::ptr::null(),
                lpsz_class_name: class_name.as_ptr(),
            };
            let atom = RegisterClassW(&wc);
            println!("[host] register_class -> {atom}");

            let title = wide("host-loop");
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                900,
                120,
                420,
                320,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                inst,
                std::ptr::null_mut(),
            );
            println!("[host] hwnd -> {hwnd:?}");
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            SetTimer(hwnd, 1, (seconds * 1000) as u32, std::ptr::null_mut());

            let mut m: Msg = std::mem::zeroed();
            while GetMessageW(&mut m, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            let _ = DestroyWindow(hwnd);
            println!("[host] loop exited");
        }
    }
}

struct Opts {
    tabs: usize,
    mode: String,
    url_base: String,
    urls: Vec<String>,
    /// Script injected into every main frame after load. `None` = pure
    /// benchmark mode, which is what the memory numbers were measured with.
    probe: Option<String>,
    /// Create a host-owned window and pump it with a standard message loop, the
    /// way Tauri does. Only meaningful with `--multi-threaded`.
    host_loop: bool,
    idle: u64,
    label: String,
    tag: String,
    cache: Option<String>,
    no_sandbox: bool,
    multi_threaded: bool,
}

fn parse_opts() -> Opts {
    let mut o = Opts {
        tabs: 10,
        mode: "shared".to_string(),
        url_base: "http://127.0.0.1:".to_string(),
        urls: Vec::new(),
        probe: None,
        host_loop: false,
        idle: 45,
        label: "cef".to_string(),
        tag: "cef".to_string(),
        cache: None,
        no_sandbox: false,
        multi_threaded: false,
    };
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--tabs" => {
                o.tabs = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--mode" => {
                o.mode = args[i + 1].clone();
                i += 2;
            }
            "--url-base" => {
                o.url_base = args[i + 1].clone();
                i += 2;
            }
            // Explicit per-tab URLs (comma separated). `--url-base` builds a
            // synthetic port per tab, which cannot express "three real apps".
            "--urls" => {
                o.urls = args[i + 1]
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                i += 2;
            }
            // Same question the WebView2 harness was asked: does the page serve
            // a sign-in form, or a "browser may not be secure" wall?
            "--probe" => {
                o.probe = Some(args[i + 1].clone());
                i += 2;
            }
            // Inline JS on a command line is a quoting minefield; a file is not.
            "--probe-file" => {
                o.probe = Some(std::fs::read_to_string(&args[i + 1]).expect("probe file"));
                i += 2;
            }
            "--idle" => {
                o.idle = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--label" => {
                o.label = args[i + 1].clone();
                i += 2;
            }
            "--tag" => {
                o.tag = args[i + 1].clone();
                i += 2;
            }
            "--cache" => {
                o.cache = Some(args[i + 1].clone());
                i += 2;
            }
            "--no-sandbox" => {
                o.no_sandbox = true;
                i += 1;
            }
            // The fork from the ADR: CEF owns the loop instead of the host. Used
            // here to tell "the host failed to pump" apart from "the engine hung".
            "--multi-threaded" => {
                o.multi_threaded = true;
                i += 1;
            }
            // The Tauri shape: a host window with its own GetMessage loop, while
            // CEF services its browsers on its own UI thread.
            "--host-loop" => {
                o.host_loop = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    o
}

fn main() {
    let o = parse_opts();

    // CEF refuses to initialise unless the host has checked the API hash first:
    // without it the browser process dies on a Chromium CHECK with no message,
    // which reads like "CEF is broken" rather than "you skipped a call".
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = cef::args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        eprintln!("[cef] cannot read the command line");
        std::process::exit(1);
    };
    let switch = CefString::from("type");
    let is_browser_process = cmd_line.has_switch(Some(&switch)) != 1;
    let main_args = args.as_main_args();

    let ret = execute_process(Some(main_args), None, std::ptr::null_mut());
    if !is_browser_process {
        std::process::exit(if ret >= 0 { 0 } else { 1 });
    }

    // Publish the probe before any browser exists, so `on_load_end` can find it.
    *PROBE_JS.lock().unwrap() = o.probe.clone();

    if let Some(dir) = &o.cache {
        std::fs::create_dir_all(dir).ok();
    }

    let mut app = TabsApp::new(RefCell::new(None));
    let settings = Settings {
        no_sandbox: if o.no_sandbox { 1 } else { 0 },
        external_message_pump: if o.multi_threaded { 0 } else { 1 },
        multi_threaded_message_loop: if o.multi_threaded { 1 } else { 0 },
        windowless_rendering_enabled: 0,
        log_severity: LogSeverity::WARNING,
        cache_path: o
            .cache
            .as_ref()
            .map(|d| CefString::from(d.as_str()))
            .unwrap_or_default(),
        root_cache_path: o
            .cache
            .as_ref()
            .map(|d| CefString::from(d.as_str()))
            .unwrap_or_default(),
        ..Default::default()
    };

    assert_eq!(
        initialize(Some(main_args), Some(&settings), Some(&mut app), std::ptr::null_mut()),
        1,
        "cef initialize failed"
    );

    let started = Instant::now();
    let mut contexts: Vec<RequestContext> = Vec::new();
    // `TabsClient::new()` yields the wrapper `Client`; CEF refcounts it, but the
    // handlers stay alive only as long as something holds it, so keep them.
    let mut client_slots: Vec<Client> = Vec::new();

    for i in 0..o.tabs {
        let url = if o.urls.is_empty() {
            format!("{}{}/", o.url_base, 8901 + i)
        } else {
            o.urls[i % o.urls.len()].clone()
        };
        let mut client = TabsClient::new();

        let window_info = WindowInfo {
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        };

        #[cfg(target_os = "windows")]
        let window_info = {
            let mut wi = window_info.set_as_popup(Default::default(), "cef-tabs");
            wi.bounds = Rect {
                x: ((i % 5) * 310) as _,
                y: ((i / 5) * 420) as _,
                width: 300,
                height: 400,
            };
            wi
        };

        let mut settings = BrowserSettings::default();
        settings.windowless_frame_rate = 0;

        let created = if o.mode == "contexts" {
            let cache = format!(
                "{}\\ctx-{}",
                o.cache.clone().unwrap_or_else(|| std::env::temp_dir().display().to_string()),
                i
            );
            std::fs::create_dir_all(&cache).ok();
            let rc_settings = RequestContextSettings {
                cache_path: CefString::from(cache.as_str()),
                ..Default::default()
            };
            let mut rc = request_context_create_context(Some(&rc_settings), None).unwrap();
            let r = browser_host_create_browser(
                Some(&window_info),
                Some(&mut client),
                Some(&CefString::from(url.as_str())),
                Some(&settings),
                None,
                Some(&mut rc),
            );
            contexts.push(rc);
            r
        } else {
            browser_host_create_browser(
                Some(&window_info),
                Some(&mut client),
                Some(&CefString::from(url.as_str())),
                Some(&settings),
                None,
                None,
            )
        };

        println!("[cef] create {i} -> {created}");
        client_slots.push(client);
    }

    // Startup cost = time until every browser object exists.
    //
    // `do_message_loop_work()` is illegal when CEF owns the loop (multi_threaded),
    // so the wait becomes a plain sleep in that mode.
    let create_deadline = Instant::now() + Duration::from_secs(60);
    while CREATED.load(Ordering::Relaxed) < o.tabs && Instant::now() < create_deadline {
        if !o.multi_threaded {
            do_message_loop_work();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let create_ms = started.elapsed().as_millis();

    let load_deadline = Instant::now() + Duration::from_secs(90);
    let mut timed_out = false;
    while LOADED.load(Ordering::Relaxed) < o.tabs && Instant::now() < load_deadline {
        if !o.multi_threaded {
            do_message_loop_work();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if LOADED.load(Ordering::Relaxed) < o.tabs {
        timed_out = true;
    }
    let load_ms = started.elapsed().as_millis();

    println!(
        "READY {{\"label\":\"{}\",\"mode\":\"{}\",\"tabs\":{},\"createMs\":{},\"loadMs\":{},\"loaded\":{},\"timedOut\":{},\"sandbox\":{},\"pid\":{}}}",
        o.label,
        o.mode,
        o.tabs,
        create_ms,
        load_ms,
        LOADED.load(Ordering::Relaxed),
        timed_out,
        !o.no_sandbox,
        std::process::id()
    );
    use std::io::Write;
    std::io::stdout().flush().ok();

    let idle_end = Instant::now() + Duration::from_secs(o.idle);
    if o.host_loop {
        // Tauri's shape: the main thread owns a window and pumps it. CEF, in
        // multi_threaded mode, has its own UI thread, so neither loop services
        // the other's queue.
        assert!(
            o.multi_threaded,
            "--host-loop requires --multi-threaded: do_message_loop_work() cannot \
             share a thread with GetMessage()"
        );
        #[cfg(target_os = "windows")]
        hostui::run(o.idle);
        #[cfg(not(target_os = "windows"))]
        std::thread::sleep(Duration::from_secs(o.idle));
    } else {
        while Instant::now() < idle_end {
            if !o.multi_threaded {
                do_message_loop_work();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    println!("DONE");
    std::io::stdout().flush().ok();

    drop(contexts);
    shutdown();
}
