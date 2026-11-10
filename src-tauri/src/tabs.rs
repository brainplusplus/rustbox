use crate::config::{App, Engine, ProfileMode};
use crate::paths;
use crate::settings::{LinkTarget, Resolved};
use std::collections::HashMap;
use std::sync::Mutex;
use tauri::{Manager, PhysicalPosition, PhysicalSize, WebviewWindow};
#[cfg(windows)]
use webview2_com::{
  Microsoft::Web::WebView2::Win32::ICoreWebView2,
  take_pwstr,
};

/// One webview window belonging to an app.
///
/// An app is no longer one window. It has a pinned first tab plus any number of
/// sub-tabs the user or a link opened, and popups that float over it. All of
/// them share the app's profile, so a login in one is a login in all of them —
/// which is the point of keeping them under the same app instead of adding
/// another workspace entry for the same site.
struct SubTab {
  /// Stable within the app: `main` for the pinned first tab, then `t2`, `t3`…
  id: String,
  label: String,
  window: WebviewWindow,
  /// The app's first tab. Cannot be closed: closing it would leave the app with
  /// no surface at all while its sidebar entry still claims it is open.
  pinned: bool,
  /// A popup keeps its own size and place instead of being stretched over the
  /// content rect, and is never the tab the address bar points at.
  detached: bool,
  /// Last URL and document title seen for this tab.
  ///
  /// Cached rather than read back from the window: `WebviewWindow::url()` and
  /// `title()` post to the main thread and wait for the reply, so calling them
  /// while holding the tab registry — which the tab strip and the tab-open path
  /// both do — would deadlock the moment the main thread wants the same lock.
  /// The navigation watcher already receives both values, so it records them.
  url: String,
  title: String,
}

/// Every window an app has open.
struct AppTabs {
  /// Id source for the next sub-tab. Never reused, so a closed tab's CDP buffers
  /// cannot be resurrected by a later tab that happens to land on its id.
  next: u32,
  /// The sub-tab currently stretched over the content rect.
  active: String,
  tabs: Vec<SubTab>,
}

impl AppTabs {
  fn new() -> Self {
    Self {
      next: 2,
      active: "main".to_string(),
      tabs: Vec::new(),
    }
  }

  /// Mint the next sub-tab id. `main` is reserved for the pinned first tab, so
  /// numbering starts at 2 — otherwise a second tab would collide with it.
  fn mint(&mut self) -> String {
    let id = format!("t{}", self.next);
    self.next += 1;
    id
  }

  fn get(&self, tab_id: &str) -> Option<&SubTab> {
    self.tabs.iter().find(|t| t.id == tab_id)
  }

  fn active_tab(&self) -> Option<&SubTab> {
    self.get(&self.active).or_else(|| self.tabs.first())
  }
}

/// app id -> its open windows
static OPEN_TABS: Mutex<Option<HashMap<String, AppTabs>>> = Mutex::new(None);

/// Last known content rect (logical CSS px, relative to the main webview viewport),
/// reported by the UI. Used to re-place tab overlays when the main window moves.
static CONTENT_RECT: Mutex<(f64, f64, f64, f64)> = Mutex::new((0.0, 0.0, 0.0, 0.0));

/// The app whose tab is currently on screen.
///
/// Tracked here rather than in the UI because the MCP bridge has to enforce the
/// "active tab only" scope in the backend. A restriction that the page could
/// lift by lying about which tab is active would not be a restriction.
static ACTIVE_APP: Mutex<Option<String>> = Mutex::new(None);

/// Record which tab is on screen. Called by the UI when it activates a tab.
#[tauri::command]
pub fn set_active_tab(app_id: Option<String>) -> Result<(), String> {
  *ACTIVE_APP.lock().map_err(|e| e.to_string())? = app_id;
  Ok(())
}

/// The app id of the tab currently on screen, if any.
pub fn active_app() -> Option<String> {
  ACTIVE_APP.lock().ok().and_then(|guard| guard.clone())
}

/// Bring an open tab to the front, so an agent can switch what the user sees.
///
/// Refuses when the tab is not open rather than opening it: a request to show a
/// tab is not a request to start one, and silently launching a webview would
/// surprise the user with a new window they did not ask for.
#[tauri::command]
pub fn activate_app(
  app_handle: tauri::AppHandle,
  app_id: String,
  tab_id: Option<String>,
) -> Result<(), String> {
  let target = tabs(|m| {
    let a = m.get(&app_id)?;
    Some(match tab_id {
      Some(id) => a.get(&id).map(|t| (t.id.clone(), t.label.clone())),
      None => a.active_tab().map(|t| (t.id.clone(), t.label.clone())),
    })
  })
  .flatten()
  .ok_or_else(|| format!("app '{app_id}' has no open tab; open it first"))?;
  show_only(&app_handle, &app_id, &target.0);
  Ok(())
}

fn tabs<R>(f: impl FnOnce(&mut HashMap<String, AppTabs>) -> R) -> R {
  let mut guard = OPEN_TABS.lock().unwrap();
  f(guard.get_or_insert_with(HashMap::new))
}

/// Window label for a sub-tab. The first tab keeps the label it had before
/// sub-tabs existed, so an app's main window is still `tab-<session>`.
fn label_for(app: &App, tab_id: &str) -> String {
  let session = app.session.clone().unwrap_or_else(|| app.id.clone());
  if tab_id == "main" {
    format!("tab-{session}")
  } else {
    format!("tab-{session}-{tab_id}")
  }
}

/// The label of the sub-tab to act on: `tab_id` when given, else the app's
/// active one. `None` means the app has no window at all.
fn resolve_label(app_id: &str, tab_id: Option<&str>) -> Option<String> {
  tabs(|m| {
    let a = m.get(app_id)?;
    match tab_id {
      Some(id) => a.get(id).map(|t| t.label.clone()),
      None => a.active_tab().map(|t| t.label.clone()),
    }
  })
}


/// Resolve the proxy for `app` against the current global settings.
///
/// The app's index in the sidebar is passed through because a round-robin pool
/// hands members out in that order. A missing state (early startup) resolves to
/// direct rather than failing the open: a tab that refuses to appear because
/// settings could not be read is a worse outcome than one that connects
/// directly, and the settings page is reachable to fix it.
fn resolve_for(handle: &tauri::AppHandle, app: &App) -> Resolved {
  use tauri::Manager;
  let state: tauri::State<crate::config::AppState> = handle.state();
  let Ok(cfg) = state.config.lock() else {
    return Resolved::Direct;
  };
  let index = cfg.apps.iter().position(|a| a.id == app.id).unwrap_or(0);
  crate::settings::resolve(&cfg.settings, app, index)
}

/// Content rect (logical CSS px) -> physical screen rect, relative to the main
/// window's client area (the main webview fills the client area exactly).
fn content_rect_physical(
  main: &WebviewWindow,
  x: f64,
  y: f64,
  width: f64,
  height: f64,
) -> Result<(i32, i32, u32, u32), String> {
  let scale = main.scale_factor().map_err(|e| e.to_string())?;
  let inner = main.inner_position().map_err(|e| e.to_string())?;
  Ok((
    inner.x + (x * scale).round() as i32,
    inner.y + (y * scale).round() as i32,
    (width * scale).round() as u32,
    (height * scale).round() as u32,
  ))
}

/// Emit navigation state to the UI. Shared by both platform backends so the
/// address bar sees one event shape regardless of engine.
///
/// `tabId` is carried because an app can now have several tabs: without it the
/// address bar of the visible tab would be overwritten by whichever background
/// tab happened to navigate last.
fn emit_nav(
  handle: &tauri::AppHandle,
  app_id: &str,
  tab_id: &str,
  url: &str,
  can_back: bool,
  can_forward: bool,
) {
  use tauri::Emitter;
  // Record before emitting: the strip and the snapshot both read these, and the
  // event may reach the UI before the next snapshot is taken.
  tabs(|m| {
    if let Some(a) = m.get_mut(app_id)
      && let Some(t) = a.tabs.iter_mut().find(|t| t.id == tab_id)
    {
      t.url = url.to_string();
    }
  });
  let _ = handle.emit(
    "tab-nav",
    serde_json::json!({
      "appId": app_id,
      "tabId": tab_id,
      "url": url,
      "canBack": can_back,
      "canForward": can_forward,
    }),
  );
}

/// Tell the UI a tab started or finished loading.
///
/// Separate from `tab-nav` because it fires on a different schedule: a reload
/// navigates to the same URL, so the URL never changes and `SourceChanged` can
/// stay silent. Without this event the reload button has no visible effect at
/// all, which makes a working reload look like a dead button.
#[cfg(windows)]
fn emit_loading(handle: &tauri::AppHandle, app_id: &str, tab_id: &str, loading: bool) {
  use tauri::Emitter;
  let _ = handle.emit(
    "tab-loading",
    serde_json::json!({ "appId": app_id, "tabId": tab_id, "loading": loading }),
  );
}

/// Record a tab's document title. The tab strip shows it, because an app's
/// sub-tabs are usually the same site on different pages and the URL alone does
/// not tell them apart.
fn emit_title(handle: &tauri::AppHandle, app_id: &str, tab_id: &str, title: &str) {
  use tauri::Emitter;
  tabs(|m| {
    if let Some(a) = m.get_mut(app_id)
      && let Some(t) = a.tabs.iter_mut().find(|t| t.id == tab_id)
    {
      t.title = title.to_string();
    }
  });
  let _ = handle.emit(
    "tab-title",
    serde_json::json!({ "appId": app_id, "tabId": tab_id, "title": title }),
  );
}

/// Emit the tab's live navigation state (url + history availability) to the
/// main UI so the address bar and the back/forward buttons follow the tab the
/// way a real browser's chrome does.
#[cfg(windows)]
fn emit_nav_state(handle: &tauri::AppHandle, app_id: &str, tab_id: &str, core: &ICoreWebView2) {
  let mut src = windows_core::PWSTR::null();
  if unsafe { core.Source(&mut src) }.is_err() {
    return;
  }
  let url = take_pwstr(src);
  let mut back = windows_core::BOOL(0);
  let mut fwd = windows_core::BOOL(0);
  unsafe {
    let _ = core.CanGoBack(&mut back);
    let _ = core.CanGoForward(&mut fwd);
  }
  emit_nav(handle, app_id, tab_id, &url, back.as_bool(), fwd.as_bool());
}

/// Watch a tab's WebView2 for URL / history changes and relay them to the UI.
/// `SourceChanged` covers in-page and full navigations, `HistoryChanged` covers
/// pushState + back/forward availability, `NavigationCompleted` catches the
/// first load (no `SourceChanged` fires before a document exists).
///
/// `NavigationStarting`/`NavigationCompleted` also drive the toolbar's loading
/// state. A reload is the reason: it navigates to the URL the tab already has,
/// so every other event here stays quiet and the button looked dead.
#[cfg(windows)]
fn watch_tab_navigation(
  win: &WebviewWindow,
  app_id: String,
  tab_id: String,
  handle: tauri::AppHandle,
) {
  let _ = win.with_webview(move |platform| {
    use webview2_com::{
      HistoryChangedEventHandler, NavigationCompletedEventHandler, NavigationStartingEventHandler,
      SourceChangedEventHandler,
    };
    let core = match unsafe { platform.controller().CoreWebView2() } {
      Ok(core) => core,
      Err(_) => return,
    };

    let hs = handle.clone();
    let ids = app_id.clone();
    let ts = tab_id.clone();
    let start = NavigationStartingEventHandler::create(Box::new(move |_core, _args| {
      emit_loading(&hs, &ids, &ts, true);
      Ok(())
    }));

    let h1 = handle.clone();
    let id1 = app_id.clone();
    let t1 = tab_id.clone();
    let src = SourceChangedEventHandler::create(Box::new(move |core, _args| {
      if let Some(core) = core {
        emit_nav_state(&h1, &id1, &t1, &core);
      }
      Ok(())
    }));

    let h2 = handle.clone();
    let id2 = app_id.clone();
    let t2 = tab_id.clone();
    let hist = HistoryChangedEventHandler::create(Box::new(move |core, _args| {
      if let Some(core) = core {
        emit_nav_state(&h2, &id2, &t2, &core);
      }
      Ok(())
    }));

    let h3 = handle.clone();
    let id3 = app_id.clone();
    let t3 = tab_id.clone();
    let done = NavigationCompletedEventHandler::create(Box::new(move |core, _args| {
      emit_loading(&h3, &id3, &t3, false);
      if let Some(core) = core {
        emit_nav_state(&h3, &id3, &t3, &core);
      }
      Ok(())
    }));

    unsafe {
      let mut token = 0i64;
      let _ = core.add_NavigationStarting(&start, &mut token);
      let mut token = 0i64;
      let _ = core.add_SourceChanged(&src, &mut token);
      let mut token = 0i64;
      let _ = core.add_HistoryChanged(&hist, &mut token);
      let mut token = 0i64;
      let _ = core.add_NavigationCompleted(&done, &mut token);
    }

    // Report the state we already have so the bar is right before the first event.
    emit_nav_state(&handle, &app_id, &tab_id, &core);
  });
}

