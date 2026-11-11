#!/bin/bash
# Builds dist/Dropfin.app (universal: Apple Silicon + Intel), ad-hoc signs it,
# and packages dist/Dropfin.dmg and dist/Dropfin-mac.zip. Run on macOS.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
export MACOSX_DEPLOYMENT_TARGET=11.0

rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --locked --target aarch64-apple-darwin
cargo build --release --locked --target x86_64-apple-darwin

rm -rf dist
APP="dist/Dropfin.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/Dropfin" \
  target/aarch64-apple-darwin/release/dropfin \
  target/x86_64-apple-darwin/release/dropfin
sed "s/__VERSION__/$VERSION/g" macos/Info.plist > "$APP/Contents/Info.plist"
cp macos/Dropfin.icns "$APP/Contents/Resources/Dropfin.icns"
plutil -lint "$APP/Contents/Info.plist"

# Ad-hoc signature (no Apple developer account needed). Required for Apple Silicon.
codesign --force --deep --sign - "$APP"
codesign --verify --verbose "$APP"

ditto -c -k --keepParent "$APP" dist/Dropfin-mac.zip

mkdir -p dist/dmg
cp -R "$APP" dist/dmg/
ln -s /Applications dist/dmg/Applications
hdiutil create -volname "Dropfin" -srcfolder dist/dmg -ov -format UDZO dist/Dropfin.dmg
rm -rf dist/dmg

lipo -info "$APP/Contents/MacOS/Dropfin"
ls -lh dist
