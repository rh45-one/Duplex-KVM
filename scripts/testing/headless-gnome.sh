#!/bin/sh
# Runs a command inside a private headless GNOME Shell (Mutter ScreenCast,
# PipeWire, Wayland) so video tests never touch the live desktop. Settings,
# caches and data live under target/headless-gnome; use with a private bus:
#
#   dbus-run-session -- scripts/testing/headless-gnome.sh COMMAND [ARGS...]
#
# Mirrors a typical GNOME setup: fractional scaling enabled, no welcome tour,
# overview closed. The virtual monitor is 1920x1200 (DUPLEX_TEST_MONITOR).
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
T="$ROOT/target/headless-gnome"
mkdir -p "$T/home" "$T/config" "$T/data" "$T/cache"
export HOME="$T/home" XDG_CONFIG_HOME="$T/config" XDG_DATA_HOME="$T/data" XDG_CACHE_HOME="$T/cache"
export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=GNOME
unset DISPLAY WAYLAND_DISPLAY
gsettings set org.gnome.shell welcome-dialog-last-shown-version "9999" 2>/dev/null
gsettings set org.gnome.mutter experimental-features "['scale-monitor-framebuffer', 'xwayland-native-scaling']" 2>/dev/null
gnome-shell --headless --no-x11 --wayland-display duplex-test \
    --virtual-monitor "${DUPLEX_TEST_MONITOR:-1920x1200}" >"$T/shell.log" 2>&1 &
SHELL_PID=$!
for _ in $(seq 1 50); do
    [ -S "$XDG_RUNTIME_DIR/duplex-test" ] && break
    sleep 0.2
done
export WAYLAND_DISPLAY=duplex-test
sleep 1
gdbus call --session --dest org.gnome.Shell --object-path /org/gnome/Shell \
    --method org.freedesktop.DBus.Properties.Set org.gnome.Shell OverviewActive "<false>" >/dev/null 2>&1
"$@"
STATUS=$?
kill "$SHELL_PID" 2>/dev/null
wait "$SHELL_PID" 2>/dev/null
exit $STATUS