/// Non-Windows navigation watcher.
///
/// WebKit (WKWebView / WebKitGTK) exposes no equivalent of WebView2's
/// `SourceChanged` / `HistoryChanged` events through Tauri, so the URL is
/// polled instead. Polling is not elegant, but it is the only portable option
/// short of taking a direct dependency on the platform webview crates, and it
/// keeps the address bar honest for SPA navigations that never fire a load.
///
/// `canBack` / `canForward` stay false: WebKit history state is not reachable
/// from here, so the buttons stay disabled rather than lying about availability.
#[cfg(not(windows))]
fn watch_tab_navigation(
  win: &WebviewWindow,
  app_id: String,
  tab_id: String,
  handle: tauri::AppHandle,
) {
  let win = win.clone();
  std::thread::spawn(move || {
    let mut last = String::new();
    loop {
      // Stop once the tab is gone, otherwise the thread outlives the window.
      if tabs(|m| !m.get(&app_id).is_some_and(|a| a.get(&tab_id).is_some())) {
        return;
      }
      let asked = std::time::Instant::now();
      let got = win.url();
      if trace_focus() {
        // How long the main thread took to answer. A long wait here, while the
        // user is switching apps, is the evidence this loop is the bottleneck.
        shell_log(&format!(
          "trace url tab={tab_id} waited_ms={} ok={}",
          asked.elapsed().as_millis(),
          got.is_ok()
        ));
      }
      if let Ok(url) = got {
        let url = url.to_string();
        if url != last {
          last = url.clone();
          emit_nav(&handle, &app_id, &tab_id, &url, false, false);
        }
      }
      std::thread::sleep(std::time::Duration::from_millis(700));
    }
  });
}

/// Show the sub-tab `tab_id` of `app_id`, hide every other tab, and record which
/// app is on screen (that record is what the MCP "active tab only" scope is
/// enforced against).
///
/// Popups are windows, not pages: they are left exactly as they are, because a
/// popup the user dragged somewhere is theirs, and switching tabs is not a
/// request to move or hide it.
fn show_only(handle: &tauri::AppHandle, app_id: &str, tab_id: &str) {
  let all: Vec<(String, String, bool, WebviewWindow)> = tabs(|m| {
    m.iter()
      .flat_map(|(id, a)| {
        a.tabs.iter().map(move |t| {
          (
            id.clone(),
            t.id.clone(),
            t.detached,
            t.window.clone(),
          )
        })
      })
      .collect()
  });
  for (id, tid, detached, window) in all {
    if detached {
      // A popup belongs to its app: it stays put while that app's tabs are
      // switched, and goes with the app when another app comes to the front.
      // Leaving it up would float one app's window over a different app's page.
      if id != app_id {
        let _ = window.hide();
      }
      continue;
    }
    if id == app_id && tid == tab_id {
      let _ = window.show();
      // Owned windows at the same rect keep creation z-order; show() alone
      // leaves the first-created tab covering the others. Raise the active one.
      let _ = window.set_focus();
    } else {
      let _ = window.hide();
    }
  }
  if let Ok(mut guard) = ACTIVE_APP.lock() {
    *guard = Some(app_id.to_string());
  }
  // Remember which sub-tab is on screen, so a command that names only the app
  // (the MCP bridge, the address bar, back/forward) acts on what is actually
  // visible rather than on whatever was active last time.
  tabs(|m| {
    if let Some(a) = m.get_mut(app_id) {
      a.active = tab_id.to_string();
    }
  });
  let _ = handle;
}

/// Re-place every open tab overlay over the current content rect.
/// Called when the main window moves/resizes and when the UI reports a new rect.
pub fn sync_all_tabs(handle: &tauri::AppHandle) {
  let Some(main) = handle.get_webview_window("main") else {
    return;
  };
  let (x, y, width, height) = *CONTENT_RECT.lock().unwrap();
  let Ok((px, py, pw, ph)) = content_rect_physical(&main, x, y, width, height) else {
    return;
  };
  let all: Vec<WebviewWindow> = tabs(|m| {
    m.values()
      .flat_map(|a| a.tabs.iter())
      .filter(|t| !t.detached)
      .map(|t| t.window.clone())
      .collect()
  });
  for window in all {
    let _ = window.set_position(PhysicalPosition::new(px, py));
    let _ = window.set_size(PhysicalSize::new(pw, ph));
  }
}

/// What a queued open is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenKind {
  /// The app's pinned first tab.
  Main,
  /// An additional sub-tab inside the app.
  Sub,
  /// A detached popup that keeps its own size and position.
  Popup,
}

/// One queued tab open: the app, which kind of window, the URL to load (the
/// app's start page when `None`), and the content rect it was requested for.
///
/// A named type rather than a five-tuple because the four floats are
/// indistinguishable at the call site, and mixing up `width` and `height` there
/// is a bug that renders as a mis-sized window rather than an error.
///
/// `Clone` because the runtime-swap pre-flight has to park the request while the
/// user decides, and still build it if they say yes.
#[derive(Clone)]
struct PendingOpen {
  app: App,
  kind: OpenKind,
  url: Option<String>,
  rect: (f64, f64, f64, f64),
  /// Build the window without showing it and without hiding the tab that is
  /// already on screen.
  ///
  /// Startup preloading needs this: it opens every app at once, and the naive
  /// path would show each one in turn — nine webviews flashing over each other
  /// before the remembered tab finally settles on top.
  warm: bool,
}

/// Pending tab-open requests queued by `open_embedded_tab`. WebView2 window
/// creation DEADLOCKS when performed directly inside an IPC command handler
/// (wry#583 reentrancy: the controller-creation completion callback never
/// fires because the main thread is busy inside the handler). The command
/// only enqueues here; `PENDING_OPENS` is drained on the main thread from a
/// deferred `run_on_main_thread` task (safe context, same as app setup).
static PENDING_OPENS: Mutex<Vec<PendingOpen>> = Mutex::new(Vec::new());

/// The open the runtime-swap prompt is holding back.
///
/// Kept so the user's answer can act on the exact request that was refused,
/// instead of re-deriving it from the app id and guessing.
static BLOCKED_OPEN: Mutex<Option<PendingOpen>> = Mutex::new(None);

/// Set while replaying a parked open, so the pre-flight lets it through.
///
/// A one-shot flag rather than a parameter because the check sits in
/// `build_tab_window`, which every open path funnels through; threading an
/// "already asked" argument through all of them would put the decision in nine
/// places instead of one.
static SWAP_BYPASS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Drain queued tab opens. Must run on the main thread OUTSIDE any IPC
/// handler (we get there by scheduling this as a fresh main-thread task
/// after the handler returns).
fn drain_pending_opens(handle: &tauri::AppHandle) {
  loop {
    let mut next = PENDING_OPENS.lock().unwrap();
    if next.is_empty() {
      return;
    }
    let pending = next.remove(0);
    drop(next);
    build_tab_window(handle, pending);
  }
}

/// Queue a window build and schedule the drain.
///
/// The IPC handler runs ON the main thread, so `run_on_main_thread` from there
/// would execute inline and WebView2 creation would deadlock (wry#583). The only
/// context proven to work is a detached thread like the startup test hook: spawn
/// one, defer briefly so the handler returns, then drain on the main thread.
fn schedule_open(handle: &tauri::AppHandle, pending: PendingOpen) {
  PENDING_OPENS.lock().unwrap().push(pending);
  let h = handle.clone();
  std::thread::spawn(move || {
    std::thread::sleep(std::time::Duration::from_millis(120));
    let h2 = h.clone();
    let _ = h.run_on_main_thread(move || {
      drain_pending_opens(&h2);
    });
  });
}


/// Whether an open request should be held back and asked about.
///
/// Pure so the cases can be tested without an `AppHandle`: these inputs are the
/// whole decision, and getting any of them wrong either blocks a working tab (a
/// false alarm the user cannot explain) or lets a doomed build run.
fn hold_for_skew(reveals_existing: bool, bypass: bool, warm: bool, skew_present: bool) -> bool {
  // Revealing an app that is already open creates no webview, so it cannot be
  // affected by a swap and must never prompt.
  if reveals_existing {
    return false;
  }
  // The user was already asked and said to go ahead.
  if bypass {
    return false;
  }
  // A warm build is speculative: it exists so the first click on an app feels
  // instant, and nobody asked for it. Prompting here would fire once per app at
  // startup, and the prompt that survived would name an app the user never
  // clicked while "Open anyway" opened that same unwanted tab. The app they do
  // click arrives through the non-warm path and asks then.
  if warm {
    return false;
  }
  skew_present
}

