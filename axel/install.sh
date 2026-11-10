#!/bin/sh
# Install the Axel watcher, which opens a rex-git pane in every Axel thread
# window. Copies axel-git.lua into Rex's config directory and, on macOS,
# starts it now and at every login with a LaunchAgent.
#
#   axel/install.sh               install, or update after a change
#   axel/install.sh --uninstall   stop it and remove it
set -eu

label=dev.rex-git.axel-watcher
config="${XDG_CONFIG_HOME:-$HOME/.config}/rex"
script="$config/axel-git.lua"
plist="$HOME/Library/LaunchAgents/$label.plist"
here=$(cd "$(dirname "$0")" && pwd)

if [ "${1:-}" = "--uninstall" ]; then
  if [ "$(uname)" = Darwin ]; then
    launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
    rm -f "$plist"
  fi
  rm -f "$script"
  echo "Removed the Axel watcher."
  exit 0
fi

rex_bin=$(command -v rex) || { echo "rex isn't on your PATH. Install Rex first." >&2; exit 1; }
command -v rex-git >/dev/null || echo "Note: rex-git isn't on your PATH yet. Run: cargo install --path ." >&2
command -v axel >/dev/null || echo "Note: axel isn't on your PATH yet. See https://axel.md." >&2

mkdir -p "$config"
cp "$here/axel-git.lua" "$script"

if [ "$(uname)" != Darwin ]; then
  echo "Copied the watcher to $script. Keep it running with: rex do $script --all"
  exit 0
fi

mkdir -p "$(dirname "$plist")" "$HOME/Library/Logs"
cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$rex_bin</string>
    <string>do</string>
    <string>$script</string>
    <string>--all</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$PATH</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>15</integer>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/rex-git-axel-watcher.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/rex-git-axel-watcher.log</string>
</dict>
</plist>
EOF

launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist"
echo "The Axel watcher is running and will start at login."
echo "Log: ~/Library/Logs/rex-git-axel-watcher.log"
