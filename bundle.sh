#!/bin/sh
# Builds target/Hop.app. `./bundle.sh install` also replaces /Applications/Hop.app and opens it.
set -e
cd "$(dirname "$0")"
cargo build --release
APP=target/Hop.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/hop "$APP/Contents/MacOS/hop"
cp Info.plist "$APP/Contents/Info.plist"
codesign --force --sign - "$APP"
echo "built $APP"
if [ "$1" = install ]; then
  pkill -f /Applications/Hop.app/Contents/MacOS/hop || true # a running copy keeps the old code
  rm -rf /Applications/Hop.app
  cp -R "$APP" /Applications/Hop.app
  open /Applications/Hop.app # the first time, macOS asks to make it the default browser
  echo "installed /Applications/Hop.app"
fi