fn build_tab_window(handle: &tauri::AppHandle, pending: PendingOpen) {
  let PendingOpen {
    app,
    kind,
    url: requested,
    rect: (x, y, width, height),
    warm,
  } = pending.clone();
  if app.engine != Engine::OsWebview || !app.enabled {
    return;
  }

  let app_id = app.id.clone();
  if kind != OpenKind::Popup {
    *CONTENT_RECT.lock().unwrap() = (x, y, width, height);
  }

  // Ask before building when the runtime was swapped under this process.
  //
  // The build would fail with a bare `HRESULT(0x80010108)` that says nothing
  // about the real cause, so the check runs *before* the attempt and the request
  // is parked rather than dropped. The prompt names the app the user clicked and
  // offers the two answers that exist: restart now, or try this tab anyway.
  //
  // Nothing restarts on its own. The user may be mid-task in another app, and a
  // window that vanishes and reappears by itself reads as a crash.
  //
  // Revealing an app that is already open creates no webview, so it is exempt:
  // that path keeps working during a swap and prompting there would be noise.
  // Placed before the tab id is minted, so a parked open cannot burn a sub-tab
  // number that never became a window.
  let reveals_existing = kind == OpenKind::Main && tabs(|m| m.contains_key(&app_id));
  let bypass = SWAP_BYPASS.swap(false, std::sync::atomic::Ordering::SeqCst);
  // Reading three directory entries once per open is negligible, and fetching it
  // unconditionally keeps `hold_for_skew` the single authority on the decision.
  let skew = crate::webview2_skew::current_skew();
  if hold_for_skew(reveals_existing, bypass, warm, skew.is_some())
    && let Some(skew) = skew
  {
    // The log gets the sentence, not just the versions: whoever reads it later
    // needs to know why the tab was withheld, not only which versions were seen.
    shell_log(&format!("tab for {app_id} not built: {}", skew.explanation()));
    park_blocked_open(pending.clone());
    emit_runtime_swap(handle, &app_id, &skew);
    return;
  }

  // Decide the tab id and label.
  //
  // `Main` is idempotent: asking for the app's first tab when it already exists
  // just shows it. `Sub` always mints a new id, because two clicks on the same
  // link must produce two tabs the way a browser does.
  let (tab_id, label) = match kind {
    OpenKind::Main => {
      // Reopening an app restores the tab it was last showing, not always its
      // first one: an app with three tabs left open on tab 3 must come back to
      // tab 3, or the user's place is lost every time they visit the catalog.
      let existing = tabs(|m| m.get(&app_id).map(|a| a.active.clone()));
      if let Some(id) = existing {
        // A warm open of an app that is already built is a no-op: showing it
        // would steal the screen from the tab the user is on.
        if !warm {
          show_only(handle, &app_id, &id);
          sync_all_tabs(handle);
        }
        emit_tabs_changed(handle);
        return;
      }
      ("main".to_string(), label_for(&app, "main"))
    }
    OpenKind::Sub => {
      let id = tabs(|m| m.get_mut(&app_id).map(|a| a.mint()));
      let Some(id) = id else {
        emit_tab_error(handle, "cannot add a tab to an app that is not open");
        return;
      };
      let label = label_for(&app, &id);
      (id, label)
    }
    OpenKind::Popup => {
      let id = tabs(|m| m.get_mut(&app_id).map(|a| a.mint()));
      let Some(id) = id else {
        emit_tab_error(handle, "cannot open a popup for an app that is not open");
        return;
      };
      let label = format!("popup-{}-{id}", app.session.clone().unwrap_or_else(|| app_id.clone()));
      (id, label)
    }
  };

  let Some(main) = handle.get_webview_window("main") else {
    emit_tab_error(handle, "main window missing");
    return;
  };
  let Ok((px, py, pw, ph)) = content_rect_physical(&main, x, y, width, height) else {
    emit_tab_error(handle, "could not resolve the content rect");
    return;
  };

  let target_url = requested.unwrap_or_else(|| app.url.clone());
  let url: tauri::Url = match target_url.parse() {
    Ok(u) => u,
    Err(e) => {
      emit_tab_error(handle, &format!("invalid URL {target_url}: {e}"));
      return;
    }
  };

  let scale = main.scale_factor().unwrap_or(1.0);
  // A popup is a floating window, not a page-sized one: it keeps its own size
  // and sits centred over the app's content area instead of covering it. Sized
  // here rather than from the request's `window_features` because a page may ask
  // for 100x100 or for the whole screen, and neither is what a user wants to
  // land on; a fixed, sane box is the predictable answer.
  let (px, py, pw, ph) = if kind == OpenKind::Popup {
    const POPUP_W: f64 = 560.0;
    const POPUP_H: f64 = 680.0;
    let w = (POPUP_W * scale).round() as u32;
    let h = (POPUP_H * scale).round() as u32;
    // Centre over the content rect, then clamp inside the monitor so a popup
    // opened on a small window is still reachable.
    let cx = px + (pw as i32 - w as i32) / 2;
    let cy = py + (ph as i32 - h as i32) / 2;
    let (mon_x, mon_y, mon_w, mon_h) = monitor_bounds(&main);
    let cx = cx.max(mon_x + 8).min(mon_x + mon_w - w as i32 - 8);
    let cy = cy.max(mon_y + 8).min(mon_y + mon_h - h as i32 - 8);
    (cx, cy, w, h)
  } else {
    (px, py, pw, ph)
  };
  // Popups get a real title bar and a resize grip: they are windows the user
  // moves and closes, unlike a tab, which is pinned to the content rect and has
  // no chrome of its own. They stay out of the taskbar so they read as part of
  // the app rather than as a second program.
  let detached = kind == OpenKind::Popup;

  let ua = app.user_agent.clone();

  // Per-app proxy for the embedded tab.
  //
  // The decision itself is made by `settings::resolve`, which applies the
  // app-level mode (inherit / none / custom) against the global setting. Keeping
  // it there is what stops this call site and the external-browser one from
  // disagreeing about which proxy an app uses.
  //
  // Resolved *before* the builder because the proxy has to travel in the browser
  // argument string too: setting `additional_browser_args` replaces the string
  // wry would otherwise have built, and wry only appends `--proxy-server` while
  // building its own default. Passing just `proxy_url` would leave the app
  // looking proxied while it connected directly.
  //
  // `Isolated` gives every app its own data directory, so each environment is
  // created once and never has to agree with another. `Light` deliberately
  // shares one environment with the shell, and WebView2 refuses a webview whose
  // options differ from those already in use for the same user data folder
  // (HRESULT 0x8007139F) — so a proxy there is reported instead of silently
  // doing nothing.
  // Two channels carry the same proxy, on purpose:
  //
  //   `proxy_url`    the portable one. It is the only one macOS and Linux have,
  //                  because `additional_browser_args` is Windows-only there.
  //   `--proxy-server` in the argument string  the one that actually applies on
  //                  Windows, because setting that string replaces the default
  //                  wry would otherwise have appended the proxy to. Without it
  //                  the app would look proxied and connect directly.
  let mut proxy_uri: Option<String> = None;
  let mut proxy_url: Option<tauri::Url> = None;
  match resolve_for(handle, &app) {
    Resolved::Direct => {}
    Resolved::PoolUnavailable(reason) => emit_tab_error(
      handle,
      &format!(
        "{}: the proxy pool could not supply a proxy ({reason}), so this tab \
         would connect directly. Fix the pool in Settings, or set this app to \
         \"no proxy\" to silence this.",
        app.name
      ),
    ),
    Resolved::Use(proxy) => {
      if app.profile_mode == ProfileMode::Light {
        emit_tab_error(
          handle,
          &format!(
            "{}: a proxy needs the Isolated storage mode. Light shares one browser \
             environment with the shell, so the proxy was not applied.",
            app.name
          ),
        );
      } else {
        let uri = crate::external::resolve_proxy_uri(&proxy);
        match tauri::Url::parse(&uri) {
          Ok(url) => {
            proxy_uri = Some(uri);
            proxy_url = Some(url);
          }
          Err(e) => emit_tab_error(
            handle,
            &format!("{}: invalid proxy address ({e})", app.name),
          ),
        }
      }
    }
  }

  let mut builder = tauri::WebviewWindowBuilder::new(
    handle,
    &label,
    tauri::WebviewUrl::External(url),
  )
  .title(format!("{} - RustBox", app.name))
  // Every RustBox webview carries the same switches, plus this app's proxy when
  // it has one. The switches are what keep a hidden tab alive: without them
  // WebView2 backgrounds the renderer, and a mail or chat app stops servicing
  // its own connection and shows "trying to reconnect" until it is reloaded.
  .additional_browser_args(&crate::webview_args::with_proxy(proxy_uri.as_deref()))
  // Tauri's own drag-drop handler registers an OLE drop target on the window,
  // and on Windows that swallows the page's HTML5 drag-and-drop: WebView2's
  // controller is told `SetAllowExternalDrop(false)` and in-page drags stop
  // working. A web app that lets you drag an attachment into a folder or a card
  // across a board needs those, so the handler is off here. RustBox never reads
  // `DragDropEvent`, so nothing is lost.
  .disable_drag_drop_handler()
  .decorations(detached)
  .skip_taskbar(true)
  .shadow(detached)
  .resizable(detached)
  .maximizable(false)
  .minimizable(false)
  .inner_size(pw as f64 / scale, ph as f64 / scale)
  // explicit position: owned windows must not use CW_USEDEFAULT
  .position(px as f64 / scale, py as f64 / scale)
  // A warm tab is built hidden but still loads: the window is hidden, the
  // webview inside it is not, so the page starts fetching immediately and the
  // first click on the app shows content instead of a blank frame.
  .visible(!warm)
  // The tab strip labels each sub-tab with the page's own title. `WebviewWindow`
  // has no title getter that is cheap enough to call from the strip, so the
  // title is recorded as it changes.
  .on_document_title_changed({
    let h = handle.clone();
    let a = app_id.clone();
    let t = tab_id.clone();
    move |_win, title| emit_title(&h, &a, &t, &title)
  });

  // Per-app storage isolation, two modes:
  //
  // `Isolated` gives the app its own webview data store: cookies, localStorage,
  // IndexedDB, cache and the GPU/crash domain are private to that app.
  //
  // `Light` keeps the shared environment and selects a named WebView2 profile
  // instead: site data is still isolated per app, but the runtime, GPU process
  // and crash domain are shared, so it is much cheaper. Windows only.
  //
  // How "its own data store" is expressed differs per platform:
  //
  //   Windows  `data_directory` -> a whole separate WebView2 environment
  //   Linux    `data_directory` -> separate WebKitGTK WebsiteDataManager
  //   macOS    `data_store_identifier` -> a named WKWebsiteDataStore, because
  //            WKWebView ignores `data_directory` entirely
  //
  // Setting a data directory used to deadlock because `create_environment` runs
  // a nested message pump and wry re-entered WebView2 creation when the call
  // came from an IPC handler (wry#583). `open_embedded_tab` defers every build
  // to a detached thread + main-thread drain, which is the context that works,
  // so the old "do NOT set data_directory" warning no longer applies.
  match app.profile_mode {
    ProfileMode::Isolated => {
      #[cfg(target_os = "macos")]
      {
        // WKWebView has no per-webview data directory, so the identifier is the
        // only way to keep one app's session out of another's. Requires macOS
        // 14+; on older systems wry falls back to the shared default store, so
        // apps would share a session there. `platform_info` reports this so the
        // UI can say so instead of silently pretending isolation happened.
        builder = builder.data_store_identifier(paths::data_store_identifier(&app_id));
      }
      #[cfg(not(target_os = "macos"))]
      {
        builder = builder.data_directory(paths::ensure(paths::embedded_profile(&app_id)));
      }
    }
    ProfileMode::Light => {
      #[cfg(windows)]
      {
        // The profile name deliberately does NOT travel through
        // `additional_browser_args`: WebView2 requires every webview that
        // shares a user data folder to be created with identical environment
        // options, so a tab carrying extra args fails against the shell window
        // with HRESULT(0x8007139F). The patched tauri-runtime-wry reads the
        // name from a label-keyed registry and applies it as a controller
        // option instead (see src-tauri/Cargo.toml).
        tauri_runtime_wry::set_webview_profile_name(&label, Some(paths::profile_name(&app_id)));
      }
      #[cfg(not(windows))]
      {
        // WebView2 multi-profile has no equivalent in WKWebView or WebKitGTK,
        // so Light degrades to the `Isolated` path rather than silently sharing
        // one profile between every app.
        #[cfg(target_os = "macos")]
        {
          builder = builder.data_store_identifier(paths::data_store_identifier(&app_id));
        }
        #[cfg(not(target_os = "macos"))]
        {
          builder = builder.data_directory(paths::ensure(paths::embedded_profile(&app_id)));
        }
      }
    }
  }

  if let Some(ua) = ua {
    builder = builder.user_agent(&ua);
  }
  // The portable half of the proxy setting: Windows reads the argument string
  // above, every other platform reads this one. Setting both keeps one code
  // path honest on all three; on Windows the argument string wins and this is
  // simply inert.
  if let Some(url) = proxy_url {
    builder = builder.proxy_url(url);
  }
  // `owner` is Windows-only (HWND ownership): it keeps the tab above the main
  // window and hides it when the main window is minimized. macOS/Linux use
  // `parent`, which gives the same "child of the main window" relationship.
  #[cfg(windows)]
  let builder = match builder.owner(&main) {
    Ok(b) => b,
    Err(e) => {
      emit_tab_error(handle, &format!("could not attach the tab window: {e}"));
      return;
    }
  };
  #[cfg(not(windows))]
  let builder = match builder.parent(&main) {
    Ok(b) => b,
    Err(e) => {
      emit_tab_error(handle, &format!("could not attach the tab window: {e}"));
      return;
    }
  };

  // A link that asks for a new window (`target="_blank"`, `window.open`) is a
  // request WebView2 drops on the floor when nobody answers it: with no handler
  // installed it calls `SetHandled(true)` and opens nothing, so the click looks
  // dead. That was the whole "links inside the browser do nothing" bug.
  //
  // Every answer is `Deny` first: that only stops WebView2 from making its own
  // window, and RustBox then does what the user picked. `Allow` would hand the
  // window to wry, which builds it in the shared environment rather than the
  // app's partition, so a link that needs the app's login would land on a login
  // wall — the one outcome the app tabs exist to avoid.
  //
  // The policy is read from the live config on every click rather than captured
  // when the tab was built, so a change in Settings takes effect without
  // reopening the tab.
  let link_app = app.clone();
  let link_tab_id = tab_id.clone();
  let link_handle = handle.clone();
  let builder = builder.on_new_window(move |url, _features| {
    use tauri::webview::NewWindowResponse;
    // A non-web scheme (`mailto:`, `tel:`) has no page to show: sending it to
    // the OS is the only answer that does anything, and a chooser would be a
    // dialog with one real option.
    if !crate::external::is_web_scheme(url.as_str()) {
      if let Err(e) = crate::external::open_in_default_browser(url.as_str()) {
        emit_tab_error(&link_handle, &format!("could not open the link: {e}"));
      }
      return NewWindowResponse::Deny;
    }
    match current_link_target(&link_handle, &link_app) {
      LinkTarget::Ask => {
        // Park the request and let the user decide. Nothing is opened while the
        // chooser is up, so dismissing it behaves like closing a popup.
        show_link_chooser(&link_handle, &link_app.id, &link_tab_id, url.as_str());
      }
      other => perform_link_action(
        &link_handle,
        &link_app.id,
        &link_tab_id,
        url.as_str(),
        other,
      ),
    }
    NewWindowResponse::Deny
  });

  match builder.build() {
    Ok(win) => {
      let _ = win.set_position(PhysicalPosition::new(px, py));
      let _ = win.set_size(PhysicalSize::new(pw, ph));
      tabs(|m| {
        let a = m.entry(app_id.clone()).or_insert_with(AppTabs::new);
        a.tabs.push(SubTab {
          id: tab_id.clone(),
          label: label.clone(),
          window: win.clone(),
          pinned: tab_id == "main",
          detached,
          url: target_url.clone(),
          title: app.name.clone(),
        });
        if !detached {
          a.active = tab_id.clone();
        }
      });
      watch_tab_navigation(&win, app_id.clone(), tab_id.clone(), handle.clone());
      // CDP state is keyed by the tab's window label, not the app id, so each
      // sub-tab has its own console and network buffers. Sharing one buffer per
      // app was wrong for the same reason it would be wrong in a browser: two
      // pages produce two different streams, and merging them makes an agent's
      // "read the console of this tab" answer a mixture of tabs it never asked
      // about. The label is unique per sub-tab and survives as the stable key
      // that `tab_console` / `tab_network` resolve from (appId, tabId).
      // Receivers are cheap and idle; the CDP domains that actually produce
      // events stay off until capture is switched on, so a normal session pays
      // nothing for this.
      if crate::cdp::mark_subscribed(&label) {
        crate::cdp::subscribe(&win, label.clone());
      }
      // A tab opened while capture is already on (i.e. the MCP bridge is
      // running) must start emitting immediately, otherwise an agent sees an
      // empty console for every tab it did not itself open.
      if crate::cdp::capture_enabled() {
        crate::cdp::enable_domains(&win);
      }
      // A popup is a second surface on top of the tab the user is reading, not a
      // replacement for it, so it is shown without switching the app's tab.
      //
      // A warm tab stays hidden: startup preloading builds it so the first
      // click on that app shows a loaded page, and showing it here would flash
      // every app over the one the user is actually looking at.
      if detached {
        let _ = win.show();
        let _ = win.set_focus();
      } else if !warm {
        show_only(handle, &app_id, &tab_id);
      }
      // The window can be closed by the OS (a popup's own title bar) as well as
      // by `close_sub_tab`, and both have to leave the same state behind.
      let cleanup_handle = handle.clone();
      let cleanup_app = app_id.clone();
      let cleanup_tab = tab_id.clone();
      let cleanup_label = label.clone();
      win.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
          let left = tabs(|m| {
            let Some(a) = m.get_mut(&cleanup_app) else {
              return 0;
            };
            a.tabs.retain(|t| t.label != cleanup_label);
            let n = a.tabs.len();
            if a.active == cleanup_tab
              && let Some(next) = a.tabs.first().map(|t| t.id.clone())
            {
              a.active = next;
            }
            n
          });
          // This window's own buffers go with it. Per tab, not per app: the
          // remaining sub-tabs of the same app still have live receivers and
          // still need their own streams.
          crate::cdp::detach(&cleanup_label);
          crate::cdp::forget_logs(&cleanup_label);
          crate::cdp::mark_unsubscribed(&cleanup_label);
          // Same reason as `close_sub_tab`: a question asked by this tab has no
          // window left to answer it in.
          prune_pending_links(Some(&cleanup_app), Some(&cleanup_tab));
          if left == 0 {
            // The last window of the app is gone, so the app entry goes too.
            tabs(|m| {
              m.remove(&cleanup_app);
            });
            if let Ok(mut guard) = ACTIVE_APP.lock()
              && guard.as_deref() == Some(cleanup_app.as_str())
            {
              *guard = None;
            }
          }
          emit_tabs_changed(&cleanup_handle);
        }
      });
      emit_tabs_changed(handle);
    }
    Err(e) => {
      // The registration is consumed on success; drop it on failure so a later
      // retry with the same label does not reuse a stale name.
      #[cfg(windows)]
      tauri_runtime_wry::set_webview_profile_name(&label, None);
      // `RPC_E_DISCONNECTED` on webview creation has one dominant cause that is
      // not a bug in this app: Windows swapped the WebView2 runtime while this
      // process was running, so the COM objects it holds are gone. Say that,
      // with the versions, instead of handing the user a bare HRESULT — the fix
      // is a restart, and they cannot guess that from the error code.
      let message = match runtime_swap_explanation() {
        Some(why) => format!("could not open the tab: {why}"),
        None => format!("could not open the tab: {e}"),
      };
      shell_log(&format!("tab {label} for {app_id} failed: {message}"));
      emit_tab_error(handle, &message);
    }
  }
}

