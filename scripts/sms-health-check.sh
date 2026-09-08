#!/usr/bin/env bash
# telesms-bot health check: catches silently-lost inbound SMS end to end.
#
# The 2026-09-08 incident (see docs/incidents/2026-09-08-inbound-sms-silent-loss.md)
# dropped every inbound SMS with zero errors. This script sends an SMS to the
# modem's own number and verifies it comes back through the whole pipeline
# (modem -> ModemManager -> bot -> inbound_log). If it does not, the same
# failure class is happening again and the script alerts via Telegram.
#
# Also checks: API liveness, modem registration, stuck "receiving" SMS on the
# modem (should be swept by the bot within 6h), and contacts sync health.
#
# Usage:
#   sms-health-check.sh [--force-probe] [--skip-probe]
#
# Cron (daily 09:17):
#   17 9 * * * /home/amir/w/telesms-bot/scripts/sms-health-check.sh >> /home/amir/w/telesms-bot/data/health-check.log 2>&1
#
# Exit codes: 0 healthy, 1 unhealthy.

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$REPO_DIR/.env"
DB_PATH="$REPO_DIR/data/telesms.sqlite"
STATE_FILE="${XDG_CACHE_HOME:-$HOME/.cache}/telesms-health-probe"
PROBE_MIN_INTERVAL_SECS=$((6 * 3600))
PROBE_WAIT_SECS=150

FORCE_PROBE=0
SKIP_PROBE=0
for arg in "$@"; do
  case "$arg" in
    --force-probe) FORCE_PROBE=1 ;;
    --skip-probe) SKIP_PROBE=1 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

[ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE" >&2; exit 1; }
# shellcheck disable=SC1090
set -a; . "$ENV_FILE"; set +a

API_KEY="${API_KEY:-}"
API_PORT="${API_PORT:-8787}"
TELEGRAM_BOT_TOKEN="${TELEGRAM_BOT_TOKEN:-}"
TELEGRAM_GROUP_ID="${TELEGRAM_GROUP_ID:-}"

PROBLEMS=()

fail() {
  PROBLEMS+=("$1")
  echo "FAIL: $1"
}

info() { echo "ok:   $1"; }

tg_alert() {
  [ -n "$TELEGRAM_BOT_TOKEN" ] && [ -n "$TELEGRAM_GROUP_ID" ] || return 0
  curl -s -m 15 -X POST "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendMessage" \
    --data-urlencode "chat_id=${TELEGRAM_GROUP_ID}" \
    --data-urlencode "text=🚨 telesms-bot health check FAILED:
$1
Host: $(hostname), $(date -Is)" >/dev/null 2>&1 || true
}

# 1. API liveness
if curl -s -m 10 "http://127.0.0.1:${API_PORT}/health" | grep -qi 'ok'; then
  info "API /health responds"
else
  fail "API /health on 127.0.0.1:${API_PORT} is not responding — is the container up?"
fi

# 2. Status endpoint
STATUS="$(curl -s -m 10 -H "X-Api-Key: ${API_KEY}" "http://127.0.0.1:${API_PORT}/api/v1/status" 2>/dev/null || true)"
if [ -z "$STATUS" ]; then
  fail "GET /api/v1/status returned nothing (bad API_KEY or API down)"
else
  echo "ok:   status: $STATUS"
  if command -v python3 >/dev/null 2>&1; then
    STATE="$(printf '%s' "$STATUS" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("modem",{}).get("state","?"), d.get("modem",{}).get("sim","?"))' 2>/dev/null || echo '? ?')"
    if [ "$STATE" != "registered ok" ]; then
      fail "modem state is not registered/ok: '$STATE'"
    else
      info "modem registered, sim ok"
    fi
    if printf '%s' "$STATUS" | grep -q '"contacts_ok":false'; then
      echo "warn: contacts_ok=false — Google token revoked, re-run 'cargo run -- auth' (or docker compose run --rm telesms auth)"
    fi
  fi
fi

# 3. Stuck 'receiving' SMS on the modem (bot sweep should remove these within 6h)
if command -v mmcli >/dev/null 2>&1; then
  STUCK="$(mmcli -m any --messaging-list-sms 2>/dev/null | grep -c '(receiving)' || true)"
  if [ "${STUCK:-0}" -gt 0 ]; then
    fail "$STUCK SMS stuck in 'receiving' state on the modem — inbox sweep not cleaning up? (mmcli -m any --messaging-list-sms)"
  else
    info "no stuck 'receiving' SMS on modem"
  fi
  OWN_NUMBER="$(mmcli -m any 2>/dev/null | awk '/own:/ {print $NF; exit}' | tr -d ' ')"
else
  echo "warn: mmcli not available; skipping modem checks"
  OWN_NUMBER=""
fi

# 4. End-to-end loopback probe (the definitive silent-loss detector)
probe() {
  [ -n "$OWN_NUMBER" ] || { echo "warn: own number unknown; skipping probe"; return 0; }
  STAMP="$(date +%Y%m%dT%H%M%S)"
  TEXT="telesms health probe $STAMP"
  if ! curl -s -m 30 -X POST -H "X-Api-Key: ${API_KEY}" -H 'Content-Type: application/json' \
    -d "{\"number\":\"+${OWN_NUMBER#+}\",\"text\":\"$TEXT\"}" \
    "http://127.0.0.1:${API_PORT}/api/v1/sms" | grep -q '"sent":true'; then
    fail "probe SMS could not be sent (outbound path broken)"
    return 1
  fi
  echo "ok:   probe SMS sent to own number, waiting up to ${PROBE_WAIT_SECS}s for it to come back"
  local waited=0
  while [ "$waited" -lt "$PROBE_WAIT_SECS" ]; do
    sleep 5; waited=$((waited + 5))
    if python3 - "$DB_PATH" "$TEXT" <<'PY' 2>/dev/null | grep -q FOUND
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
row = conn.execute("SELECT 1 FROM inbound_log WHERE body = ?", (sys.argv[2],)).fetchone()
print("FOUND" if row else "MISSING")
PY
    then
      info "probe SMS recorded in inbound_log — inbound pipeline healthy"
      return 0
    fi
  done
  fail "probe SMS was sent but never appeared in inbound_log after ${PROBE_WAIT_SECS}s — inbound pipeline is silently dropping SMS (see docs/incidents/2026-09-08-inbound-sms-silent-loss.md)"
  return 1
}

RUN_PROBE=1
if [ "$SKIP_PROBE" -eq 1 ]; then
  RUN_PROBE=0
elif [ "$FORCE_PROBE" -ne 1 ] && [ -f "$STATE_FILE" ]; then
  LAST="$(cat "$STATE_FILE" 2>/dev/null || echo 0)"
  NOW="$(date +%s)"
  if [ -n "$LAST" ] && [ $((NOW - LAST)) -lt "$PROBE_MIN_INTERVAL_SECS" ]; then
    echo "ok:   skipping loopback probe (last ran $(( (NOW - LAST) / 60 )) min ago)"
    RUN_PROBE=0
  fi
fi

if [ "$RUN_PROBE" -eq 1 ] && probe; then
  mkdir -p "$(dirname "$STATE_FILE")"
  date +%s > "$STATE_FILE"
fi

# 5. Result
if [ "${#PROBLEMS[@]}" -gt 0 ]; then
  tg_alert "$(printf '%s\n' "${PROBLEMS[@]}")"
  echo "UNHEALTHY: ${#PROBLEMS[@]} problem(s)"
  exit 1
fi
echo "HEALTHY"
