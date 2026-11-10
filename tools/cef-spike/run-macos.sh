#!/bin/sh
# Run the CEF spike the way CEF requires on macOS: from inside a .app bundle whose
# Contents/Frameworks holds the framework plus five helper .app bundles.
#
# The layout, the helper names and the Info.plist keys below are ported from the
# cef crate itself (cef-156.4.0+156.0.26, src/build_util/mac.rs), which is what its
# own `bundle-cef-app` tool produces. Guessing this layout does not work, and the
# failure looks like CEF being broken rather than a file being missing:
#
#   * the crate does NOT copy the CEF runtime next to the binary on macOS (it does
#     on Windows and Linux); the distribution stays in the build script's out dir;
#   * CEF resolves its five helper processes through helper .app bundles that do
#     not exist until this script makes them;
#   * a page load hangs with no error at all when a helper is missing.
#
# No Apple Developer ID is needed: an ad-hoc signature runs locally. See
# SIGNING-WITHOUT-DEVELOPER-ID.md for what that does and does not allow.
#
# Usage: ./run-macos.sh [--multi-threaded] [url]
set -eu

cd "$(dirname "$0")"

APP_NAME="cef-spike"
BUNDLE="build/$APP_NAME.app"
BUNDLE_ID="app.rustbox.cefspike"
FW_NAME="Chromium Embedded Framework.framework"

echo "== 1/6 build"
cargo build --release

echo "== 2/6 locate the CEF distribution"
# The cef-dll-sys build script unpacks the distribution into its own OUT_DIR and,
# on macOS only, does NOT copy it next to the binary (on Windows and Linux it
# does). So the framework has to be taken from the build directory.
CEF_DIST=""
for cand in target/release/build/cef-dll-sys-*/out/cef_macos_*; do
  if [ -d "$cand/$FW_NAME" ]; then
    CEF_DIST="$cand"
    break
  fi
done
if [ -z "$CEF_DIST" ] && [ -n "${CEF_PATH:-}" ]; then
  for cand in "$CEF_PATH" "$CEF_PATH"/*/cef_macos_*; do
    if [ -d "$cand/$FW_NAME" ]; then
      CEF_DIST="$cand"
      break
    fi
  done
fi
if [ -z "$CEF_DIST" ]; then
  # Last resort: wherever the framework ended up under target/. Slower, but it
  # turns "unexpected layout" into a working run instead of a failed one.
  found=$(find target -maxdepth 8 -type d -name "$FW_NAME" 2>/dev/null | head -1 || true)
  if [ -n "$found" ]; then
    CEF_DIST=$(dirname "$found")
  fi
fi
if [ -z "$CEF_DIST" ]; then
  echo "FAIL: no $FW_NAME found." >&2
  echo "      Looked in target/release/build/cef-dll-sys-*/out/cef_macos_*," >&2
  echo "      under CEF_PATH, and anywhere under target/." >&2
  echo "      Re-run 'cargo build --release' and read its output: the download" >&2
  echo "      and unpack step is what puts the framework there." >&2
  exit 1
fi
echo "   from: $CEF_DIST"

echo "== 3/6 assemble the bundle"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Frameworks" "$BUNDLE/Contents/Resources"
cp "target/release/$APP_NAME" "$BUNDLE/Contents/MacOS/"

# A framework is a bundle of symlinks into Versions/A, so it must be copied with
# those symlinks intact. ditto is the tool that does that correctly; `cp -R` is the
# fallback (`cp -R -L` would dereference and produce a bundle codesign rejects).
if command -v ditto >/dev/null 2>&1; then
  ditto "$CEF_DIST/$FW_NAME" "$BUNDLE/Contents/Frameworks/$FW_NAME"
else
  cp -R "$CEF_DIST/$FW_NAME" "$BUNDLE/Contents/Frameworks/$FW_NAME"
fi

# Info.plist keys as the crate writes them, including the ones that are not
# obvious: MallocNanoZone=0 in LSEnvironment, and LSFileQuarantineEnabled.
# LSUIElement keeps the helpers out of the Dock.
write_plist() {
  plist_path="$1"
  executable="$2"
  is_helper="$3"
  ui_element=""
  if [ "$is_helper" = "1" ]; then
    ui_element="<key>LSUIElement</key><string>1</string>"
  fi
  cat > "$plist_path" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>English</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleExecutable</key><string>$executable</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.0.0</string>
  <key>CFBundleSignature</key><string>????</string>
  <key>CFBundleVersion</key><string>0.0.0</string>
  <key>LSEnvironment</key><dict><key>MallocNanoZone</key><string>0</string></dict>
  <key>LSFileQuarantineEnabled</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSBluetoothAlwaysUsageDescription</key><string>$APP_NAME</string>
  <key>NSCameraUsageDescription</key><string>$APP_NAME</string>
  <key>NSMicrophoneUsageDescription</key><string>$APP_NAME</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSWebBrowserPublicKeyCredentialUsageDescription</key><string>$APP_NAME</string>
  $ui_element
</dict>
</plist>
PLIST
}

