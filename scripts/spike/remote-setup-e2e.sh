#!/bin/bash
# Runs the ignored `sets_a_host_up_and_binds_a_project` test against a throwaway sshd on this
# computer: the whole remote-workspace setup (host key, probe, helper install with its checksum,
# folders, project, a real call to the helper) with nothing to prepare.
#
#   scripts/spike/remote-setup-e2e.sh            # uses src-tauri/target/debug/oleafly-remoted
#   HELPER=/path/to/oleafly-remoted scripts/spike/remote-setup-e2e.sh
#   KEEP=1 ...                                   # leave the scratch host home in place
#
# Build the helper first: cargo build -p oleafly --features remote-spike --bin oleafly-remoted
# What it starts: sshd on 127.0.0.1:22022 for this user only, key login with a key made here, and
# an ssh-agent holding that key. Both are stopped on exit. The "host" is this computer, so its
# $HOME is a short scratch folder (the helper keeps a unix socket under it, which has a length limit).
set -u
repo="$(cd "$(dirname "$0")/../.." && pwd)"
helper="${HELPER:-$repo/src-tauri/target/debug/oleafly-remoted}"
[ -x "$helper" ] || { echo "no helper at $helper: build it first" >&2; exit 2; }
sshd=/usr/sbin/sshd
[ -x "$sshd" ] || sshd="$(command -v sshd)" || { echo "sshd not found" >&2; exit 2; }
conf="$(mktemp -d)"
home="$HOME/.oleafly-e2e-home"
rm -rf "$home"; mkdir -p "$home"
cleanup() {
  [ -f "$conf/sshd.pid" ] && kill "$(cat "$conf/sshd.pid")" 2>/dev/null
  [ -n "${SSH_AGENT_PID:-}" ] && kill "$SSH_AGENT_PID" 2>/dev/null
  pkill -f "$home/.oleafly/remoted" 2>/dev/null
  rm -rf "$conf"
  [ "${KEEP:-0}" = 1 ] || rm -rf "$home"
}
trap cleanup EXIT
ssh-keygen -q -t ed25519 -N '' -f "$conf/hostkey"
ssh-keygen -q -t ed25519 -N '' -f "$conf/userkey"
cp "$conf/userkey.pub" "$conf/authorized_keys"; chmod 600 "$conf/authorized_keys"
cat > "$conf/sshd_config" <<CFG
Port 22022
ListenAddress 127.0.0.1
HostKey $conf/hostkey
PidFile $conf/sshd.pid
AuthorizedKeysFile $conf/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
StrictModes no
MaxAuthTries 30
SetEnv HOME=$home
LogLevel ERROR
CFG
"$sshd" -f "$conf/sshd_config" -E "$conf/sshd.log" || { echo "sshd did not start"; cat "$conf/sshd.log"; exit 1; }
eval "$(ssh-agent -s)" >/dev/null
ssh-add -q "$conf/userkey"
sleep 1
cd "$repo/src-tauri" || exit 1
REMOTE_SETUP_HOST=127.0.0.1 REMOTE_SETUP_PORT=22022 REMOTE_SETUP_USER="$(whoami)" REMOTE_SETUP_HELPER="$helper" \
  cargo test -p oleafly --features remote-spike,remote-bridge --lib \
  remote_bridge::setup::tests::sets_a_host_up -- --ignored --nocapture
