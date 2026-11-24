# Vendored `tauri-runtime-cef` — local patch

Source of: `tauri-runtime-cef 3.0.0-alpha.7` (crates.io), unmodified except for the
shutdown drain described below. Pulled in through `[patch.crates-io]` in
`src-tauri/Cargo.toml`.

See `docs/adr/0009-cef-shutdown-drain.md` for the full diagnosis and measurements.

## Why

`run()` called `cef::shutdown()` the moment winit's event loop returned. With
`Settings::external_message_pump = true`, CEF's queued UI-thread teardown work is
only run by the host loop's pump, so that work was still queued when `CefShutdown`
tore down profiles that still owned browsers. On real profiles this faulted inside
`libcef` (`0xc0000005` at `+0x412eb0`) on roughly every third quit, and the
resulting torn LevelDB logged users out of apps (WhatsApp lost its pairing).

## What changed

Only two source files. The crate's `Cargo.lock` is its own (the published one
pinned crates that are no longer cached, so it was regenerated); it is only used
by a standalone `cargo test` in this directory — the app build resolves through
`src-tauri/Cargo.lock`.

### `src/external_message_pump/mod.rs`

- `PumpState::immediate_requests: AtomicUsize` counts scheduling requests with
  `delay_ms <= 0`. The pump's own `K_TIMER_DELAY_PLACEHOLDER` re-arm is excluded —
  it means "nothing to do", not "work pending" — so this is a usable idle signal.
- `PumpState::pump_once()` runs one `cef::do_message_loop_work()` without touching
  the platform timer, which would otherwise post messages to a window whose loop is
  gone.
- `CefExternalPump::drain_before_shutdown()` pumps until CEF has asked for no
  immediate work for `SHUTDOWN_QUIET` (150 ms) or `SHUTDOWN_BUDGET` (5 s) elapses,
  and returns a `DrainOutcome`.
- `DrainTracker` holds the timing rule with no CEF dependency; 4 unit tests in
  `drain_tests` cover quiet-stop, sustained work, a late request restarting the
  window, and the budget winning.

### `src/runtime.rs`

- `run()` clones the pump (so its platform window outlives the loop), calls
  `drain_before_shutdown()` after `run_app` returns, logs a warning on timeout,
  then calls `cef::shutdown()` as before.
- One import: `use crate::external_message_pump::{CefExternalPump, DrainOutcome};`

## Upgrading

1. Copy the new published source over this directory.
2. Re-apply the two changes above (they are small and self-contained).
3. `cargo test` inside this directory must pass, including `drain_tests`.
4. Re-run the E2E quit loop against a fixture with several real profiles — the
   crash only appears with real profiles and several of them.