// ---------------------------------------------------------------------------
// Link target resolution
// ---------------------------------------------------------------------------

/// A new-window request that is waiting for the user to answer the chooser.
struct PendingLink {
  id: u64,
  app_id: String,
  /// The sub-tab the click came from: `Same` navigates that one, and a new tab
  /// is opened next to it.
  from_tab: String,
  url: String,
}

static PENDING_LINKS: Mutex<Vec<PendingLink>> = Mutex::new(Vec::new());
static NEXT_LINK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Ceiling on queued questions. A page can fire dozens in a loop, so the queue
/// is bounded; the oldest goes first, which matches "the newest click is the
/// one you meant".
const MAX_PENDING: usize = 8;

/// The window label for one chooser.
///
/// Per request, not fixed. A single shared label meant the second chooser was
/// created while the first one was still closing, and Tauri refuses a label that
/// is still registered — so the build failed, the failure was discarded, and the
/// click simply did nothing. Distinct labels let the replacement coexist with
/// the one on its way out.
fn choice_label(id: u64) -> String {
  format!("link-choice-{id}")
}

/// Drop parked questions that can no longer be answered.
///
/// Called when a tab or an app closes. Without it the bounded queue filled with
/// dead entries, and reaching the ceiling evicted the user's newest click — the
/// symptom being a link that stopped working only after the app had been used
/// for a while.
///
/// `tab_id` of `None` means the whole app went away; `app_id` of `None` prunes
/// nothing, which keeps a partial call from emptying the queue by accident.
fn prune_pending_links(app_id: Option<&str>, tab_id: Option<&str>) {
  let Some(app_id) = app_id else {
    return;
  };
  let mut queue = PENDING_LINKS.lock().unwrap();
  // With a tab named, only that tab's questions go; the app's other tabs still
  // have live windows to answer theirs. With no tab named the whole app left.
  queue.retain(|p| match tab_id {
    Some(tab) => p.app_id != app_id || p.from_tab != tab,
    None => p.app_id != app_id,
  });
}

/// The app's current link policy, read from the live config.
///
/// Read per click rather than captured at build time so a change in Settings
/// applies to tabs that are already open, which is what a user expects after
/// changing a setting.
fn current_link_target(handle: &tauri::AppHandle, app: &App) -> LinkTarget {
  app_by_id(handle, &app.id)
    .map(|a| a.link_target)
    .unwrap_or(app.link_target)
}

/// Park a new-window request and show the chooser.
///
/// The webview is denied its own window before this runs, so nothing is left
/// half-open while the dialog is up: if the user dismisses it, the click simply
/// did nothing, which is what closing a popup does in any browser.
fn show_link_chooser(handle: &tauri::AppHandle, app_id: &str, from_tab: &str, url: &str) {
  // A page can fire dozens of these in a loop; keep the queue bounded so a
  // hostile page cannot grow it without limit.
  let id = NEXT_LINK_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
  {
    let mut queue = PENDING_LINKS.lock().unwrap();
    if queue.len() >= MAX_PENDING {
      queue.remove(0);
    }
    queue.push(PendingLink {
      id,
      app_id: app_id.to_string(),
      from_tab: from_tab.to_string(),
      url: url.to_string(),
    });
  }
  open_choice_window(handle, id, app_id, url);
}

/// Open the chooser as a native window over the app's content area.
///
/// A dialog drawn inside the main window cannot be used here: the tab webviews
/// are separate OS windows stacked above it, so an HTML overlay would appear
/// underneath the very page the question is about. The window is created the
/// same deferred way the context menu is, for the same wry#583 reason.
fn open_choice_window(handle: &tauri::AppHandle, id: u64, app_id: &str, url: &str) {
  use tauri::Manager;
  // One chooser at a time: a second link click replaces the first question
  // rather than stacking dialogs the user has to dismiss one by one. Every
  // label starts with the same prefix, so this sweeps up any that are still
  // closing as well as the visible one.
  for (label, win) in handle.webview_windows() {
    if label.starts_with("link-choice") {
      let _ = win.close();
    }
  }
  let Some(main) = handle.get_webview_window("main") else {
    return;
  };
  let (x, y, w, h) = *CONTENT_RECT.lock().unwrap();
  let Ok((px, py, pw, ph)) = content_rect_physical(&main, x, y, w, h) else {
    return;
  };
  let scale = main.scale_factor().unwrap_or(1.0);
  let h0 = handle.clone();
  let app_id = app_id.to_string();
  let url = url.to_string();
  std::thread::spawn(move || {
    std::thread::sleep(std::time::Duration::from_millis(80));
    let h2 = h0.clone();
    let _ = h0.run_on_main_thread(move || {
      let Some(mwin) = h2.get_webview_window("main") else {
        return;
      };
      // The URL travels in the query string rather than through a second IPC
      // round trip: the page needs it before it can lay itself out, and an
      // async fetch would show the dialog with a blank link line first.
      let encoded: String = url
        .bytes()
        .map(|b| match b {
          b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
            (b as char).to_string()
          }
          other => format!("%{other:02X}"),
        })
        .collect();
      let page = format!("link-choice.html?id={id}&app={app_id}&url={encoded}");
      // The label is per request: a fixed one collided with the chooser that
      // was still closing, and the failed build then looked like a dead link.
      let label = choice_label(id);
      let builder = tauri::WebviewWindowBuilder::new(
        &h2,
        label.as_str(),
        tauri::WebviewUrl::App(page.into()),
      )
      .title("Open link")
      .decorations(false)
      .skip_taskbar(true)
      .shadow(false)
      .resizable(false)
      .maximizable(false)
      .minimizable(false)
      .focused(true)
      .transparent(true)
      // Modal in intent: a chooser that opens underneath another application
      // is a dialog the user never sees and a link that appears to do nothing.
      .always_on_top(true)
      .inner_size(w, h)
      // The chooser runs in the shell's own environment (it has no data
      // directory of its own), so its browser arguments have to match the
      // shell window byte for byte or WebView2 refuses to create it.
      .additional_browser_args(crate::webview_args::BASE)
      // Same rule as every other webview: Tauri's drag-drop handler replaces
      // WebView2's drop target, which is what breaks HTML5 drag and drop on
      // Windows. Nothing in RustBox reads `DragDropEvent`.
      .disable_drag_drop_handler()
      .position(px as f64 / scale, py as f64 / scale);
      #[cfg(windows)]
      let builder = match builder.owner(&mwin) {
        Ok(b) => b,
        Err(_) => return,
      };
      #[cfg(not(windows))]
      let builder = match builder.parent(&mwin) {
        Ok(b) => b,
        Err(_) => return,
      };
      // A build failure here used to be discarded, which turned into "the link
      // does nothing" with no trace anywhere. Report it: a chooser that never
      // appears is a bug, and the only way to tell it apart from a page that
      // opened nothing is to record the attempt.
      match builder.build() {
        Ok(win) => {
          let _ = win.set_position(PhysicalPosition::new(px, py));
          let _ = win.set_size(PhysicalSize::new(pw, ph));
          let _ = win.set_focus();
          // Paired with the failure line below so a repro can tell "the click
          // never reached the handler" apart from "the chooser failed".
          shell_log(&format!("link chooser {label} opened for {app_id}"));
        }
        Err(e) => {
          shell_log(&format!(
            "link chooser for {label} ({app_id}) failed to build, so the click did \
             nothing: {e}"
          ));
          purge_pending_link(id);
        }
      }
    });
  });
}

/// Answer a parked new-window request.
///
/// `choice` is one of `same`/`here`, `new-tab`, `popup`, `external`. `remember`
/// stores it as the app's default, so the chooser stops appearing for that app.
#[tauri::command]
pub fn resolve_link_choice(
  id: u64,
  choice: String,
  remember: Option<bool>,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  let pending = {
    let mut queue = PENDING_LINKS.lock().unwrap();
    let Some(pos) = queue.iter().position(|p| p.id == id) else {
      // The dialog can outlive its request (the tab closed, or a newer click
      // replaced the queue). Answering nothing is correct: there is no window
      // left to put the page in.
      return Ok(());
    };
    queue.remove(pos)
  };
  let target = LinkTarget::parse(&choice)
    .filter(|t| *t != LinkTarget::Ask)
    .ok_or_else(|| format!("'{choice}' is not a way to open a link"))?;

  if remember.unwrap_or(false) {
    // Persisted, so the answer survives a restart. Done before the window is
    // built: if the build fails, the choice the user just made is still theirs.
    remember_link_target(&app_handle, &pending.app_id, target);
  }

  perform_link_action(
    &app_handle,
    &pending.app_id,
    &pending.from_tab,
    &pending.url,
    target,
  );
  // The answer is delivered; the dialog has nothing left to say.
  close_choice_window(&app_handle);
  Ok(())
}

/// Close the chooser without acting on the link.
#[tauri::command]
pub fn dismiss_link_choice(id: Option<u64>, app_handle: tauri::AppHandle) -> Result<(), String> {
  if let Some(id) = id {
    let mut queue = PENDING_LINKS.lock().unwrap();
    queue.retain(|p| p.id != id);
  }
  close_choice_window(&app_handle);
  Ok(())
}

/// Append one line to the shell log.
///
/// Release builds are `windows_subsystem = "windows"`, so `eprintln!` goes
/// nowhere and a swallowed failure stays invisible. Anything the user can
/// interpret as "the app stopped working" needs a line here instead.
fn shell_log(message: &str) {
  use std::io::Write as _;
  let dir = crate::paths::root().join("logs");
  let _ = std::fs::create_dir_all(&dir);
  if let Ok(mut file) = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(dir.join("shell.log"))
  {
    let _ = writeln!(file, "{message}");
  }
}

/// Focus and navigation tracing for diagnosing a hang, off unless asked for.
///
/// Set `RB_TRACE_FOCUS=1` before launching. Read once, so the hot polling loop
/// does not touch the environment on every tick.
pub(crate) fn trace_focus() -> bool {
  static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
  *ON.get_or_init(|| std::env::var("RB_TRACE_FOCUS").as_deref() == Ok("1"))
}

/// Write one trace line to `logs/shell.log`. Callers check `trace_focus()` first.
pub(crate) fn trace_line(message: &str) {
  shell_log(message);
}

/// Why a webview could not be created, when the cause is a WebView2 runtime swap.
///
/// Kept as a thin wrapper so the call sites read as "is this the runtime-swap
/// case?" and the version comparison lives in one tested place.
fn runtime_swap_explanation() -> Option<String> {
  crate::webview2_skew::runtime_swap_explanation()
}

/// Tear down every chooser window that is up.
///
/// The label carries the request id now, so this matches by prefix rather than
/// looking up one fixed name — that way a chooser still on its way out is
/// swept up too, instead of blocking the next one.
fn close_choice_window(handle: &tauri::AppHandle) {
  use tauri::Manager;
  for (label, win) in handle.webview_windows() {
    if label.starts_with("link-choice") {
      let _ = win.close();
    }
  }
}

