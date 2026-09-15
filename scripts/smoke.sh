#!/bin/sh
# End-to-end smoke test: fake Windows env vars on a scratch dir.
set -e
cd "$(dirname "$0")/.."
FAKE="$PWD/.smoke"
rm -rf "$FAKE"
mkdir -p "$FAKE/AppData/Local/pip/cache/wheels" \
         "$FAKE/AppData/Local/npm-cache" \
         "$FAKE/AppData/Local/Google/Chrome/User Data/Default/Cache" \
         "$FAKE/AppData/Local/JetBrains/IdeaIC2026.1/caches" \
         "$FAKE/AppData/Roaming/dustpan" \
         "$FAKE/Temp"
dd if=/dev/zero of="$FAKE/AppData/Local/pip/cache/wheels/big.whl" bs=1024 count=300 2>/dev/null
dd if=/dev/zero of="$FAKE/AppData/Local/npm-cache/pkg.tgz" bs=1024 count=120 2>/dev/null
dd if=/dev/zero of="$FAKE/AppData/Local/npm-cache/precious.tgz" bs=1024 count=40 2>/dev/null
dd if=/dev/zero of="$FAKE/AppData/Local/Google/Chrome/User Data/Default/Cache/data_1" bs=1024 count=50 2>/dev/null
dd if=/dev/zero of="$FAKE/AppData/Local/JetBrains/IdeaIC2026.1/caches/idx" bs=1024 count=30 2>/dev/null
dd if=/dev/zero of="$FAKE/Temp/tmpfile" bs=1024 count=10 2>/dev/null
echo "*precious*" > "$FAKE/AppData/Roaming/dustpan/whitelist.txt"

export USERPROFILE="$FAKE" LOCALAPPDATA="$FAKE/AppData/Local" \
       APPDATA="$FAKE/AppData/Roaming" TEMP="$FAKE/Temp" TMP="$FAKE/Temp"
BIN=./target/debug/dpan

echo "=== list ===";     "$BIN" --list --no-color
echo "=== dry run ===";  "$BIN" --dry-run --no-color
echo "=== real run ==="; "$BIN" --yes --no-color --verbose
echo "=== assertions ==="
test -f "$FAKE/AppData/Local/npm-cache/precious.tgz" && echo "OK whitelisted file survived"
# npm 属于高重新下载成本缓存；--yes 采用 Smart Clean 默认计划，不应静默删除。
test -f "$FAKE/AppData/Local/npm-cache/pkg.tgz" && echo "OK expensive cache stayed optional"
test ! -f "$FAKE/Temp/tmpfile" && echo "OK temp file removed"
test -d "$FAKE/AppData/Local/pip/cache" && echo "OK target dir itself kept"
echo "=== audit log ==="
sed "s|$FAKE|<FAKE>|" "$LOCALAPPDATA/dustpan/operations.log"
rm -rf "$FAKE"
echo "SMOKE OK"
