// Build a platform installer with the MCP shim and the Chromium build included.
//
// The app ships as two executables, not one, and that is a measured constraint
// rather than a packaging choice: `libcef.dll` is a load-time import of the
// Chromium build, so running CEF without the 292 MB payload on disk is
// impossible (`0xC0000135` without it; `/DELAYLOAD` moves the failure, it does
// not remove it). Both binaries therefore have to be installed, and the user
// still gets one shortcut because `handoff.rs` starts whichever one the config
// asks for.
//
// Both are staged here as `externalBin` sidecars. Shipping only the launcher is
// how an install ends up with the engine payload present, `installed_versions()`
// reporting it ready, and `stage_binary_if_present` finding no binary - the
// launcher then hands off to a `rustbox-cef.exe` that was never installed.
//
// Why this wrapper exists: `bundle.externalBin` in `src-tauri/tauri.conf.json`
// points at `binaries/rustbox-mcp`, which the bundler resolves to
// `binaries/rustbox-mcp-<TARGET_TRIPLE><ext>`. `tauri-build` checks that file
// exists while compiling the *app* crate, i.e. before the shim has necessarily
// been built. A `beforeBundleCommand` hook is too late, and the hook shell has an
// empty PATH so it cannot call cargo or node anyway.
//
// So the order has to be: build the shim, stage it under the triple-suffixed
// name, then let `cargo tauri build` do the rest. The bundler strips the suffix
// and installs it as plain `rustbox-mcp` next to the app binary, which is exactly
// the name `mcp::shim_path()` looks for.
//
// The bundle set is per-platform rather than fixed: `tauri.conf.json` names only
// the targets that make sense for each OS, and this script follows it instead of
// hard-coding `nsis`, so the same command produces a `.dmg`/`.app` on macOS and a
// `.deb`/`.AppImage` on Linux. `RB_BUNDLES` overrides it, which is what CI uses
// to build one target per job.

import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..");
const srcTauri = join(repo, "src-tauri");
const exe = process.platform === "win32" ? ".exe" : "";

// What the bundler writes per target, so the summary names real files instead of
// guessing from the version string.
const EXTENSIONS = {
  nsis: [".exe"],
  msi: [".msi"],
  app: [".app"],
  dmg: [".dmg"],
  deb: [".deb"],
  rpm: [".rpm"],
  appimage: [".AppImage"],
};

function run(command, args, env) {
  console.log(`\n> ${command} ${args.join(" ")}`);
  execFileSync(command, args, {
    cwd: srcTauri,
    stdio: "inherit",
    env: env ? { ...process.env, ...env } : process.env,
  });
}

// Path length the Chromium build has to fit into, measured rather than assumed.
//
// `CEF_BUILD_SUFFIX` is what cargo appends to the target dir for a release build
// of cef-dll-sys: `\release\build\cef-dll-sys-<16 hex>\out\build`.
// `CEF_DEEPEST_OBJECT` is the longest relative object path under that build dir,
// from `cpptoc/test/`. Together they are what has to stay under `MAX_PATH`.
const CEF_BUILD_SUFFIX = "\\release\\build\\cef-dll-sys-0123456789abcdef\\out\\build".length;
const CEF_DEEPEST_OBJECT =
  "\\libcef_dll_wrapper\\CMakeFiles\\libcef_dll_wrapper.dir\\cpptoc\\test\\api_version_test_ref_ptr_client_cpptoc.cc.obj".length;

// Bundle targets this platform can actually produce.
//
// `tauri.conf.json` lists every platform's targets in one array (`nsis` and `msi`
// for Windows, `app` and `dmg` for macOS, `deb`/`rpm`/`appimage` for Linux),
// because the config is one file for all of them. That is fine when the CLI reads
// it - it filters by host - but it is *not* fine to forward the list verbatim to
// `--bundles`, which validates every name against the host's supported set and
// aborts on the first foreign one:
//
//   error: invalid value 'dmg' for '--bundles [<BUNDLES>...]'
//     [possible values: msi, nsis]
//
// So the configured list is filtered to this platform's set. Anything the config
// names that the host cannot build is dropped rather than failing the run; if
// that leaves nothing, the run fails with a message that says which names were
// considered.
const BUNDLE_TARGETS = {
  win32: ["nsis", "msi"],
  darwin: ["app", "dmg"],
  linux: ["deb", "rpm", "appimage"],
};

