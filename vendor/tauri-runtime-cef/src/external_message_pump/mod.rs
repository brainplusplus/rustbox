// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Self-contained CEF external message pump.
//!
//! With [`cef::Settings::external_message_pump`] enabled, CEF does not run its
//! own message loop. Instead it asks the host to call
//! [`cef::do_message_loop_work`] by invoking `OnScheduleMessagePumpWork(delay)`
//! whenever it has work pending.
//!
//! Upstream cefclient:
//! - <https://github.com/chromiumembedded/cef/blob/b41c5c64fc50871630678e3d0c8c9bfe77cc6353/tests/shared/browser/main_message_loop_external_pump.cc>
//! - <https://github.com/chromiumembedded/cef/blob/b41c5c64fc50871630678e3d0c8c9bfe77cc6353/tests/shared/browser/main_message_loop_external_pump.h>
//!
//! This is a port of upstream cefclient's external pump — same semantics and
//! logic, adapted to Rust. The platform-independent scheduling/reentrancy logic
//! lives here; each platform supplies a [`PlatformPump`] backend that drives a
//! timer:
//!
//! - Windows: a `WM_TIMER` on a message-only window.
//! - macOS: an `NSTimer` in the common and event-tracking run-loop modes.
//! - Linux/BSD: a custom low-priority GLib source with a wakeup pipe.
//!
//! On Windows and macOS the timer lives on the same native loop winit already
//! runs, so CEF keeps painting and processing IPC even while the OS spins a
//! nested modal loop winit cannot observe (window move/resize on Windows, menu
//! and event tracking on macOS). On Linux/BSD the GLib source still dispatches
//! inside nested GLib loops (e.g. GTK menus/dialogs) for the same reason.

use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
use linux::PlatformPump;
#[cfg(target_os = "macos")]
use macos::PlatformPump;
#[cfg(windows)]
use windows::PlatformPump;

// Special timer delay placeholder value. Intentionally 32-bit for Windows and
// OS X platform API compatibility.
const K_TIMER_DELAY_PLACEHOLDER: i64 = i32::MAX as i64;

// The maximum number of milliseconds we're willing to wait between calls to
// DoWork().
const K_MAX_TIMER_DELAY: i64 = 1000 / 30; // 30fps

/// Handle to the external message pump. Cloning shares the same underlying
/// state; the backing platform resources are released when the last clone drops.
#[derive(Clone)]
pub(crate) struct CefExternalPump {
  state: Arc<PumpState>,
}

impl CefExternalPump {
  pub(crate) fn new() -> Self {
    let state = Arc::new_cyclic(|weak| PumpState {
      is_active: AtomicBool::new(false),
      reentrancy_detected: AtomicBool::new(false),
      immediate_requests: AtomicUsize::new(0),
      platform: Mutex::new(PlatformPump::new(weak.clone())),
    });

    Self { state }
  }

  /// Called from CEF's `OnScheduleMessagePumpWork`. May run on any thread.
  pub(crate) fn on_schedule_message_pump_work(&self, delay_ms: i64) {
    self.state.on_schedule_message_pump_work(delay_ms);
  }

  /// Explicit tick, used to drive CEF before winit's loop is running (startup)
  /// and after winit processes a batch of events. Must run on the owner thread.
  pub(crate) fn do_work(&self) {
    self.state.do_work();
  }

  /// Number of requests CEF made for work to run *immediately*. Used by
  /// [`Self::drain_before_shutdown`] to tell "CEF still has tasks queued" from
  /// "CEF is idle", because the pump's own re-arming with the maximum timer
  /// delay is not a sign of pending work.
  fn immediate_requests(&self) -> usize {
    self.state.immediate_requests.load(Ordering::Relaxed)
  }

  /// Runs CEF's queued UI-thread work after the host's event loop has exited.
  ///
  /// Destroying a browser or a request context is posted to CEF's UI thread, and
  /// the only thing that normally runs those tasks is the pump being driven from
  /// the host loop (a `WM_TIMER`/`WM_USER` post on Windows, an `NSTimer` on
  /// macOS, a GLib source on Linux). Once that loop is gone nothing drains them,
  /// and `cef::shutdown()` then tears down profiles whose browsers are still
  /// alive - the crash this drain exists to prevent.
  ///
  /// Pumps until CEF has gone `SHUTDOWN_QUIET` without asking for immediate
  /// work, or `SHUTDOWN_BUDGET` elapses. Must run on the owner thread.
  pub(crate) fn drain_before_shutdown(&self) -> DrainOutcome {
    let start = Instant::now();
    let mut tracker =
      DrainTracker::new(self.immediate_requests(), SHUTDOWN_BUDGET, SHUTDOWN_QUIET);
    loop {
      // `do_work` would also re-arm the platform timer, which posts messages
      // that nothing dispatches once the host loop is gone. Only the CEF side
      // of the pump matters here.
      self.state.pump_once();
      let elapsed = start.elapsed();
      if let Some(outcome) = tracker.observe(elapsed, self.immediate_requests()) {
        return outcome;
      }
      std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
    }
  }
}

