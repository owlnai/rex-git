#!/bin/sh
# Install the Axel watcher, which opens a rex-git pane in every Axel thread
# window. Installs rex-git from the latest release if it's missing, copies
# axel-git.lua into Rex's config directory and, on macOS, starts it now and
# at every login with a LaunchAgent. Works from a clone or piped from curl:
#
#   curl -LsSf https://raw.githubusercontent.com/owlnai/rex-git/main/axel/install.sh | sh
#   axel/install.sh               install, or update after a change
#   axel/install.sh --uninstall   stop it and remove it
set -eu

label=dev.rex-git.axel-watcher
config="${XDG_CONFIG_HOME:-$HOME/.config}/rex"
script="$config/axel-git.lua"
plist="$HOME/Library/LaunchAgents/$label.plist"
repo=https://github.com/owlnai/rex-git
here=$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo .)

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
if ! command -v rex-git >/dev/null && [ ! -x "$HOME/.cargo/bin/rex-git" ]; then
  echo "Installing rex-git from the latest release..."
  curl -LsSf "$repo/releases/latest/download/rex-git-installer.sh" | sh
fi
command -v axel >/dev/null || echo "Note: axel isn't on your PATH yet. See https://axel.md." >&2

mkdir -p "$config"
if [ -f "$here/axel-git.lua" ]; then
  cp "$here/axel-git.lua" "$script"
else
  curl -LsSf "https://raw.githubusercontent.com/owlnai/rex-git/main/axel/axel-git.lua" -o "$script"
fi

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
# bootout returns before the old job is gone, and bootstrap fails until it is.
i=0
while launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1 && [ $i -lt 50 ]; do
  sleep 0.1
  i=$((i + 1))
done
launchctl bootstrap "gui/$(id -u)" "$plist"
echo "The Axel watcher is running and will start at login."
echo "Log: ~/Library/Logs/rex-git-axel-watcher.log"