function bundles() {
  const override = process.env.RB_BUNDLES?.trim();
  if (override) return override;

  const configured = JSON.parse(
    readFileSync(join(srcTauri, "tauri.conf.json"), "utf8"),
  ).bundle?.targets ?? "all";
  // `"all"` is Tauri's own spelling for "every target this platform supports", so
  // it is passed straight through instead of being expanded here.
  if (configured === "all") return "all";

  const supported = BUNDLE_TARGETS[process.platform] ?? [];
  const wanted = (Array.isArray(configured) ? configured : [configured]).filter((t) =>
    supported.includes(t),
  );
  if (wanted.length === 0) {
    console.error(
      `build-installer: tauri.conf.json names no bundle target for ${process.platform}.\n` +
        `  configured: ${JSON.stringify(configured)}\n` +
        `  supported:  ${supported.join(", ") || "(none)"}\n` +
        `Set RB_BUNDLES to choose one explicitly.`,
    );
    process.exit(1);
  }
  const dropped = (Array.isArray(configured) ? configured : []).filter(
    (t) => !supported.includes(t),
  );
  if (dropped.length > 0) {
    console.log(`skipping bundle targets for other platforms: ${dropped.join(", ")}`);
  }
  return wanted.join(",");
}

const hostTriple = execFileSync("rustc", ["--print", "host-tuple"], {
  encoding: "utf8",
}).trim();
// `RB_TARGET` lets CI build a triple other than the host's: on macOS the universal
// bundle is requested as `--target universal-apple-darwin`, and the shim has to be
// staged under exactly that name.
const triple = process.env.RB_TARGET?.trim() || hostTriple;
if (!triple) {
  console.error("build-installer: could not determine the target triple");
  process.exit(1);
}

// A universal build is requested with the meta-triple `universal-apple-darwin`,
// which cargo cannot compile directly. Either Apple triple in RB_TARGET means the
// caller wants a binary that runs on both, so both are built and lipo'd.
const universal =
  triple.includes("apple-darwin") &&
  (triple === "universal-apple-darwin" || process.env.RB_UNIVERSAL === "1");
// A universal app is built from two real architectural targets, so the shim has
// to be compiled twice and lipo'd. Every other triple is a plain native build.
const shimTriples = universal
  ? ["aarch64-apple-darwin", "x86_64-apple-darwin"]
  : triple === hostTriple
    ? [hostTriple]
    : [triple];

// 1. Build the shim first so the staged copy is never stale.
//
// `tauri-build` validates `bundle.externalBin` while compiling the app crate and
// fails when the triple-suffixed file is missing, which it is on a clean tree.
// Clearing `externalBin` for this one invocation lets the shim compile without
// that check: `tauri-build` *does* read `TAURI_CONFIG` and merges it over
// tauri.conf.json. (The CLI itself does not read it - see step 4 - but this
// invocation never goes through the CLI.)
for (const t of shimTriples) {
  run(
    "cargo",
    [
      "build",
      "--release",
      "--bin",
      "rustbox-mcp",
      ...(t === hostTriple ? [] : ["--target", t]),
    ],
    { TAURI_CONFIG: JSON.stringify({ bundle: { externalBin: [] } }) },
  );
}

// The bundler appends the triple to the configured sidecar name and copies it; it
// does no lipo. So the file has to already be at the name it will look for.
const staging = join(srcTauri, "binaries");
mkdirSync(staging, { recursive: true });
const staged = join(staging, `rustbox-mcp-${triple}${exe}`);

// Cargo writes to `target/release` when the triple is the host's and to
// `target/<triple>/release` otherwise. Assuming the per-triple path for both
// made the macOS job report a missing shim even though it had just built it.
const cargoOutDir = (t) =>
  t === hostTriple
    ? join(srcTauri, "target", "release")
    : join(srcTauri, "target", t, "release");

