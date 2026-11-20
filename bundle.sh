#!/bin/sh
# ./bundle.sh          build target/Hop.app
# ./bundle.sh install  build, install Hop.app and the `hop` command, pick favorites, make Hop the default browser
# After that, use the `hop` command (`hop help`).
set -e
cd "$(dirname "$0")"
APP=/Applications/Hop.app
HOP=$APP/Contents/MacOS/hop
BIN=$HOME/.local/bin
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

cargo build --release
rm -rf target/Hop.app
mkdir -p target/Hop.app/Contents/MacOS
cp target/release/hop target/Hop.app/Contents/MacOS/hop
cp Info.plist target/Hop.app/Contents/Info.plist
codesign --force --sign - target/Hop.app
echo "built target/Hop.app"

case "${1:-}" in
  "") ;;
  install)
    pkill -x hop || true # a running copy keeps the old code
    rm -rf "$APP"
    cp -R target/Hop.app "$APP"
    "$LSREGISTER" -u target/Hop.app 2>/dev/null || true # links must never open the build copy
    "$LSREGISTER" -f "$APP"
    mkdir -p "$BIN"
    ln -sf "$HOP" "$BIN/hop"
    case ":$PATH:" in *":$BIN:"*) ;; *) echo "Add $BIN to your PATH to use the hop command." ;; esac
    "$HOP" favorites
    "$HOP" default
    open "$APP"
    echo "installed $APP and $BIN/hop" ;;
  *) sed -n '2,4p' "$0" | sed 's/^# //'; exit 1 ;;
esac
