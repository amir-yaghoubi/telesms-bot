#!/usr/bin/env bash
# Enable the modem and wait for network registration.
# ModemManager probes the stick at boot but leaves it `disabled`; the bot then
# logs "Wrong state: modem in disabled state" and cannot send or receive SMS
# until someone runs `mmcli --enable` by hand. Installed as a systemd oneshot
# (telesms-modem-enable.service) by scripts/setup-ubuntu-modem.sh so this
# happens automatically on every boot.
set -euo pipefail

TIMEOUT="${TELESMS_MODEM_ENABLE_TIMEOUT:-300}"
POLL=5

log() { echo "telesms-modem-enable: $*"; }

deadline=$(( $(date +%s) + TIMEOUT ))

modem_state() {
  mmcli -m any -J 2>/dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin).get("modem", {})
if isinstance(d, list):  # older mmcli versions wrap the object in a list
    d = d[0] if d else {}
print(d.get("generic", {}).get("state", ""))
' 2>/dev/null || true
}

while true; do
  left=$(( deadline - $(date +%s) ))
  if (( left <= 0 )); then
    log "timed out; last state: $(modem_state || echo unknown)"
    exit 1
  fi

  state="$(modem_state)"
  case "$state" in
    registered | connected)
      log "modem $state — ready"
      exit 0
      ;;
    disabled)
      log "enabling modem (${left}s left)"
      mmcli -m any --enable
      ;;
    "")
      log "no modem probed yet, waiting (${left}s left)"
      ;;
    *)
      log "state=$state, waiting (${left}s left)"
      ;;
  esac
  sleep "$POLL"
done
