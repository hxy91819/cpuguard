#!/bin/sh
set -eu

section() {
  printf '\n## %s\n' "$1"
}

section "thermal"
pmset -g therm 2>&1 || true

section "sleep assertions"
pmset -g assertions 2>&1 || true

section "top cpu processes"
ps -axo pid,ppid,user,%cpu,%mem,stat,comm,args -r 2>&1 | head -n "${CPUGUARD_SNAPSHOT_COUNT:-30}" || true

section "cpuguard default-domain view"
if command -v cpuguard >/dev/null 2>&1; then
  cpuguard watches 2>&1 || true
  cpuguard status 2>&1 || true
else
  echo "cpuguard not found on PATH"
fi

section "cpuguard launchd user labels"
launchctl print "gui/$(id -u)" 2>&1 | grep 'com\.cpuguard' || true

section "cpuguard processes"
ps -axo pid,ppid,user,%cpu,%mem,stat,comm,args 2>&1 | grep -E 'cpuguard|cpulimit' | grep -v grep || true

section "sudo-dependent checks"
echo "Run manually when sudo is available:"
echo "sudo cpuguard --domain system watches"
echo "sudo cpuguard --domain system status"
echo "sudo launchctl print system | grep 'com\\.cpuguard'"
