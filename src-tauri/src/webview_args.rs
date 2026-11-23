//! How every RustBox webview is created: the browser switches it gets, and the
//! two settings that have to agree across all of them.
//!
//! # The switches
//!
//! RustBox keeps each app in its own OS window stacked over the same rectangle
//! and shows one at a time by hiding the others. Chromium treats every window it
//! did not paint as background: timers are clamped (Intensive Wake-Up Throttling
//! allows one wake-up a minute after five minutes hidden), the renderer is
//! deprioritised, and Windows reports a fully covered window as *occluded*,
//! which is backgrounded all the same.
//!
//! For a mail or chat app that is fatal in a quiet way: the page stops servicing
//! its own WebSocket and timers, Gmail shows "Trying to reconnect…", and the tab
//! only comes back after it is torn down and rebuilt — which is why disabling and
//! re-enabling the app used to be the only way out. These switches keep a hidden
//! tab running at full cadence instead.
//!
//! Two rules make this a single shared constant rather than three call sites:
//!
//! 1. **Setting `additional_browser_args` REPLACES wry's own default string**
//!    instead of extending it. wry's default is
//!    `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection` plus
//!    `--autoplay-policy=no-user-gesture-required` (its `autoplay` default) plus
//!    `--proxy-server=…` when one is set. All three have to be restated here or
//!    they silently switch off.
//! 2. **WebView2 refuses a webview whose environment options differ from the
//!    ones already in use for the same user data folder** (HRESULT
//!    0x8007139F). The shell window and every `Light` tab share one folder, and
//!    so do the context menu and the link chooser, so all of them must pass a
//!    byte-identical string. That is why [`BASE`] is mirrored into
//!    `tauri.conf.json` and pinned by the test below.
//!
//! Chromium keeps only the last occurrence of a repeated switch, so there is
//! exactly one `--disable-features=` token — also pinned by a test, because a
//! second one would silently drop whichever list came first.
//!
//! # The drag-drop handler
//!
//! Tauri registers its own OLE drop target on every window and tells WebView2
//! `SetAllowExternalDrop(false)` so it can report file drops itself. On Windows
//! that replaces WebView2's drop target, which is what makes in-page HTML5 drag
//! and drop stop working — Tauri's own docs say disabling the handler is
//! "required to use HTML5 drag and drop on the frontend on Windows". RustBox
//! reads no `DragDropEvent`, but it does rely on HTML5 drag and drop twice: the
//! sidebar reorder, and any web app that accepts a dragged file. So the handler
//! is off everywhere, and the shell window's copy of that decision lives in
//! `tauri.conf.json` next to the switches.

/// Switches shared by the shell window, every app tab, popups, the context menu
/// and the link chooser.
pub const BASE: &str = concat!(
  // wry's defaults, restated: a replacement string drops them.
  "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,",
  // The one that ends the reconnect problem: after five minutes hidden, chained
  // timers are allowed a single wake-up per minute.
  "IntensiveWakeUpThrottling,",
  // Windows reports a window that is fully covered as occluded, and an occluded
  // renderer is backgrounded like a hidden one. Every tab except the front one
  // is covered by construction, so this has to go too.
  "CalculateNativeWinOcclusion ",
  // wry's autoplay default, restated: without it a voice note or a video in a
  // web app stops starting on its own.
  "--autoplay-policy=no-user-gesture-required ",
  "--disable-background-timer-throttling ",
  "--disable-renderer-backgrounding ",
  "--disable-backgrounding-occluded-windows"
);

/// [`BASE`] plus a proxy, for the one case that needs it.
///
/// `proxy` is a full URI (`http://host:port`, `socks5://host:port`), the form
/// Chromium's `--proxy-server` switch takes. Only `Isolated` apps can have one:
/// they own a separate data folder, so a different argument string does not
/// collide with the environment the shell already created.
///
/// WebView2-only: the CEF build never builds an argument string, because
/// Chromium's command line is per process and its per-app proxy is `proxy_url`.
#[cfg(any(feature = "wry", test))]
pub fn with_proxy(proxy: Option<&str>) -> String {
  match proxy.map(str::trim) {
    Some(uri) if !uri.is_empty() => format!("{BASE} --proxy-server={uri}"),
    _ => BASE.to_string(),
  }
}

