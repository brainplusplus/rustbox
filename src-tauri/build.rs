fn main() {
    // A clean checkout cannot satisfy `bundle.externalBin`.
    //
    // `tauri-build` resolves that entry to `binaries/rustbox-mcp-<triple>` and fails
    // the build script when the file is absent. The sidecar is a build artifact (it
    // is gitignored), so any plain `cargo test` or `cargo build` on a fresh clone
    // died before compiling a single test — which is exactly what happened on the
    // first CI run.
    //
    // Dropping `externalBin` when the staged file is missing keeps ordinary builds
    // working. The release path still bundles the sidecar: scripts/build-installer.mjs
    // compiles the shim and stages it *before* invoking `cargo tauri build`, so by
    // the time the bundler reads the config the file is there.
    if let Some(path) = missing_external_bin() {
        println!(
            "cargo:warning=MCP sidecar not staged at {path}; building without it. \
             Run scripts/build-installer.mjs for an installer that bundles it."
        );
        // `tauri-build` reads this and merges it over tauri.conf.json.
        //
        // Written with `set_var` rather than layered over whatever the caller set:
        // building the shim itself has no `bundle` section that matters, and the
        // release script sets its own value for that same invocation. A textual
        // merge of two JSON documents is easy to get wrong on key collisions, and
        // the only thing this branch ever needs to express is "no sidecar".
        //
        // SAFETY: single-threaded, before any other code in this process reads the
        // environment.
        unsafe {
            std::env::set_var("TAURI_CONFIG", r#"{"bundle":{"externalBin":[]}}"#);
        }
    }

    tauri_build::build()
}

/// The staged `externalBin` this build would need but does not have.
///
/// Mirrors how `external_binaries()` in tauri-utils builds the name: the configured
/// path with `-<triple>` and a platform extension appended.
fn missing_external_bin() -> Option<String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let triple = std::env::var("TARGET")
        .or_else(|_| std::env::var("HOST"))
        .ok()?;
    let ext = if triple.contains("windows") { ".exe" } else { "" };
    let staged = std::path::Path::new(&manifest)
        .join("binaries")
        .join(format!("rustbox-mcp-{triple}{ext}"));
    (!staged.exists()).then(|| staged.display().to_string())
}