/// Forget one parked question by id.
///
/// Used when the chooser never made it on screen: the window is gone, so the
/// request can never be answered, and leaving it queued would take a slot that
/// a later click needs.
fn purge_pending_link(id: u64) {
  PENDING_LINKS.lock().unwrap().retain(|p| p.id != id);
}

/// Store a link target on the app and persist the workspace.
fn remember_link_target(handle: &tauri::AppHandle, app_id: &str, target: LinkTarget) {
  use tauri::Manager;
  let state: tauri::State<crate::config::AppState> = handle.state();
  let Ok(mut cfg) = state.config.lock() else {
    return;
  };
  let Some(app) = cfg.apps.iter_mut().find(|a| a.id == app_id) else {
    return;
  };
  if app.link_target == target {
    return;
  }
  app.link_target = target;
  // A failed write must not silently discard the choice the user just made in
  // memory, so the error is reported and the in-memory value is kept.
  if let Err(e) = cfg.save(&state.config_path) {
    drop(cfg);
    emit_tab_error(handle, &format!("could not save the link preference: {e}"));
  }
}

/// Set an app's link-handling default from outside the UI (MCP, chat tools).
///
/// Same effect as the right-click menu's radio group: the value is stored and
/// the sidebar re-renders from the `apps-changed` event, so an agent changing it
/// is visible to the user instead of silently disagreeing with the panel.
pub fn set_link_target(
  app_id: &str,
  target: LinkTarget,
  handle: &tauri::AppHandle,
) -> Result<(), String> {
  use tauri::{Emitter, Manager};
  let state: tauri::State<crate::config::AppState> = handle.state();
  let apps = {
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    let app = cfg
      .apps
      .iter_mut()
      .find(|a| a.id == app_id)
      .ok_or_else(|| format!("unknown app '{app_id}'"))?;
    app.link_target = target;
    cfg.save(&state.config_path)?;
    cfg.apps.clone()
  };
  let _ = handle.emit("apps-changed", serde_json::to_value(&apps).unwrap_or_default());
  Ok(())
}

/// Perform one of the four answers.
///
/// `NewTab` and `Popup` both build a window with the app's storage partition and
/// the app's proxy, so the login carries over. That is the difference between
/// them and the OS browser, and the reason the chooser offers them.
fn perform_link_action(
  handle: &tauri::AppHandle,
  app_id: &str,
  from_tab: &str,
  url: &str,
  target: LinkTarget,
) {
  match target {
    LinkTarget::External => {
      // Nothing to do in the tab: the link leaves the app entirely. The launch
      // is fire-and-forget, so a browser that fails to start reports through the
      // error strip instead of silently eating the click.
      if let Err(e) = crate::external::open_in_default_browser(url) {
        emit_tab_error(handle, &format!("could not open the link: {e}"));
      }
    }
    LinkTarget::Same => {
      // Navigating from inside WebView2's new-window dispatch re-enters the
      // browser before it has finished answering, so it is deferred to a fresh
      // main-thread task.
      let Some(label) = resolve_label(app_id, Some(from_tab)) else {
        return;
      };
      let h = handle.clone();
      let url = url.to_string();
      std::thread::spawn(move || {
        let h2 = h.clone();
        let _ = h.run_on_main_thread(move || {
          if let Some(win) = h2.get_webview_window(&label) {
            let _ = navigate_window(&win, &url);
          }
        });
      });
    }
    LinkTarget::NewTab | LinkTarget::Popup => {
      let Some(app) = app_by_id(handle, app_id) else {
        emit_tab_error(handle, "the app is gone; could not open the link");
        return;
      };
      let kind = if target == LinkTarget::Popup {
        OpenKind::Popup
      } else {
        OpenKind::Sub
      };
      // Built from the current content rect, so a new tab lands exactly over the
      // one it was opened from.
      let rect = *CONTENT_RECT.lock().unwrap();
      schedule_open(
        handle,
        PendingOpen {
          app,
          kind,
          url: Some(url.to_string()),
          rect,
          warm: false,
        },
      );
    }
    LinkTarget::Ask => {}
  }
}

/// Read an app out of the live config.
fn app_by_id(handle: &tauri::AppHandle, app_id: &str) -> Option<App> {
  use tauri::Manager;
  let state: tauri::State<crate::config::AppState> = handle.state();
  let cfg = state.config.lock().ok()?;
  cfg.apps.iter().find(|a| a.id == app_id).cloned()
}

/// Tell the UI the set of sub-tabs changed, so the tab strip can redraw.
///
/// Sent as a whole snapshot rather than as add/close events: the strip is small,
/// and a snapshot cannot drift out of sync with the backend the way an
/// incremental stream can when one event is dropped.
fn emit_tabs_changed(handle: &tauri::AppHandle) {
  use tauri::Emitter;
  let _ = handle.emit("tabs-changed", tabs_snapshot());
}

/// One sub-tab as the tab strip sees it. A struct rather than a tuple: eight
/// positional fields, four of them `String`, is the kind of signature where a
/// swap is invisible at the call site and shows up as a strip labelled with the
/// wrong thing.
struct StripRow {
  app_id: String,
  tab_id: String,
  app_active: String,
  url: String,
  title: String,
  pinned: bool,
  popup: bool,
  active: bool,
}

/// Every open sub-tab, grouped by app.
///
/// The URLs and titles come from the registry's cache rather than from the
/// windows: `WebviewWindow::url()` / `title()` post to the main thread and wait
/// for the reply, and this runs while the caller holds no lock but the main
/// thread may be inside a handler that wants the same one. The navigation
/// watcher records both as they change, so the cache is always current.
fn tabs_snapshot() -> serde_json::Value {
  let rows: Vec<StripRow> = tabs(|m| {
    m.iter()
      .flat_map(|(id, a)| {
        let active = a.active.clone();
        a.tabs.iter().map(move |t| StripRow {
          app_id: id.clone(),
          tab_id: t.id.clone(),
          app_active: active.clone(),
          url: t.url.clone(),
          title: t.title.clone(),
          pinned: t.pinned,
          popup: t.detached,
          active: t.id == active,
        })
      })
      .collect()
  });
  let mut by_app: std::collections::BTreeMap<String, (String, Vec<serde_json::Value>)> =
    std::collections::BTreeMap::new();
  for row in rows {
    let entry = by_app
      .entry(row.app_id)
      .or_insert_with(|| (row.app_active, Vec::new()));
    entry.1.push(serde_json::json!({
      "id": row.tab_id,
      "url": row.url,
      "title": row.title,
      "pinned": row.pinned,
      "popup": row.popup,
      "active": row.active,
    }));
  }
  let out: Vec<serde_json::Value> = by_app
    .into_iter()
    .map(|(app_id, (active, tabs))| {
      serde_json::json!({ "appId": app_id, "active": active, "tabs": tabs })
    })
    .collect();
  serde_json::Value::Array(out)
}

/// The tab strip's current state, for the UI to render on demand.
#[tauri::command]
pub fn list_sub_tabs() -> Result<serde_json::Value, String> {
  Ok(tabs_snapshot())
}

/// Open a new sub-tab in an app, optionally at `url`.
#[tauri::command]
pub fn add_sub_tab(
  app_id: String,
  url: Option<String>,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  let app = app_by_id(&app_handle, &app_id).ok_or_else(|| format!("unknown app '{app_id}'"))?;
  schedule_sub_tab(app_handle, app, url)
}

/// Queue a sub-tab open for an app that was already resolved.
///
/// Split out so the MCP bridge can open a tab without going through the command
/// wrapper, which needs a `tauri::State` the bridge does not hold.
pub fn schedule_sub_tab(
  app_handle: tauri::AppHandle,
  app: App,
  url: Option<String>,
) -> Result<(), String> {
  let rect = *CONTENT_RECT.lock().unwrap();
  schedule_open(
    &app_handle,
    PendingOpen {
      app,
      kind: OpenKind::Sub,
      url,
      rect,
      warm: false,
    },
  );
  Ok(())
}

/// Show one of the app's sub-tabs.
#[tauri::command]
pub fn activate_sub_tab(
  app_id: String,
  tab_id: String,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  show_only(&app_handle, &app_id, &tab_id);
  emit_tabs_changed(&app_handle);
  Ok(())
}

/// Move one sub-tab next to another, the way dragging a chip does.
///
/// `after` decides which side of `target_id` the moved tab lands on, so dropping
/// on the right half of a chip moves it right. The pinned first tab is refused
/// as a *subject*: it is the app itself, and letting it slide down the strip
/// would mean the app's own tab is no longer the first thing in its row. It is
/// still a valid target, because "move this tab in front of the app tab" is the
/// only way to put a tab at the start.
///
/// The order in `AppTabs::tabs` *is* the strip order, so this is a splice and
/// not a stored index — two sources of truth for one order is how they drift.
#[tauri::command]
pub fn reorder_sub_tab(
  app_id: String,
  tab_id: String,
  target_id: String,
  after: bool,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  let moved = tabs(|m| {
    let a = m.get_mut(&app_id)?;
    let from = a.tabs.iter().position(|t| t.id == tab_id)?;
    if a.tabs[from].pinned {
      return None;
    }
    if tab_id == target_id {
      return None;
    }
    let to = a.tabs.iter().position(|t| t.id == target_id)?;
    let tab = a.tabs.remove(from);
    let insert = reorder_index(from, to, after, a.tabs.len());
    a.tabs.insert(insert, tab);
    Some(())
  });
  match moved {
    Some(()) => {
      // The strip is rebuilt from this snapshot, so the new order is the only
      // thing the UI has to react to.
      emit_tabs_changed(&app_handle);
      Ok(())
    }
    None => Err("that tab cannot be moved".to_string()),
  }
}

/// Where a tab removed from `from` belongs, given a target measured beforehand.
///
/// Split out and tested on its own because it is pure index arithmetic, and the
/// off-by-one it hides is invisible in a strip until a chip lands one slot away
/// from where it was dropped.
///
/// `len` is the length *after* the removal. A target below the source has
/// already shifted up by one, so the insertion point is one lower than the raw
/// index; `after` then adds one back to land on the far side. The result is
/// clamped to at least 1, which is what keeps the pinned tab first.
fn reorder_index(from: usize, to: usize, after: bool, len: usize) -> usize {
  let base = if to > from { to - 1 } else { to };
  let insert = if after { base + 1 } else { base };
  insert.max(1).min(len)
}

/// Close one sub-tab.
///
/// Refuses the pinned first tab. That tab is the app: closing it would leave the
/// sidebar entry pointing at a window that no longer exists, and the only way
/// back would be to close and reopen the app — so the refusal is reported rather
/// than silently ignored.
#[tauri::command]
pub fn close_sub_tab(
  app_id: String,
  tab_id: String,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  let removed = tabs(|m| {
    let a = m.get_mut(&app_id)?;
    let pos = a.tabs.iter().position(|t| t.id == tab_id)?;
    if a.tabs[pos].pinned {
      return None;
    }
    let tab = a.tabs.remove(pos);
    // Whatever was on screen is gone; fall back to the nearest neighbour, the
    // way a browser does, rather than leaving the app blank.
    if a.active == tab_id {
      a.active = a
        .tabs
        .get(pos.saturating_sub(1))
        .or_else(|| a.tabs.first())
        .map(|t| t.id.clone())
        .unwrap_or_else(|| "main".to_string());
    }
    let active = a.active.clone();
    let empty = a.tabs.is_empty();
    Some((tab, active, empty))
  });
  let Some((tab, active, empty)) = removed else {
    // Distinguish "pinned" from "not found" so the caller can say which.
    let pinned = tabs(|m| {
      m.get(&app_id)
        .and_then(|a| a.get(&tab_id))
        .map(|t| t.pinned)
        .unwrap_or(false)
    });
    return Err(if pinned {
      format!("'{tab_id}' is the app's main tab and cannot be closed")
    } else {
      format!("no tab '{tab_id}' in app '{app_id}'")
    });
  };
  if let Some(win) = app_handle.get_webview_window(&tab.label) {
    let _ = win.close();
  }
  // A question this tab asked can no longer be answered. Dropping it keeps the
  // bounded queue clear for clicks that are still live.
  prune_pending_links(Some(&app_id), Some(&tab_id));
  // The window's own CDP buffers die with the window (the Destroyed handler
  // does the same for a window the OS closed). Here only the app entry is
  // removed, and only when this was the last tab.
  if empty {
    tabs(|m| {
      m.remove(&app_id);
    });
    if let Ok(mut guard) = ACTIVE_APP.lock()
      && guard.as_deref() == Some(app_id.as_str())
    {
      *guard = None;
    }
  } else {
    show_only(&app_handle, &app_id, &active);
  }
  emit_tabs_changed(&app_handle);
  Ok(())
}

/// Surface a tab-open failure in the UI. These used to be dropped silently,
/// which made a failing build look like a dead button.
fn emit_tab_error(handle: &tauri::AppHandle, message: &str) {
  use tauri::Emitter;
  eprintln!("[rustbox] tab error: {message}");
  let _ = handle.emit("tab-error", message.to_string());
}

