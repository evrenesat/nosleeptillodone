#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
destination_dir="${1:-"$repo_root/target/release"}"
app_name="No Sleep Till Done"
app_path="$destination_dir/$app_name.app"
contents_dir="$app_path/Contents"
macos_dir="$contents_dir/MacOS"
resources_dir="$contents_dir/Resources"

cargo build --release --bins --manifest-path "$repo_root/Cargo.toml"

rm -rf "$app_path"
mkdir -p "$macos_dir" "$resources_dir/launchd"
install -m 755 "$repo_root/target/release/no-sleep-till-done-menubar" "$macos_dir/no-sleep-till-done-menubar"
install -m 755 "$repo_root/target/release/no-sleep-till-done" "$resources_dir/no-sleep-till-done"
install -m 755 "$repo_root/target/release/no-sleep-till-done-watchdog" "$resources_dir/no-sleep-till-done-watchdog"
install -m 644 "$repo_root/launchd/com.evren.nosleeptilldone.plist" "$resources_dir/launchd/com.evren.nosleeptilldone.plist"
install -m 644 "$repo_root/launchd/com.evren.nosleeptilldone.watchdog.plist" "$resources_dir/launchd/com.evren.nosleeptilldone.watchdog.plist"

cat > "$contents_dir/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDisplayName</key>
  <string>No Sleep Till Done</string>

  <key>CFBundleExecutable</key>
  <string>no-sleep-till-done-menubar</string>

  <key>CFBundleIdentifier</key>
  <string>com.evren.nosleeptilldone</string>

  <key>CFBundleName</key>
  <string>No Sleep Till Done</string>

  <key>CFBundlePackageType</key>
  <string>APPL</string>

  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>

  <key>CFBundleVersion</key>
  <string>0.1.0</string>

  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>

  <key>LSUIElement</key>
  <true/>

  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

echo "$app_path"
