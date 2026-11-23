//! Keep ordinary builds working when a release sidecar has not been staged yet.
//!
//! `bundle.externalBin` is a list of sidecars the installer ships: the MCP shim
//! and, on Windows, the Chromium build. `tauri-build` resolves each entry to
//! `binaries/<name>-<triple><ext>` and *fails the build script* when the file is
//! absent. Both are build artifacts (the directory is gitignored), so a plain
//! `cargo test` or `cargo build` on a fresh clone died before compiling a single
//! test - which is exactly what happened on the first CI run.
//!
//! So each entry is checked and the missing ones are dropped, instead of the
//! list being cleared as a unit. The distinction matters now that there are two:
//! dropping everything because the Chromium build is absent would also silently
//! drop the MCP shim that *was* staged, and the resulting installer would be
//! missing a helper without saying so.
//!
//! The release path is unaffected: `scripts/build-installer.mjs` produces every
//! sidecar and stages it *before* invoking the bundler, and it passes the
//! complete list through `TAURI_CONFIG`, so the bundler never reads the reduced
//! list this function calculates for ordinary builds.

fn main() {
    let staged = staged_external_bins();
    let present: Vec<String> = staged
        .iter()
        .filter(|b| b.staged)
        .map(|b| b.configured.clone())
        .collect();
    let missing: Vec<&StagedBin> = staged.iter().filter(|b| !b.staged).collect();

    if !missing.is_empty() {
        for bin in &missing {
            println!(
                "cargo:warning={} sidecar not staged at {}; building without it. \
                 Run scripts/build-installer.mjs for an installer that bundles it.",
                bin.configured, bin.path
            );
        }
        // `tauri-build` reads this and merges it over tauri.conf.json, so the
        // build script's own validation sees the list that actually exists.
        //
        // Hand-formatted rather than built with serde_json: the build script has
        // no JSON dependency and adding one for a single flat string array is
        // not worth a new crate in the build graph.
        //
        // SAFETY: single-threaded, before any other code in this process reads
        // the environment.
        let list = present
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(",");
        unsafe {
            std::env::set_var(
                "TAURI_CONFIG",
                format!(r#"{{"bundle":{{"externalBin":[{list}]}}}}"#),
            );
        }
    }

    tauri_build::build()
}

struct StagedBin {
    /// The entry as `tauri.conf.json` spells it, which is what has to be written
    /// back into `TAURI_CONFIG`.
    configured: String,
    /// Where the bundler would look for it.
    path: String,
    staged: bool,
}

/// Which of the configured sidecars exist on disk right now.
///
/// Mirrors `external_binaries()` in tauri-utils: the configured path with
/// `-<triple>` and a platform extension appended.
///
/// The Chromium build is listed for Windows only, because that is the only
/// platform with a published engine payload (`engine-manifest.json`). Listing it
/// everywhere would make every macOS and Linux build print a warning about a
/// sidecar that platform never ships.
fn staged_external_bins() -> Vec<StagedBin> {
    let Some(manifest) = std::env::var("CARGO_MANIFEST_DIR").ok().map(std::path::PathBuf::from)
    else {
        return Vec::new();
    };
    let triple = std::env::var("TARGET")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_default();
    let ext = if triple.contains("windows") { ".exe" } else { "" };

    let configured: &[&str] = if triple.contains("windows") {
        &["binaries/rustbox-mcp", "binaries/rustbox-cef"]
    } else {
        &["binaries/rustbox-mcp"]
    };

    configured
        .iter()
        .map(|name| {
            let path = manifest
                .join("binaries")
                .join(format!("{}-{triple}{ext}", name.trim_start_matches("binaries/")));
            StagedBin {
                configured: (*name).to_string(),
                staged: path.exists(),
                path: path.display().to_string(),
            }
        })
        .collect()
}