if (universal) {
  const parts = shimTriples.map((t) => join(cargoOutDir(t), `rustbox-mcp${exe}`));
  for (const part of parts) {
    if (!existsSync(part)) {
      console.error(`build-installer: shim missing for a universal build: ${part}`);
      process.exit(1);
    }
  }
  try {
    execFileSync("lipo", ["-create", "-output", staged, ...parts], { stdio: "inherit" });
  } catch (e) {
    console.error(`build-installer: lipo failed (${e.message}); run this on macOS`);
    process.exit(1);
  }
  console.log(`\nlipo'd ${staged} from ${parts.length} architectures`);
} else {
  const shim = join(cargoOutDir(triple), `rustbox-mcp${exe}`);
  if (!existsSync(shim)) {
    console.error(`build-installer: the MCP shim was not built: ${shim}`);
    process.exit(1);
  }
  // 2. Stage it where `externalBin` expects it.
  copyFileSync(shim, staged);
}

console.log(`\nstaged ${staged} (${statSync(staged).size} bytes)`);

// 2b. A universal build also has to plant the shim in the bundler's *output*
// directory, not just under `binaries/`.
//
// `src-tauri/Cargo.toml` declares `[[bin]] name = "rustbox-mcp"`, so the Tauri CLI
// lists it as a secondary bundle binary and `copy_binaries_to_bundle()`
// (tauri-bundler `bundle/macos/app.rs`) copies `settings.binary_path(bin)` --
// literally `<out>/rustbox-mcp` -- into `Contents/MacOS/`. The CLI's lipo step
// only merges the *main* binary, so on a universal target nothing ever writes
// that path and the bundle aborts with "Failed to copy binary from
// target/universal-apple-darwin/release/rustbox-mcp ... does not exist".
// Native builds never hit this: plain `cargo build` already put the shim in
// `target/release`, which is the same out dir.
if (triple !== hostTriple) {
  const outDir = cargoOutDir(triple);
  mkdirSync(outDir, { recursive: true });
  const outShim = join(outDir, `rustbox-mcp${exe}`);
  copyFileSync(staged, outShim);
  // The bundler copies this file into Contents/MacOS with `fs::copy`, which carries
  // the mode over. A non-executable helper inside a signed .app is a broken bundle,
  // so make the bit explicit rather than rely on what the copy happened to keep.
  if (process.platform !== "win32") chmodSync(outShim, 0o755);
  console.log(`seeded bundler out dir: ${outShim}`);
}