/// Ask the user what to do about a WebView2 runtime swap.
///
/// Names the app the user just tried to open, because a prompt about a runtime
/// they never asked about reads as noise. Nothing disruptive happens until they
/// answer: the restart can only be whole-process, and a window that restarts
/// itself mid-task reads as a crash.
fn emit_runtime_swap(
  handle: &tauri::AppHandle,
  app_id: &str,
  skew: &crate::webview2_skew::Skew,
) {
  use tauri::Emitter;
  let name = app_name(handle, app_id).unwrap_or_else(|| app_id.to_string());
  let payload = serde_json::json!({
    "appId": app_id,
    "appName": name,
    "from": skew.from,
    "to": skew.to,
  });
  shell_log(&format!("runtime swap prompt for {app_id}"));
  let _ = handle.emit("runtime-swap", payload);
}

/// The user's display name for an app, for prompts that have to name it.
fn app_name(handle: &tauri::AppHandle, app_id: &str) -> Option<String> {
  use tauri::Manager;
  let state: tauri::State<crate::config::AppState> = handle.state();
  let cfg = state.config.lock().ok()?;
  cfg
    .apps
    .iter()
    .find(|a| a.id == app_id)
    .map(|a| a.name.clone())
}

/// Restart RustBox because Windows swapped the WebView2 runtime underneath it.
///
/// Whole-process on purpose, and the prompt says so. WebView2 runs one browser
/// process per user-data folder, and every RustBox app shares one folder (each
/// app gets a named profile *inside* it), so there is no per-app or per-tab
/// repair to offer — retiring one app's windows would leave the same dead
/// browser process serving the rest. Promising a narrower restart would be a
/// promise the runtime cannot keep.
///
/// Nothing calls this on its own. A window that restarts itself mid-task reads as
/// a crash, so the user answers first; open tabs are restored from the config on
/// the way back up, which is what makes accepting cheap.
#[tauri::command]
pub fn restart_after_runtime_swap(handle: tauri::AppHandle) {
  shell_log("restart confirmed by user after webview2 runtime swap");
  handle.restart();
}

/// Park a request while the user decides what to do about the swap.
///
/// Only the newest request is kept: the prompt names one app, so holding a queue
/// would mean the answer applied to a tab the user has already forgotten.
fn park_blocked_open(pending: PendingOpen) {
  *BLOCKED_OPEN.lock().unwrap() = Some(pending);
}

/// Take the parked request and arm the one-shot bypass for its replay.
///
/// Split out from the command so "exactly once" is testable. The bypass has to be
/// consumed by that one replay: leaving it armed would make the *next* open skip
/// the check even though the user only ever approved the parked one, and a tab
/// that silently fails again is worse than being asked twice.
fn take_blocked_open() -> Result<PendingOpen, String> {
  let parked = BLOCKED_OPEN
    .lock()
    .unwrap()
    .take()
    .ok_or_else(|| "there is no held-back tab to open".to_string())?;
  SWAP_BYPASS.store(true, std::sync::atomic::Ordering::SeqCst);
  Ok(parked)
}

