// Build a platform installer with the MCP shim included.
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
import { copyFileSync, existsSync, mkdirSync, readdirSync, statSync } from "node:fs";
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

function bundles() {
  const override = process.env.RB_BUNDLES?.trim();
  if (override) return override;
  // A dry read of the config, not a JSON parse: the file is the source of truth
  // for what this platform can actually produce, and Tauri validates it anyway.
  const conf = JSON.parse(
    execFileSync("node", ["-e", `
      const fs = require("node:fs");
      const c = JSON.parse(fs.readFileSync(${JSON.stringify(join(srcTauri, "tauri.conf.json"))}, "utf8"));
      process.stdout.write(JSON.stringify(c.bundle?.targets ?? "all"));
    `], { encoding: "utf8" }),
  );
  return Array.isArray(conf) ? conf.join(",") : String(conf);
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
// Clearing `externalBin` for this one invocation (TAURI_CONFIG is merged over
// tauri.conf.json) lets the shim compile without that check.
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

if (universal) {
  const parts = shimTriples.map((t) =>
    join(srcTauri, "target", t, "release", `rustbox-mcp${exe}`),
  );
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
  const shim = join(srcTauri, "target", "release", `rustbox-mcp${exe}`);
  if (!existsSync(shim)) {
    console.error(`build-installer: the MCP shim was not built: ${shim}`);
    process.exit(1);
  }
  // 2. Stage it where `externalBin` expects it.
  copyFileSync(shim, staged);
}

console.log(`\nstaged ${staged} (${statSync(staged).size} bytes)`);

// 3. Build the app and the installer(s).
//
// The Tauri CLI is invoked through the local npm install rather than `cargo tauri`:
// the cargo subcommand only exists where someone ran `cargo install tauri-cli`
// by hand, which is true on a developer machine and false on a CI runner. The npm
// binary is pinned by package-lock.json and is the same version everywhere.
const targets = bundles();
console.log(`\nBundling: ${targets}`);
const tauriCli = join(repo, "node_modules", ".bin", process.platform === "win32" ? "tauri.cmd" : "tauri");
if (!existsSync(tauriCli)) {
  console.error(`build-installer: Tauri CLI missing at ${tauriCli}; run npm ci`);
  process.exit(1);
}
execFileSync(
  tauriCli,
  ["build", "--bundles", targets, ...(triple === hostTriple ? [] : ["--target", triple])],
  { cwd: srcTauri, stdio: "inherit", env: process.env, shell: process.platform === "win32" },
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