// 3. Stage the Chromium build as a second sidecar.
//
// Two reasons it is built here rather than by `cargo tauri build`:
//
// * The engines are mutually exclusive Cargo features (`Cargo.toml`), so the
//   `rustbox-cef` target is skipped whenever the default feature set is active.
//   Nothing else in the release path would ever produce it.
// * It needs its own target directory. Sharing `target/release` with the wry
//   build would flip the feature set on every invocation and recompile the whole
//   dependency graph twice.
//
// Only Windows has a published payload (`src-tauri/engine-manifest.json` carries
// a single `windows-x86_64` entry), so elsewhere there is nothing to stage and
// the sidecar is left out of the bundle instead of failing the build.
const sidecars = ["binaries/rustbox-mcp"];
if (process.platform === "win32") {
  if (triple !== hostTriple) {
    console.error(
      `build-installer: the Chromium sidecar is built for the host triple only, but\n` +
        `RB_TARGET is ${triple} and the host is ${hostTriple}.`,
    );
    process.exit(1);
  }
  const cefPath = process.env.CEF_PATH?.trim();
  if (!cefPath) {
    console.error(
      "build-installer: CEF_PATH is not set.\n\n" +
        "The Chromium build links against a CEF 154.0.34 distribution (see\n" +
        "docs/adr/0005-embedded-chromium-cef.md, section 24). Point CEF_PATH at the\n" +
        "extracted `cef_binary_154.0.34+..._windows64_minimal` directory.",
    );
    process.exit(1);
  }
  // The distribution must be extracted, not the archive it arrived in: cef-dll-sys
  // looks for `<CEF_PATH>/<version>/cef_windows_x86_64/CEFConfig.cmake`, and
  // reports a missing one as a CMake configuration error a long way from here.
  if (!existsSync(cefPath)) {
    console.error(`build-installer: CEF_PATH does not exist: ${cefPath}`);
    process.exit(1);
  }

  // A short target directory, deliberately not `src-tauri/target/cef`.
  //
  // cef-dll-sys drives CMake over 123 wrapper translation units, and CMake's
  // Ninja generator writes each object under
  // `<target>/release/build/cef-dll-sys-<hash>/out/build/libcef_dll_wrapper/
  //  CMakeFiles/libcef_dll_wrapper.dir/cpptoc/test/<name>.cc.obj`
  // - 52 characters of cargo scaffolding plus 114 for the deepest CEF source.
  // MSVC on Windows still refuses paths over `MAX_PATH` (260) even with
  // `LongPathsEnabled=1`, and cl.exe reports it as
  // `fatal error C1083: Cannot open compiler generated file: ''`, which names
  // neither the file nor the limit. A checkout whose own path is deep (a
  // worktree under `AppData`, a CI workspace) pushes the object path over 260
  // and the build dies two thirds of the way through with no usable message.
  //
  // So the build root is placed where it is short. Temp is the portable choice
  // and it is a cache, not output: losing it costs a recompile, never a wrong
  // artifact. `RB_CEF_TARGET` overrides it for a CI job that wants to cache it.
  const cefTarget = process.env.RB_CEF_TARGET?.trim() || join(tmpdir(), "rustbox-cef-target");
  const objectBudget = cefTarget.length + CEF_BUILD_SUFFIX + CEF_DEEPEST_OBJECT;
  if (objectBudget > 250) {
    console.error(
      `build-installer: the Chromium build target is too deep for MSVC.\n\n` +
        `  target:        ${cefTarget}\n` +
        `  longest object: ~${objectBudget} characters (limit is 260)\n\n` +
        `Set RB_CEF_TARGET to a shorter directory (e.g. C:\\cef).`,
    );
    process.exit(1);
  }
  // The Chromium build is the only part of this project that compiles C++:
  // `cef-dll-sys` drives CMake over the CEF wrapper sources. CMake's Ninja
  // generator passes MSVC include and library paths through the environment
  // rather than baking them into the build files, so without vcvars it fails
  // with `fatal error C1083: Cannot open include file: 'stdint.h'` - which reads
  // like a broken checkout rather than a missing developer prompt. Check for the
  // environment explicitly instead of letting the build report it as a source
  // error. Plain `cargo build` of the wry app does not need this: rustc finds
  // its own linker and system libraries.
  if (!process.env.INCLUDE?.trim()) {
    console.error(
      "build-installer: the MSVC environment is not loaded (INCLUDE is empty).\n\n" +
        "The Chromium build compiles C++ through CMake, which needs the Visual\n" +
        "Studio environment. Run this script from a \"Developer PowerShell for VS\n" +
        "2022\", or from a shell where vcvars64.bat has been called first:\n\n" +
        '  cmd /c "call \"C:\\Program Files (x86)\\Microsoft Visual Studio\\2022\\BuildTools\\VC\\Auxiliary\\Build\\vcvars64.bat\" && node scripts/build-installer.mjs"',
    );
    process.exit(1);
  }

  run(
    "cargo",
    [
      "build",
      "--release",
      "--no-default-features",
      "--features",
      "cef",
      "--bin",
      "rustbox-cef",
    ],
    { CARGO_TARGET_DIR: cefTarget, CEF_PATH: cefPath },
  );

  const built = join(cefTarget, "release", `rustbox-cef${exe}`);
  if (!existsSync(built)) {
    console.error(`build-installer: the Chromium build is missing: ${built}`);
    process.exit(1);
  }
  const stagedCef = join(staging, `rustbox-cef-${triple}${exe}`);
  copyFileSync(built, stagedCef);
  console.log(`\nstaged ${stagedCef} (${statSync(stagedCef).size} bytes)`);
  sidecars.push("binaries/rustbox-cef");
}

