#!/bin/sh
# Run the CEF spike the way CEF requires on macOS: from inside an .app bundle,
# with the framework in Contents/Frameworks. A bare binary fails here in ways
# that look like CEF is broken.
#
# Usage: ./run-macos.sh [--multi-threaded]
set -eu

cd "$(dirname "$0")"

cargo build --release

APP="build/RustBoxCefSpike.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks"
cp target/release/cef-spike "$APP/Contents/MacOS/"

# The framework and the helper apps CEF resolves its sub-processes through. A
# missing helper makes page loads hang rather than fail loudly, so the copy is
# checked instead of silenced.
if [ -d "target/release/Chromium Embedded Framework.framework" ]; then
  cp -R "target/release/Chromium Embedded Framework.framework" \
        "target/release/"*.framework "$APP/Contents/Frameworks/" 2>/dev/null || true
else
  echo "FAIL: no Chromium Embedded Framework in target/release" >&2
  exit 1
fi

for helper in "target/release/"*" Helper"*.app; do
  [ -d "$helper" ] || continue
  cp -R "$helper" "$APP/Contents/Frameworks/"
done

# An Info.plist is what makes it a bundle. Minimum keys only.
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>cef-spike</string>
  <key>CFBundleIdentifier</key><string>app.rustbox.cefspike</string>
  <key>CFBundleName</key><string>RustBoxCefSpike</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc signature so the framework loads without a Developer ID. This is enough
# for local runs; distribution would need a real identity and notarization.
codesign --force --deep --sign - "$APP" 2>/dev/null || \
  echo "note: ad-hoc signing failed; the run may still work locally"

echo "running $APP"
exec "$APP/Contents/MacOS/cef-spike" "$@"