/// How [`CefExternalPump::drain_before_shutdown`] finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrainOutcome {
  /// CEF stopped asking for immediate work: its teardown tasks have run.
  Drained,
  /// The budget ran out first. Shutdown proceeds anyway rather than hanging the
  /// process; CEF only has to have made progress, not necessarily all of it.
  TimedOut,
}

/// How long CEF must stay free of immediate work before the drain considers the
/// queue empty. Teardown chains re-post themselves, so a single idle tick is not
/// enough evidence.
const SHUTDOWN_QUIET: Duration = Duration::from_millis(150);

/// Upper bound on the drain. Shutdown must never look like a hang.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Sleep between pump iterations, so the drain does not spin a core while CEF
/// waits on a worker.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Decides when the shutdown drain may stop.
///
/// Kept separate from the pump so the timing rule is testable without CEF.
pub(crate) struct DrainTracker {
  last_requests: usize,
  changed_at: Duration,
  budget: Duration,
  quiet: Duration,
}

impl DrainTracker {
  pub(crate) fn new(requests: usize, budget: Duration, quiet: Duration) -> Self {
    Self {
      last_requests: requests,
      changed_at: Duration::ZERO,
      budget,
      quiet,
    }
  }

  /// Feeds one observation in. Returns `Some` once the drain should stop.
  pub(crate) fn observe(&mut self, elapsed: Duration, requests: usize) -> Option<DrainOutcome> {
    if requests != self.last_requests {
      self.last_requests = requests;
      self.changed_at = elapsed;
    }

    if elapsed >= self.budget {
      return Some(DrainOutcome::TimedOut);
    }

    if elapsed.saturating_sub(self.changed_at) >= self.quiet {
      return Some(DrainOutcome::Drained);
    }

    None
  }
}

/// Platform-independent pump state, shared with the [`PlatformPump`] backend.
struct PumpState {
  is_active: AtomicBool,
  reentrancy_detected: AtomicBool,
  /// Counts scheduling requests that asked for work *now* (`delay_ms <= 0`).
  /// The pump's own `K_TIMER_DELAY_PLACEHOLDER` re-arm is excluded: it means
  /// "nothing to do, wake me later", not "work is pending".
  immediate_requests: AtomicUsize,
  platform: Mutex<PlatformPump>,
}

impl PumpState {
  /// Post a scheduling request onto the owner thread. The platform backend is
  /// responsible for delivering it there, where it lands back in
  /// [`Self::on_schedule_work`]. Mirrors the platform `OnScheduleMessagePumpWork`.
  fn on_schedule_message_pump_work(&self, delay_ms: i64) {
    if delay_ms <= 0 {
      self.immediate_requests.fetch_add(1, Ordering::Relaxed);
    }
    if let Ok(mut platform) = self.platform.lock() {
      platform.on_schedule_message_pump_work(delay_ms);
    }
  }

  /// Runs on the owner thread once a scheduling request is delivered. Mirrors
  /// cefclient's `OnScheduleWork`.
  fn on_schedule_work(&self, mut delay_ms: i64) {
    {
      let Ok(mut platform) = self.platform.lock() else {
        return;
      };

      if delay_ms == K_TIMER_DELAY_PLACEHOLDER && platform.is_timer_pending() {
        // Don't set the maximum timer requested from DoWork() if a timer event is
        // currently pending.
        return;
      }

      platform.kill_timer();
    }

    if delay_ms <= 0 {
      // Execute the work immediately.
      self.do_work();
    } else if let Ok(mut platform) = self.platform.lock() {
      // Never wait longer than the maximum allowed time.
      if delay_ms > K_MAX_TIMER_DELAY {
        delay_ms = K_MAX_TIMER_DELAY;
      }

      // Results in call to OnTimerTimeout() after the specified delay.
      platform.set_timer(delay_ms);
    }
  }

  /// Runs on the owner thread when the platform timer fires. Mirrors cefclient's
  /// `OnTimerTimeout`.
  fn on_timer_timeout(&self) {
    if let Ok(mut platform) = self.platform.lock() {
      platform.kill_timer();
    }
    self.do_work();
  }

  /// Mirrors cefclient's `DoWork`.
  fn do_work(&self) {
    let was_reentrant = self.perform_message_loop_work();
    if was_reentrant {
      // Execute the remaining work as soon as possible.
      self.on_schedule_message_pump_work(0);
    } else if !self.is_timer_pending() {
      // Schedule a timer event at the maximum allowed time. This may be dropped
      // in OnScheduleWork() if another timer event is already in-flight.
      self.on_schedule_message_pump_work(K_TIMER_DELAY_PLACEHOLDER);
    }
  }