// 4. Build the app and the installer(s).
//
// The Tauri CLI is invoked through the local npm install rather than `cargo tauri`:
// the cargo subcommand only exists where someone ran `cargo install tauri-cli`
// by hand, which is true on a developer machine and false on a CI runner. The npm
// binary is pinned by package-lock.json and is the same version everywhere.
const targets = bundles();
console.log(`\nBundling: ${targets} (sidecars: ${sidecars.join(", ")})`);
// The CLI is invoked as a JS entry through this same Node binary, not through
// `node_modules/.bin/tauri.cmd`.
//
// Two reasons. On Windows a `.cmd` is not a PE file, so it cannot be spawned
// without a shell, and `shell: true` makes Node concatenate the argument array
// instead of escaping it (`DEP0190`) - which mangles the JSON below, since it is
// full of quotes. Running the JS entry with an argument array keeps every
// argument as one argv element on every platform.
const tauriCli = join(repo, "node_modules", "@tauri-apps", "cli", "tauri.js");
if (!existsSync(tauriCli)) {
  console.error(`build-installer: Tauri CLI missing at ${tauriCli}; run npm ci`);
  process.exit(1);
}
// The sidecar list has to reach the bundler through `--config`.
//
// `TAURI_CONFIG` looks like the right channel and is not: the CLI *writes* that
// variable for the build script (`helpers/config.rs`) and never reads it, so an
// override set here is silently ignored and the bundler uses whatever
// `tauri.conf.json` declares. That failure is invisible - the installer builds
// successfully and simply lacks the sidecar. Verified: with the environment
// variable set, the NSIS installer came out at 32.3 MB uncompressed (launcher and
// MCP shim only) and `rustbox-cef.exe` was absent.
//
// Only `externalBin` is passed, not the whole `bundle` section: `json_patch::merge`
// replaces arrays wholesale, so this is enough to add the Chromium build without
// restating (and risking divergence from) the icons, targets and installer
// options the file already declares.
const configOverride = JSON.stringify({ bundle: { externalBin: sidecars } });
execFileSync(
  process.execPath,
  [
    tauriCli,
    "build",
    "--bundles",
    targets,
    ...(triple === hostTriple ? [] : ["--target", triple]),
    "--config",
    configOverride,
  ],
  { cwd: srcTauri, stdio: "inherit", env: process.env },
);

// Report whatever the bundler actually produced rather than a name built from
// the version string, so a version bump does not silently hide the artifact.
// A cross/universal build nests its output under the triple; a native build puts
// it straight in `target/release`.
const bundleRoot = join(
  srcTauri,
  "target",
  triple === hostTriple ? "" : triple,
  "release",
  "bundle",
);
const wanted = targets.split(",").map((t) => t.trim().toLowerCase());
let found = 0;
for (const target of wanted) {
  const dir = join(bundleRoot, target);
  if (!existsSync(dir)) continue;
  const exts = EXTENSIONS[target] ?? [];
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    const isDir = statSync(path).isDirectory();
    const matches = exts.some((e) => entry.endsWith(e)) || (isDir && exts.includes(`.${target}`));
    if (!matches) continue;
    found += 1;
    if (isDir) {
      // `.app` is a directory; report its size by walking one level so the line
      // is still useful in a CI log.
      let bytes = 0;
      for (const inner of readdirSync(path, { recursive: true })) {
        try {
          bytes += statSync(join(path, String(inner))).size;
        } catch {}
      }
      console.log(`\nartifact: ${path} (${bytes} bytes, directory)`);
    } else {
      console.log(`\nartifact: ${path} (${statSync(path).size} bytes)`);
    }
  }
}
if (found === 0) {
  console.error(`build-installer: no artifacts found under ${bundleRoot} for ${targets}`);
  process.exit(1);
}
console.log(`\n${found} artifact(s) built`);