write_plist "$BUNDLE/Contents/Info.plist" "$APP_NAME" 0

# The five helper bundles. CEF builds each helper's path from the main bundle's
# executable name plus these suffixes, so the names and the binary inside each app
# must match exactly. One binary serves all of them: a helper is launched with
# --type=<kind>, and this binary's own main() hands that to execute_process and
# exits, which is the whole job of a sub-process.
helper_list() {
  cat <<'LIST'
Helper
Helper (GPU)
Helper (Renderer)
Helper (Plugin)
Helper (Alerts)
LIST
}

HELPER_COUNT=0
while IFS= read -r suffix; do
  [ -n "$suffix" ] || continue
  helper_name="$APP_NAME $suffix"
  helper_app="$BUNDLE/Contents/Frameworks/$helper_name.app"
  mkdir -p "$helper_app/Contents/MacOS"
  cp "target/release/$APP_NAME" "$helper_app/Contents/MacOS/$helper_name"
  write_plist "$helper_app/Contents/Info.plist" "$helper_name" 1
  HELPER_COUNT=$((HELPER_COUNT + 1))
done <<EOF
$(helper_list)
EOF
echo "   helpers created: $HELPER_COUNT (CEF needs all five)"

echo "== 4/6 sign, inside out"
# Nested code first, the outer bundle last: signing anything after the outer bundle
# invalidates its signature, and `codesign --verify --strict` then fails.
#
# No --options runtime and no entitlements on purpose. Nothing here is notarized or
# distributed, so the hardened runtime would only add failure modes (it enables
# library validation, which rejects a prebuilt framework signed by someone else).
# A notarized build needs the hardened runtime, JIT entitlements *and* a real
# Developer ID; the spike needs none of the three.
sign() {
  codesign --force --sign - --timestamp=none "$1" 2>/dev/null || {
    echo "FAIL: codesign refused $1" >&2
    echo "      reproduce with: codesign --force --sign - \"$1\"" >&2
    exit 1
  }
}

# The framework ships loose dylibs under Versions/A/Libraries. `--verify --strict`
# walks them as nested code, so they are signed before the framework containing
# them.
FW_DIR="$BUNDLE/Contents/Frameworks/$FW_NAME/Versions/A"
if [ -d "$FW_DIR/Libraries" ]; then
  for lib in "$FW_DIR/Libraries/"*.dylib; do
    if [ -f "$lib" ]; then
      sign "$lib"
    fi
  done
fi
sign "$BUNDLE/Contents/Frameworks/$FW_NAME"

while IFS= read -r suffix; do
  [ -n "$suffix" ] || continue
  sign "$BUNDLE/Contents/Frameworks/$APP_NAME $suffix.app"
done <<EOF
$(helper_list)
EOF

# The main executable, then the bundle that contains everything above.
sign "$BUNDLE/Contents/MacOS/$APP_NAME"
sign "$BUNDLE"

echo "== 5/6 verify"
codesign --verify --strict --verbose=2 "$BUNDLE" 2>&1 || {
  echo "FAIL: signature does not verify; CEF will refuse to load the framework" >&2
  exit 1
}

# A quarantine flag from an earlier download blocks the run. This bundle was just
# built locally, so clearing it is safe.
xattr -dr com.apple.quarantine "$BUNDLE" 2>/dev/null || true

echo "== 6/6 run"
echo "   pumps= must keep climbing. A stall is the failure M0 is looking for."
echo "   The window may open behind your terminal; the log is the point."
echo

# Run the binary directly rather than through `open`, so the heartbeat reaches
# stdout. Tee it as well, because the interesting output is what the user has to
# paste back, and a 60 s run is easy to lose by scrolling.
LOG="build/run.log"
mkdir -p build
STATUS=0
"$BUNDLE/Contents/MacOS/$APP_NAME" "$@" 2>&1 | tee "$LOG" || STATUS=$?
echo
echo "exit=$STATUS  log=$LOG"