  fn is_timer_pending(&self) -> bool {
    self
      .platform
      .lock()
      .map(|platform| platform.is_timer_pending())
      .unwrap_or(true)
  }

  /// Runs one CEF message-loop pass without touching the platform timer, for
  /// hosts whose own event loop is no longer running (shutdown drain).
  fn pump_once(&self) {
    // Re-entrancy is impossible here (the host loop that could re-enter is
    // gone), and a re-entrant call would silently skip the work.
    debug_assert!(!self.is_active.load(Ordering::SeqCst));
    self.is_active.store(true, Ordering::SeqCst);
    cef::do_message_loop_work();
    self.is_active.store(false, Ordering::SeqCst);
  }

  /// Mirrors cefclient's `PerformMessageLoopWork`.
  fn perform_message_loop_work(&self) -> bool {
    if self.is_active.load(Ordering::SeqCst) {
      // When CefDoMessageLoopWork() is called there may be various callbacks
      // (such as paint and IPC messages) that result in additional calls to this
      // method. If re-entrancy is detected we must repost a request again to the
      // owner thread to ensure that the discarded call is executed in the future.
      self.reentrancy_detected.store(true, Ordering::SeqCst);
      return false;
    }

    self.reentrancy_detected.store(false, Ordering::SeqCst);

    self.is_active.store(true, Ordering::SeqCst);
    cef::do_message_loop_work();
    self.is_active.store(false, Ordering::SeqCst);

    // |reentrancy_detected_| may have changed due to re-entrant calls to this
    // method.
    self.reentrancy_detected.load(Ordering::SeqCst)
  }
}

#[cfg(test)]
mod drain_tests {
  use super::*;

  const BUDGET: Duration = Duration::from_secs(5);
  const QUIET: Duration = Duration::from_millis(150);
  const STEP: Duration = Duration::from_millis(5);

  /// Feeds the tracker `steps` observations with a fixed request count.
  fn run_for(
    tracker: &mut DrainTracker,
    from: Duration,
    steps: usize,
    requests: usize,
  ) -> Option<DrainOutcome> {
    let mut elapsed = from;
    for _ in 0..steps {
      elapsed += STEP;
      if let Some(outcome) = tracker.observe(elapsed, requests) {
        return Some(outcome);
      }
    }
    None
  }

  #[test]
  fn stops_after_the_quiet_window_when_cef_goes_idle() {
    let mut tracker = DrainTracker::new(7, BUDGET, QUIET);
    // 20 quiet steps = 100 ms: not yet.
    assert_eq!(run_for(&mut tracker, Duration::ZERO, 20, 7), None);
    // 10 more = 150 ms since the last request: drained.
    assert_eq!(
      run_for(&mut tracker, Duration::from_millis(100), 10, 7),
      Some(DrainOutcome::Drained)
    );
  }

  #[test]
  fn keeps_draining_while_cef_keeps_asking_for_work() {
    let mut tracker = DrainTracker::new(0, BUDGET, QUIET);
    // A different request count every step: never quiet, so only the budget ends it.
    let mut requests = 0;
    let mut elapsed = Duration::ZERO;
    let mut outcome = None;
    while outcome.is_none() {
      elapsed += STEP;
      requests += 1;
      outcome = tracker.observe(elapsed, requests);
    }
    assert_eq!(outcome, Some(DrainOutcome::TimedOut));
    assert_eq!(elapsed, BUDGET);
  }

  #[test]
  fn a_late_request_restarts_the_quiet_window() {
    let mut tracker = DrainTracker::new(0, BUDGET, QUIET);
    // Quiet long enough to finish ...
    assert_eq!(run_for(&mut tracker, Duration::ZERO, 30, 0), Some(DrainOutcome::Drained));

    // ... but a request that arrives just before the window closes keeps it going.
    let mut tracker = DrainTracker::new(0, BUDGET, QUIET);
    assert_eq!(run_for(&mut tracker, Duration::ZERO, 29, 0), None);
    assert_eq!(tracker.observe(Duration::from_millis(145), 1), None);
    assert_eq!(run_for(&mut tracker, Duration::from_millis(145), 20, 1), None);
    assert_eq!(
      run_for(&mut tracker, Duration::from_millis(245), 10, 1),
      Some(DrainOutcome::Drained)
    );
  }

  #[test]
  fn the_budget_wins_even_if_cef_never_settles() {
    let mut tracker = DrainTracker::new(0, BUDGET, QUIET);
    assert_eq!(tracker.observe(BUDGET, 99), Some(DrainOutcome::TimedOut));
  }
}