/// Drop the parked request, because the user closed the prompt.
fn clear_blocked_open() {
  *BLOCKED_OPEN.lock().unwrap() = None;
  // Disarming matters as much as dropping the request: a bypass left set would
  // make the next open skip the check even though nothing was approved.
  SWAP_BYPASS.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// Build the tab that was held back, despite the detected runtime swap.
///
/// Exists because the detection is a filesystem heuristic and could be wrong — a
/// runtime folder can be rewritten without the running browser process caring.
/// Without this escape hatch a false positive would leave tabs unopenable until
/// the user restarted for no reason.
#[tauri::command]
pub fn open_blocked_tab(handle: tauri::AppHandle) -> Result<(), String> {
  let parked = take_blocked_open()?;
  shell_log(&format!(
    "user chose to open {} despite the detected runtime swap",
    parked.app.id
  ));
  schedule_open(&handle, parked);
  Ok(())
}

/// Forget the held-back tab because the user dismissed the prompt.
#[tauri::command]
pub fn dismiss_runtime_swap() {
  clear_blocked_open();
}


/// Dev hook behind `RB_PROBE_OPEN=1`: open every enabled embedded app so a
/// script can inspect the resulting WebView2 environments without driving the
/// UI with synthetic clicks. Inert unless the variable is set at startup.
pub fn probe_open_all(handle: &tauri::AppHandle) {
  use tauri::Manager;
  let state: tauri::State<crate::config::AppState> = handle.state();
  let apps = match state.config.lock() {
    Ok(cfg) => cfg.apps.clone(),
    Err(_) => return,
  };
  for app in apps {
    if app.engine != Engine::OsWebview || !app.enabled {
      continue;
    }
    PENDING_OPENS.lock().unwrap().push(PendingOpen {
      app,
      kind: OpenKind::Main,
      url: None,
      rect: (0.0, 60.0, 800.0, 600.0),
      warm: false,
    });
  }
  drain_pending_opens(handle);
}

/// Queue a tab open; actual window creation happens on the main thread in a
/// deferred task (see `drain_pending_opens`) to avoid the wry#583 deadlock.
///
/// ARCHITECTURE NOTE (why not `Window::add_child`): creating a WebView2 child
/// webview from a command / webview event handler deadlocks on Windows — the
/// controller-creation completion callback never fires and the main thread
/// pumps forever (upstream wry#583, WebView2 threading-model reentrancy).
/// Instead each tab is a borderless, taskbar-less window OWNED by the main
/// window (always above it, hides/minimizes with it) positioned exactly over
/// the content rect and kept in sync on move/resize.
///
/// Idempotent: opening an app that already has a window just shows it.
#[tauri::command]
pub fn open_embedded_tab(
  app: App,
  x: f64,
  y: f64,
  width: f64,
  height: f64,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  schedule_open(
    &app_handle,
    PendingOpen {
      app,
      kind: OpenKind::Main,
      url: None,
      rect: (x, y, width, height),
      warm: false,
    },
  );
  Ok(())
}

/// Build every enabled embedded app's tab in the background at launch.
///
/// This is what makes the workspace behave like Rambox instead of like a set of
/// bookmarks: each app is a live page from the first second, so its session,
/// unread count and push channel start working before the user clicks it.
/// Without it only the app that happens to be on screen is loaded, and clicking
/// another one starts a cold load — the "I have to click a tab before it
/// reloads" complaint.
///
/// The windows are built hidden and are never shown here, so preloading cannot
/// flash one app over the one the user is reading. A later
/// `open_embedded_tab` finds the window already there and just reveals it.
///
/// Runs from a detached thread and hops to the main thread for the same
/// wry#583 reason every other open path does, and paces the builds because
/// creating a WebView2 controller is synchronous on the main thread: doing nine
/// of them back to back would freeze the window for seconds.
#[tauri::command]
pub fn warm_start_apps(
  except_app_id: Option<String>,
  app_handle: tauri::AppHandle,
) -> Result<usize, String> {
  use tauri::Manager;
  let state: tauri::State<crate::config::AppState> = app_handle.state();
  let (enabled, apps) = {
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    let apps: Vec<App> = cfg
      .apps
      .iter()
      .filter(|a| {
        a.enabled
          && a.engine == Engine::OsWebview
          && Some(&a.id) != except_app_id.as_ref()
      })
      .cloned()
      .collect();
    (cfg.settings.startup.warm_start, apps)
  };
  if !enabled || apps.is_empty() {
    return Ok(0);
  }
  let count = apps.len();
  let rect = *CONTENT_RECT.lock().unwrap();
  let handle = app_handle.clone();
  std::thread::spawn(move || {
    for app in apps {
      // Stop preloading once the app is closing: a build that lands after the
      // windows are gone would just fail noisily.
      let still_open = handle.get_webview_window("main").is_some();
      if !still_open {
        return;
      }
      let h = handle.clone();
      let _ = handle.run_on_main_thread(move || {
        build_tab_window(
          &h,
          PendingOpen {
            app,
            kind: OpenKind::Main,
            url: None,
            rect,
            warm: true,
          },
        );
      });
      // The controller creation above is synchronous, so this gap is what keeps
      // the window responsive while a whole workspace comes up.
      std::thread::sleep(std::time::Duration::from_millis(450));
    }
  });
  Ok(count)
}

/// Navigate the app's tab: back / forward / reload.
///
/// On Windows this uses the WebView2 history API rather than `history.back()`
/// in JS: the JS call fails while a document is still loading and does nothing
/// when the engine navigated cross-origin, which made the buttons feel dead.
/// On macOS/Linux Tauri exposes no history API, so the page is asked to run
/// `history.back()` / `forward()` in the active document instead.
#[tauri::command]
pub fn navigate(
  app_id: String,
  action: String,
  tab_id: Option<String>,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  let Some(label) = resolve_label(&app_id, tab_id.as_deref()) else {
    return Ok(());
  };
  let Some(win) = app_handle.get_webview_window(&label) else {
    return Ok(());
  };

  #[cfg(windows)]
  {
    win
      .with_webview(move |platform| {
        let controller = platform.controller();
        let core = match unsafe { controller.CoreWebView2() } {
          Ok(core) => core,
          Err(_) => return,
        };
        unsafe {
          match action.as_str() {
            "back" => {
              let _ = core.GoBack();
            }
            "forward" => {
              let _ = core.GoForward();
            }
            "reload" => {
              let _ = core.Reload();
            }
            _ => {}
          }
        }
      })
      .map_err(|e| e.to_string())
  }

  #[cfg(not(windows))]
  {
    match action.as_str() {
      // `reload` maps to a native call that works everywhere.
      "reload" => win.reload().map_err(|e| e.to_string()),
      // History traversal has no native binding here; run it in the document.
      "back" => win
        .eval("history.back()")
        .map_err(|e| e.to_string()),
      "forward" => win
        .eval("history.forward()")
        .map_err(|e| e.to_string()),
      _ => Ok(()),
    }
  }
}

/// Navigate the app's tab to an arbitrary URL (address bar).
#[tauri::command]
pub fn navigate_to_url(
  app_id: String,
  url: String,
  tab_id: Option<String>,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  let Some(label) = resolve_label(&app_id, tab_id.as_deref()) else {
    return Ok(());
  };
  let Some(win) = app_handle.get_webview_window(&label) else {
    return Ok(());
  };
  navigate_window(&win, &url)
}

/// Navigate one tab window, normalising a bare host to `https://`.
///
/// Shared by the address bar and the MCP bridge so both accept `example.com`
/// and both reject a malformed URL the same way.
pub fn navigate_window(win: &WebviewWindow, url: &str) -> Result<(), String> {
  let parsed: tauri::Url = if url.contains("://") {
    url.parse().map_err(|e| format!("invalid url: {e}"))?
  } else {
    format!("https://{url}")
      .parse()
      .map_err(|e| format!("invalid url: {e}"))?
  };
  win.navigate(parsed).map_err(|e| e.to_string())
}

/// Mute/unmute the app's tab.
///
/// Windows uses `ICoreWebView2_8::SetIsMuted`. Tauri exposes no mute API for
/// WKWebView or WebKitGTK, so on those platforms this reports the limitation
/// instead of pretending the tab was muted.
#[tauri::command]
pub fn set_tab_muted(
  app_id: String,
  muted: bool,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  // Mute every window of the app, not just the visible tab: muting an app that
  // is playing audio in a background sub-tab is the request the menu makes.
  let labels: Vec<String> = tabs(|m| {
    m.get(&app_id)
      .map(|a| a.tabs.iter().map(|t| t.label.clone()).collect())
      .unwrap_or_default()
  });
  if labels.is_empty() {
    return Ok(());
  }

  #[cfg(windows)]
  {
    for label in labels {
      let Some(win) = app_handle.get_webview_window(&label) else {
        continue;
      };
      win
        .with_webview(move |platform| {
          use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_8;
          use windows_core::Interface;
          let controller = platform.controller();
          unsafe {
            if let Ok(core) = controller.CoreWebView2()
              && let Ok(web8) = core.cast::<ICoreWebView2_8>()
            {
              let _ = web8.SetIsMuted(muted);
            }
          }
        })
        .map_err(|e| e.to_string())?;
    }
    Ok(())
  }

  #[cfg(not(windows))]
  {
    let _ = (labels, muted);
    Err("muting a tab is not supported on this platform yet".to_string())
  }
}

/// Show the context menu as a small owned popup window so it can appear ABOVE
/// the native webview tab overlays (an HTML element in the main window can
/// never render above them). Loads `ctx.html?app=<id>[&tab=<id>]`; the frontend
/// renders the menu and emits `ctx-menu-action` events.
///
/// `tab_id` names the sub-tab that was right-clicked, so the menu can offer
/// tab-scoped rows ("Close tab") and know which chip it is talking about. It is
/// optional because the same menu is used from the sidebar, where no single tab
/// is implied.
///
/// The popup is created tall enough for the largest menu, then
/// [`ctx_menu_fit`] shrinks it to the height the page actually measured. Sizing
/// it here from a constant is what used to clip the last row: the menu grew a
/// `Reset session` entry and the constant did not.
#[tauri::command]
pub fn ctx_menu_window(
  app_id: String,
  x: i32,
  y: i32,
  tab_id: Option<String>,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  use tauri::Manager;
  let main = app_handle
    .get_webview_window("main")
    .ok_or("no main window")?;
  // Close any previous ctx popup (one at a time).
  if let Some(old) = app_handle.get_webview_window("ctx-menu") {
    let _ = old.close();
  }
  let scale = main.scale_factor().unwrap_or(1.0);
  // Menu width in CSS px; the height comes from the page once it has laid out.
  const MW_CSS: f64 = 196.0;
  // Taller than any menu this page can build (the link-handling group takes the
  // largest menu to ~13 rows, about 400px), so nothing is clipped between
  // creation and the fit call. The window is transparent and the menu is
  // top-anchored, so the slack is invisible and is removed a few milliseconds
  // later.
  const MH_MAX_CSS: f64 = 460.0;
  // `x`/`y` arrive as CSS px relative to the main window's client area, so the
  // client origin (inner_position, physical px) is the right anchor, and the
  // offsets must be scaled by the DPI factor before adding them.
  let inner = main.inner_position().map_err(|e| e.to_string())?;
  let sx = inner.x + (x as f64 * scale).round() as i32;
  let sy = inner.y + (y as f64 * scale).round() as i32;
  let (mon_x, mon_y, mon_w, mon_h) = monitor_bounds(&main);
  // Open to the right of / level with the cursor, like a native tray menu. The
  // vertical clamp is applied again in `ctx_menu_fit`, against the real height.
  let mw = (MW_CSS * scale).round() as i32;
  let px = (sx + 6).min(mon_x + mon_w - mw - 8).max(mon_x + 8);
  let py = (sy - 6).max(mon_y + 8);
  // Remember the cursor anchor: the fit step needs the same origin to re-clamp
  // vertically once the true height is known.
  *CTX_ANCHOR.lock().unwrap() = Some(CtxAnchor {
    cursor_y: sy,
    scale,
    width: mw,
    monitor: (mon_x, mon_y, mon_w, mon_h),
  });

  let h0 = app_handle.clone();
  let app_id0 = app_id.clone();
  let tab_id0 = tab_id.unwrap_or_default();
  let scale0 = scale;
  let px0 = px;
  let py0 = py;
  let main_h = h0.clone();
  std::thread::spawn(move || {
    std::thread::sleep(std::time::Duration::from_millis(120));
    let h2 = main_h.clone();
    let _ = main_h.run_on_main_thread(move || {
      let url = tauri::WebviewUrl::App(
        format!("ctx.html?app={app_id0}&tab={tab_id0}").into(),
      );
      let builder = tauri::WebviewWindowBuilder::new(&h2, "ctx-menu", url)
        .title("ctx-menu")
        .decorations(false)
        .skip_taskbar(true)
        .shadow(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .focused(true)
        .transparent(true)
        // Topmost, not just owned by the main window.
        //
        // Ownership keeps the popup above its owner and nothing else: if
        // another application is above RustBox at that moment, the menu opens
        // underneath it and every row appears dead. The menu is short-lived and
        // modal in intent, so it is the one window here that should win.
        .always_on_top(true)
        .inner_size(MW_CSS, MH_MAX_CSS)
        // Shares the shell's environment, so the browser arguments must match
        // it exactly (WebView2 rejects a mismatched second environment).
        .additional_browser_args(crate::webview_args::BASE)
        .disable_drag_drop_handler()
        .position(px0 as f64 / scale0, py0 as f64 / scale0);
      let Some(mwin) = h2.get_webview_window("main") else { return };
      #[cfg(windows)]
      let builder = match builder.owner(&mwin) {
        Ok(b) => b,
        Err(_) => return,
      };
      #[cfg(not(windows))]
      let builder = match builder.parent(&mwin) {
        Ok(b) => b,
        Err(_) => return,
      };
      if let Ok(win) = builder.build() {
        let _ = win.set_position(PhysicalPosition::new(px0, py0));
        let _ = win.set_focus();
      }
    });
  });
  Ok(())
}

/// Cursor anchor of the open ctx menu, so the popup can be resized to its
/// measured content and re-clamped vertically without drifting from the cursor.
#[derive(Clone, Copy)]
struct CtxAnchor {
  /// Cursor Y in physical screen px.
  cursor_y: i32,
  scale: f64,
  /// Menu width in physical px, fixed at creation.
  width: i32,
  /// `(x, y, width, height)` of the monitor, physical px.
  monitor: (i32, i32, i32, i32),
}

static CTX_ANCHOR: Mutex<Option<CtxAnchor>> = Mutex::new(None);

fn monitor_bounds(main: &WebviewWindow) -> (i32, i32, i32, i32) {
  main
    .current_monitor()
    .ok()
    .flatten()
    .map(|mm| {
      let pos = mm.position();
      let size = mm.size();
      (
        pos.x,
        pos.y,
        size.width as i32,
        size.height as i32,
      )
    })
    .unwrap_or((0, 0, 1920, 1080))
}

/// Resize the ctx menu to the height the page measured, and keep it on screen.
///
/// Called by `ctx.html` once the menu is laid out, because only the page knows
/// how many rows it built. Returns the applied height in CSS px.
#[tauri::command]
pub fn ctx_menu_fit(height: f64, app_handle: tauri::AppHandle) -> Result<f64, String> {
  use tauri::Manager;
  let Some(win) = app_handle.get_webview_window("ctx-menu") else {
    return Ok(0.0);
  };
  // Guard against a page that reports nonsense before it has laid out.
  let css = if height.is_finite() && height > 0.0 {
    height.min(600.0)
  } else {
    return Ok(0.0);
  };  let anchor = *CTX_ANCHOR.lock().unwrap();
  let Some(anchor) = anchor else { return Ok(0.0) };
  let scale = anchor.scale;
  let mw = anchor.width;
  let (mon_x, mon_y, mon_w, mon_h) = anchor.monitor;
  // Never taller than the monitor: a menu that runs off the top would be just as
  // unreachable as one clipped at the bottom.
  let max_css = ((mon_h as f64 / scale) - 16.0).max(120.0);
  let css = css.min(max_css);
  let mh = (css * scale).round() as i32;
  // Same rule as the initial placement, now with the real height: level with
  // the cursor, but never past the bottom of the monitor.
  let py = (anchor.cursor_y - 6)
    .min(mon_y + mon_h - mh - 8)
    .max(mon_y + 8);
  let px = win
    .outer_position()
    .map(|p| p.x)
    .unwrap_or(mon_x + 8)
    .min(mon_x + mon_w - mw - 8)
    .max(mon_x + 8);
  let _ = win.set_size(PhysicalSize::new(mw as u32, mh as u32));
  let _ = win.set_position(PhysicalPosition::new(px, py));
  Ok(css)
}

/// Close the ctx-menu popup window (called by the frontend after any action).
#[tauri::command]
pub fn ctx_menu_close(app_handle: tauri::AppHandle) -> Result<(), String> {
  use tauri::Manager;
  if let Some(win) = app_handle.get_webview_window("ctx-menu") {
    let _ = win.close();
  }
  Ok(())
}

/// Relay an action clicked in the ctx-menu popup to the main window.
#[tauri::command]
pub fn emit_ctx_action(
  app_id: String,
  action: String,
  tab_id: Option<String>,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  use tauri::Emitter;
  app_handle
    .emit(
      "ctx-action",
      serde_json::json!({ "appId": app_id, "action": action, "tabId": tab_id }),
    )
    .map_err(|e| e.to_string())
}

/// Hide every open tab (HTML overlays like the add/edit modal must be reachable).
#[tauri::command]
pub fn hide_all_tabs() -> Result<(), String> {
  let all: Vec<WebviewWindow> = tabs(|m| {
    m.values()
      .flat_map(|a| a.tabs.iter())
      .map(|t| t.window.clone())
      .collect()
  });
  for window in all {
    let _ = window.hide();
  }
  // Nothing is on screen, so no tab is "the active one". Leaving the last value
  // in place would let an active-tab-scoped agent keep acting on a tab the user
  // cannot see.
  if let Ok(mut guard) = ACTIVE_APP.lock() {
    *guard = None;
  }
  Ok(())
}

/// The UI reports a new content rect (window resized / layout changed).
#[tauri::command]
pub fn resize_tabs(
  x: f64,
  y: f64,
  width: f64,
  height: f64,
  app_handle: tauri::AppHandle,
) -> Result<(), String> {
  *CONTENT_RECT.lock().unwrap() = (x, y, width, height);
  sync_all_tabs(&app_handle);
  Ok(())
}

/// Close the app entirely: every sub-tab and popup it owns.
#[tauri::command]
pub fn close_embedded_tab(app_id: String, app_handle: tauri::AppHandle) -> Result<(), String> {
  let closed = tabs(|m| m.remove(&app_id));
  if let Some(app_tabs) = closed {
    for tab in app_tabs.tabs {
      // Each window drops its own CDP buffers in the Destroyed handler; the
      // labels are remembered here only so the loop can close the windows.
      let _ = app_handle
        .get_webview_window(&tab.label)
        .map(|w| w.close());
    }
    // Every question this app parked is unanswerable now.
    prune_pending_links(Some(&app_id), None);
  }
  // A closed tab cannot be the active one.
  if let Ok(mut guard) = ACTIVE_APP.lock()
    && guard.as_deref() == Some(app_id.as_str())
  {
    *guard = None;
  }
  emit_tabs_changed(&app_handle);
  Ok(())
}

/// Resolve one specific sub-tab's window.
pub fn tab_window_of(
  app_id: &str,
  tab_id: Option<&str>,
  app_handle: &tauri::AppHandle,
) -> Result<WebviewWindow, String> {
  let label = resolve_label(app_id, tab_id)
    .ok_or_else(|| format!("no open tab for '{app_id}'; open it first"))?;
  app_handle
    .get_webview_window(&label)
    .ok_or_else(|| format!("tab '{app_id}' has no window"))
}

/// Evaluate JS inside the app's tab and return the value.
///
/// Runs on the caller's thread (an IPC thread or the MCP bridge), never on the
/// main thread, because the CDP reply arrives through the main thread's message
/// pump. `#[tauri::command]` handlers run on the main thread on Windows, so the
/// work is handed to a spawned thread and joined — otherwise the reply would
/// wait behind the very call that is waiting for it.
#[tauri::command]
pub async fn tab_eval(
  app_id: String,
  expression: String,
  tab_id: Option<String>,
  app_handle: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
  tauri::async_runtime::spawn_blocking(move || {
    let win = tab_window_of(&app_id, tab_id.as_deref(), &app_handle)?;
    crate::cdp::evaluate(&win, &expression)
  })
  .await
  .map_err(|e| format!("evaluation task failed: {e}"))?
}

/// Send an arbitrary CDP command to the app's tab.
#[tauri::command]
pub async fn tab_cdp(
  app_id: String,
  method: String,
  params: Option<serde_json::Value>,
  tab_id: Option<String>,
  app_handle: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
  tauri::async_runtime::spawn_blocking(move || {
    let win = tab_window_of(&app_id, tab_id.as_deref(), &app_handle)?;
    crate::cdp::call(&win, &method, params.unwrap_or(serde_json::json!({})))
  })
  .await
  .map_err(|e| format!("CDP task failed: {e}"))?
}

/// The buffer key for one sub-tab: its window label.
///
/// Logs are keyed by window rather than by app so two sub-tabs of one app keep
/// separate streams. `tab_id` selects which one; `None` means the app's active
/// tab, which is what an agent that does not care about sub-tabs expects.
fn logs_key(app_id: &str, tab_id: Option<&str>) -> String {
  resolve_label(app_id, tab_id).unwrap_or_else(|| app_id.to_string())
}

/// Console entries for a tab, optionally only those after a cursor.
#[tauri::command]
pub fn tab_console(
  app_id: String,
  since: Option<u64>,
  limit: Option<usize>,
  tab_id: Option<String>,
) -> Result<serde_json::Value, String> {
  let logs = crate::cdp::logs_for(&logs_key(&app_id, tab_id.as_deref()));
  let buffer = logs.console.lock().map_err(|e| e.to_string())?;
  let entries: Vec<crate::cdp::ConsoleEntry> = match since {
    Some(s) => buffer.since(s).into_iter().cloned().collect(),
    None => buffer.tail(limit.unwrap_or(100)).into_iter().cloned().collect(),
  };
  Ok(serde_json::json!({
    "cursor": buffer.cursor(),
    "entries": entries,
  }))
}

/// Network requests for a tab, optionally only those after a cursor.
#[tauri::command]
pub fn tab_network(
  app_id: String,
  since: Option<u64>,
  limit: Option<usize>,
  tab_id: Option<String>,
) -> Result<serde_json::Value, String> {
  let logs = crate::cdp::logs_for(&logs_key(&app_id, tab_id.as_deref()));
  let buffer = logs.network.lock().map_err(|e| e.to_string())?;
  let entries: Vec<crate::cdp::NetworkEntry> = match since {
    Some(s) => buffer.since(s).into_iter().cloned().collect(),
    None => buffer.tail(limit.unwrap_or(100)).into_iter().cloned().collect(),
  };
  Ok(serde_json::json!({
    "cursor": buffer.cursor(),
    "entries": entries,
  }))
}

/// Enable console/network capture on every tab that is already open.
///
/// Called when the MCP bridge starts: tabs opened before that moment have
/// receivers installed but no CDP domains enabled, so they would stay silent.
pub fn enable_capture_for_open_tabs() {
  let windows: Vec<WebviewWindow> = tabs(|m| {
    m.values()
      .flat_map(|a| a.tabs.iter())
      .map(|t| t.window.clone())
      .collect()
  });
  for win in windows {
    crate::cdp::enable_domains(&win);
  }
}

/// Every open tab, with the URL the browser currently has.
///
/// One entry per app. Each entry names the app's active sub-tab and lists every
/// sub-tab it owns, so a caller can both do the simple thing (address the app,
/// get whatever is on screen) and the precise thing (address one sub-tab by
/// `tabId`). Reporting only the active tab made every sub-tab except the one on
/// screen invisible to an agent; reporting every sub-tab as its own top-level
/// row would make the same app appear several times and break callers that
/// treat `appId` as unique.
#[tauri::command]
pub fn list_tabs() -> Result<serde_json::Value, String> {
  struct Row {
    app_id: String,
    active: String,
    url: String,
    tabs: Vec<serde_json::Value>,
    console: (u64, usize),
    network: (u64, usize),
    window: WebviewWindow,
  }
  let rows: Vec<Row> = tabs(|m| {
    m.iter()
      // An app entry with no windows left is on its way out; reporting it would
      // advertise a tab nobody can address.
      .filter(|(_, a)| !a.tabs.is_empty())
      .map(|(id, a)| {
        let tabs = a
          .tabs
          .iter()
          .map(|t| {
            let logs = crate::cdp::logs_for(&t.label);
            let console = logs.console.lock().map(|b| (b.cursor(), b.len())).unwrap_or((0, 0));
            let network = logs.network.lock().map(|b| (b.cursor(), b.len())).unwrap_or((0, 0));
            serde_json::json!({
              "tabId": t.id,
              "label": t.label,
              "url": t.url,
              "title": t.title,
              "pinned": t.pinned,
              // A popup is a floating window over the page, not a page the user
              // can switch to; saying so keeps a caller from activating it.
              "popup": t.detached,
              "active": t.id == a.active,
              "console": { "cursor": console.0, "buffered": console.1 },
              "network": { "cursor": network.0, "buffered": network.1 },
            })
          })
          .collect();
        // `filter` above guarantees one exists.
        let shown = a.active_tab().unwrap_or(&a.tabs[0]);
        let logs = crate::cdp::logs_for(&shown.label);
        Row {
          app_id: id.clone(),
          active: a.active.clone(),
          url: shown.url.clone(),
          tabs,
          console: logs.console.lock().map(|b| (b.cursor(), b.len())).unwrap_or((0, 0)),
          network: logs.network.lock().map(|b| (b.cursor(), b.len())).unwrap_or((0, 0)),
          window: shown.window.clone(),
        }
      })
      .collect()
  });
  let out: Vec<serde_json::Value> = rows
    .into_iter()
    .map(|row| {
      // `visible` is a cheap window flag, not a webview round trip, so it is
      // safe to read here; the URL and title come from the cache for the reason
      // documented on `SubTab`.
      let visible = row.window.is_visible().unwrap_or(false);
      serde_json::json!({
        "appId": row.app_id,
        // The active sub-tab: what the user is looking at, and what every call
        // that omits `tabId` acts on.
        "tabId": row.active,
        "url": row.url,
        "visible": visible,
        "tabs": row.tabs.len(),
        "subTabs": row.tabs,
        // `cursor` is the total ever seen, `buffered` how many are still held.
        // When they differ, older entries have been dropped by the cap.
        "console": { "cursor": row.console.0, "buffered": row.console.1 },
        "network": { "cursor": row.network.0, "buffered": row.network.1 },
      })
    })
    .collect();
  Ok(serde_json::Value::Array(out))
}



#[cfg(test)]
mod reorder_tests {
  use super::reorder_index;

  /// The strip is [main, a, b, c] (pinned first). `len` is the length after the
  /// moved tab is taken out, so it is 3 for a four-tab strip.
  #[test]
  fn dropping_on_the_right_half_moves_a_tab_right() {
    // move `a` (1) onto the right half of `b` (2): expect [main, b, a, c]
    assert_eq!(reorder_index(1, 2, true, 3), 2);
  }

  #[test]
  fn dropping_on_the_left_half_moves_a_tab_left() {
    // move `c` (3) onto the left half of `b` (2): expect [main, a, c, b]
    assert_eq!(reorder_index(3, 2, false, 3), 2);
  }

  #[test]
  fn a_tab_dropped_before_the_pinned_tab_still_lands_after_it() {
    // The pinned tab is index 0; nothing may be inserted ahead of it.
    assert_eq!(reorder_index(2, 0, false, 3), 1);
  }

  #[test]
  fn dropping_past_the_last_tab_appends() {
    // move `a` (1) onto the right half of the last tab: clamp to the end.
    assert_eq!(reorder_index(1, 3, true, 3), 3);
  }

  #[test]
  fn a_two_tab_strip_can_only_swap() {
    // [main, a]: moving `a` before `main` still has to stay at index 1.
    assert_eq!(reorder_index(1, 0, false, 2), 1);
  }
}

#[cfg(test)]
mod link_choice_tests {
  use super::{prune_pending_links, choice_label, PendingLink, PENDING_LINKS, MAX_PENDING};

  fn link(id: u64, app: &str, tab: &str) -> PendingLink {
    PendingLink {
      id,
      app_id: app.to_string(),
      from_tab: tab.to_string(),
      url: format!("https://example.com/{id}"),
    }
  }

  fn reset(entries: Vec<PendingLink>) {
    let mut q = PENDING_LINKS.lock().unwrap();
    q.clear();
    q.extend(entries);
  }

  /// The queue behind these tests is a process-wide static, and `cargo test`
  /// runs them in parallel: without this lock one test's `reset` wipes another
  /// test's fixtures mid-assertion.
  static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

  fn ids() -> Vec<u64> {
    PENDING_LINKS.lock().unwrap().iter().map(|p| p.id).collect()
  }

  /// The chooser window is created with its own label. A fixed one made the
  /// second chooser collide with the first while that one was still closing,
  /// and Tauri rejects a label that is still registered — the build failed and
  /// the click looked dead. Labels must therefore differ per request.
  #[test]
  fn every_chooser_gets_its_own_window_label() {
    assert_ne!(choice_label(1), choice_label(2));
    assert_ne!(choice_label(7), choice_label(7 + 1));
    assert_eq!(choice_label(42), "link-choice-42");
  }

  /// Closing a tab or an app has to drop the questions that pointed at it.
  /// Leaving them behind filled the bounded queue with dead entries, and the
  /// ceiling then evicted the click the user had just made.
  #[test]
  fn closing_a_tab_drops_its_pending_questions_only() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset(vec![
      link(1, "app-a", "t2"),
      link(2, "app-a", "t3"),
      link(3, "app-b", "t2"),
    ]);
    prune_pending_links(Some("app-a"), Some("t2"));
    assert_eq!(ids(), vec![2, 3]);
  }

  #[test]
  fn closing_an_app_drops_every_question_it_asked() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset(vec![
      link(1, "app-a", "t2"),
      link(2, "app-a", "t3"),
      link(3, "app-b", "t2"),
    ]);
    prune_pending_links(Some("app-a"), None);
    assert_eq!(ids(), vec![3]);
  }

  /// After pruning, a fresh click must survive the ceiling: the queue should be
  /// holding live questions, not tombstones.
  #[test]
  fn a_new_click_survives_the_ceiling_after_pruning() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stale: Vec<PendingLink> = (0..MAX_PENDING as u64)
      .map(|i| link(i + 1, "app-closed", "t9"))
      .collect();
    reset(stale);
    prune_pending_links(Some("app-closed"), None);
    assert!(ids().is_empty());

    let mut q = PENDING_LINKS.lock().unwrap();
    q.push(link(99, "app-live", "t2"));
    assert_eq!(q.last().map(|p| p.id), Some(99));
  }

  /// A prune naming something the queue never heard of must not touch it.
  #[test]
  fn pruning_an_unrelated_tab_changes_nothing() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset(vec![link(1, "app-a", "t2"), link(2, "app-b", "t3")]);
    prune_pending_links(Some("app-z"), Some("t9"));
    assert_eq!(ids(), vec![1, 2]);
  }
}