/// [`BASE`] split into individual switches.
///
/// Chromium's command line is per process, not per webview, so the CEF runtime
/// cannot take an argument string on a builder: it takes one switch at a time
/// before the browser process starts (`Cef::command_line_arg`). Parsing `BASE`
/// rather than keeping a second list is what stops the two engines from
/// drifting apart - the switches that keep a hidden tab alive have to be the
/// same ones on both, and a duplicated list is a list that eventually disagrees.
///
/// `--disable-features=a,b,c` becomes one pair with a value; a bare `--flag`
/// becomes a pair with `None`.
#[cfg(any(feature = "cef", test))]
pub fn switches() -> Vec<(&'static str, Option<&'static str>)> {
  BASE
    .split_whitespace()
    .map(|token| match token.split_once('=') {
      Some((key, value)) => (key, Some(value)),
      None => (token, None),
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn features(args: &str) -> Vec<&str> {
    args
      .split(' ')
      .find(|t| t.starts_with("--disable-features="))
      .map(|t| t.trim_start_matches("--disable-features=").split(',').collect())
      .unwrap_or_default()
  }

  #[test]
  fn keeps_a_hidden_tab_running() {
    for switch in [
      "--disable-background-timer-throttling",
      "--disable-renderer-backgrounding",
      "--disable-backgrounding-occluded-windows",
      // wry's default, which a replacement string would otherwise drop.
      "--autoplay-policy=no-user-gesture-required",
    ] {
      assert!(
        BASE.split(' ').any(|t| t == switch),
        "{switch} missing from the webview arguments"
      );
    }
    let features = features(BASE);
    for feature in [
      "msWebOOUI",
      "msPdfOOUI",
      "msSmartScreenProtection",
      "IntensiveWakeUpThrottling",
      "CalculateNativeWinOcclusion",
    ] {
      assert!(
        features.contains(&feature),
        "{feature} missing from --disable-features"
      );
    }
  }

  #[test]
  fn exactly_one_disable_features_switch() {
    let count = BASE
      .split(' ')
      .filter(|t| t.starts_with("--disable-features="))
      .count();
    assert_eq!(count, 1, "expected one --disable-features switch, got {count}");
  }

  #[test]
  fn switches_round_trip_to_the_same_argument_string() {
    let parsed = switches();
    let rebuilt = parsed
      .iter()
      .map(|(k, v)| match v {
        Some(v) => format!("{k}={v}"),
        None => (*k).to_string(),
      })
      .collect::<Vec<_>>()
      .join(" ");
    assert_eq!(rebuilt, BASE);

    // The value-carrying switch is the one that must not lose its list.
    let features = parsed
      .iter()
      .find(|(k, _)| *k == "--disable-features")
      .and_then(|(_, v)| *v)
      .expect("--disable-features is one switch with a value");
    assert!(features.contains("IntensiveWakeUpThrottling"));
    assert!(parsed.iter().any(|(k, v)| *k == "--autoplay-policy" && v.is_some()));
    assert!(
      parsed
        .iter()
        .any(|(k, v)| *k == "--disable-background-timer-throttling" && v.is_none())
    );
  }

  #[test]
  fn a_proxy_is_appended_without_splitting_the_feature_list() {
    assert_eq!(with_proxy(None), BASE);
    assert_eq!(with_proxy(Some("   ")), BASE);
    let proxied = with_proxy(Some("socks5://127.0.0.1:1080"));
    assert_eq!(
      proxied,
      format!("{BASE} --proxy-server=socks5://127.0.0.1:1080")
    );
    assert_eq!(
      proxied.matches("--disable-features=").count(),
      1,
      "a proxy must not add a second --disable-features switch"
    );
  }

  /// The shell window's arguments live in JSON, which cannot reference this
  /// constant. If the two drift apart, every `Light` tab fails to create with
  /// HRESULT 0x8007139F, so a mismatch has to be a build failure instead.
  #[test]
  fn config_matches_the_shell_window() {
    let conf: serde_json::Value =
      serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json parses");
    let windows = conf["app"]["windows"]
      .as_array()
      .expect("app.windows is an array");
    assert!(!windows.is_empty(), "no window declared");
    for window in windows {
      let args = window["additionalBrowserArgs"]
        .as_str()
        .expect("the shell window must declare additionalBrowserArgs");
      assert_eq!(
        args, BASE,
        "tauri.conf.json and webview_args::BASE disagree; Light tabs would \
         fail to create with HRESULT 0x8007139F"
      );
      // Tauri's drag-drop handler replaces WebView2's OLE drop target, which is
      // what breaks HTML5 drag and drop on Windows. The shell window has to opt
      // out of it in JSON, the same way every Rust-built webview does in code.
      assert_eq!(
        window["dragDropEnabled"],
        serde_json::Value::Bool(false),
        "the shell window must set dragDropEnabled:false, otherwise the sidebar \
         cannot be reordered by dragging"
      );
    }
  }
}