#[cfg(test)]
mod runtime_swap_tests {
  use super::{
    clear_blocked_open, hold_for_skew, park_blocked_open, take_blocked_open, OpenKind,
    PendingOpen, BLOCKED_OPEN, SWAP_BYPASS,
  };
  use crate::config::{App, Engine, ProfileMode};

  /// `BLOCKED_OPEN` and `SWAP_BYPASS` are process-wide, and `cargo test` runs
  /// these in parallel: without a lock one test's reset disarms another's fixture
  /// mid-assertion.
  static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

  fn parked(kind: OpenKind) -> PendingOpen {
    PendingOpen {
      app: App {
        id: "app-test".to_string(),
        name: "Test".to_string(),
        url: "https://example.com".to_string(),
        icon: None,
        engine: Engine::OsWebview,
        external_browser_path: None,
        session: None,
        profile_mode: ProfileMode::Light,
        fingerprint: None,
        external_args: Vec::new(),
        proxy_mode: Default::default(),
        proxy: None,
        user_agent: None,
        link_target: Default::default(),
        enabled: true,
        muted: false,
        notifications: true,
      },
      kind,
      url: Some("https://example.com/sub".to_string()),
      rect: (0.0, 60.0, 800.0, 600.0),
      warm: false,
    }
  }

  /// The whole feature is a decision not to act, so the negative cases matter as
  /// much as the positive one: a prompt on every open would train the user to
  /// dismiss it, and a prompt on an app that is already open would be a lie.
  #[test]
  fn only_a_new_tab_during_a_swap_is_held_back() {
    assert!(
      hold_for_skew(false, false, false, true),
      "new tab, swap: hold"
    );
    assert!(
      !hold_for_skew(false, false, false, false),
      "no swap: never hold"
    );
    assert!(
      !hold_for_skew(true, false, false, true),
      "revealing an open app builds no webview: no prompt"
    );
    assert!(
      !hold_for_skew(false, true, false, true),
      "the user already said to go ahead"
    );
  }

  /// Preloading the workspace at startup must never raise the prompt. Twenty warm
  /// opens would otherwise stack twenty questions the user never asked, and the
  /// one left standing would name an app they never clicked — so "Open anyway"
  /// would open the wrong tab.
  #[test]
  fn preloading_the_workspace_never_prompts() {
    assert!(
      !hold_for_skew(false, false, true, true),
      "a warm build is speculative, not a request"
    );
  }

  /// Parking must not consume the bypass: the answer has not been given yet, so a
  /// flag set here would let the next open skip the check with nothing approved.
  #[test]
  fn parking_a_request_does_not_arm_the_bypass() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_blocked_open();
    park_blocked_open(parked(OpenKind::Main));
    assert!(BLOCKED_OPEN.lock().unwrap().is_some());
    assert!(
      !SWAP_BYPASS.load(std::sync::atomic::Ordering::SeqCst),
      "nothing was approved yet"
    );
    clear_blocked_open();
  }

  /// "Open anyway" replays the exact request the user made, so a sub-tab keeps
  /// its own URL instead of falling back to the app's start page.
  #[test]
  fn taking_the_parked_request_preserves_what_was_asked_for() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_blocked_open();
    park_blocked_open(parked(OpenKind::Sub));
    let taken = take_blocked_open().expect("the parked request");
    assert_eq!(taken.url.as_deref(), Some("https://example.com/sub"));
    assert_eq!(taken.app.id, "app-test");
    assert!(SWAP_BYPASS.load(std::sync::atomic::Ordering::SeqCst));
    clear_blocked_open();
  }

  /// The bypass is one-shot. If it survived, the next tab would skip detection and
  /// fail with the raw HRESULT instead of being asked about.
  #[test]
  fn the_bypass_covers_one_replay_only() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_blocked_open();
    park_blocked_open(parked(OpenKind::Main));
    take_blocked_open().expect("the parked request");
    // This is exactly what `build_tab_window` does on entry.
    let first = SWAP_BYPASS.swap(false, std::sync::atomic::Ordering::SeqCst);
    let second = SWAP_BYPASS.swap(false, std::sync::atomic::Ordering::SeqCst);
    assert!(first, "the replay is allowed through once");
    assert!(!second, "a later open is checked again");
    clear_blocked_open();
  }

  /// A second click on "open anyway" has nothing to act on, and must say so rather
  /// than build a tab from a stale request.
  #[test]
  fn taking_twice_fails_the_second_time() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_blocked_open();
    park_blocked_open(parked(OpenKind::Main));
    assert!(take_blocked_open().is_ok());
    assert!(take_blocked_open().is_err());
    clear_blocked_open();
  }

  /// Dismissing has to disarm the bypass too: otherwise closing the prompt would
  /// silently approve the next open, which is the opposite of dismissing.
  #[test]
  fn dismissing_drops_the_request_and_the_bypass() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    park_blocked_open(parked(OpenKind::Main));
    SWAP_BYPASS.store(true, std::sync::atomic::Ordering::SeqCst);
    clear_blocked_open();
    assert!(BLOCKED_OPEN.lock().unwrap().is_none());
    assert!(!SWAP_BYPASS.load(std::sync::atomic::Ordering::SeqCst));
  }
}
