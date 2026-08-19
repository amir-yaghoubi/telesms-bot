#!/usr/bin/env bash
# Prepare Ubuntu so ModemManager can own a USB LTE stick for telesms-bot.
# Model-agnostic. D-Link DWM-222 is --preset dwm222.
set -euo pipefail

ORIG_ARGS=("$@")

usage() {
  cat <<'EOF'
Usage: sudo ./scripts/setup-ubuntu-modem.sh [options]

Always installs usb-modeswitch + modemmanager and enables ModemManager.

Optional stable UID (survives replug). Needs the stick's *modem-mode* USB id:

  --uid NAME                 ModemManager Device / MODEM_UID (e.g. telesms)
  --usb VENDOR:PRODUCT       Modem-mode USB id from lsusb (e.g. 2001:7e3d)
  --storage VENDOR:PRODUCT   Zero-CD storage id to eject (e.g. 2001:ac01)
  --preset dwm222            D-Link DWM-222: uid dwm222, usb 2001:7e3d,
                             storage 2001:ac01
  --user NAME                Add this user to dialout (default: sudo caller)

Examples:
  sudo ./scripts/setup-ubuntu-modem.sh
  sudo ./scripts/setup-ubuntu-modem.sh --preset dwm222
  sudo ./scripts/setup-ubuntu-modem.sh --uid stick --usb 12d1:15c1
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

is_usb_id() {
  [[ "${1,,}" =~ ^[0-9a-f]{4}:[0-9a-f]{4}$ ]]
}

split_usb() {
  local id="${1,,}"
  is_usb_id "$id" || die "USB id must look like abcd:1234, got: $1"
  printf '%s %s' "${id%%:*}" "${id##*:}"
}

UID_NAME=""
USB_ID=""
STORAGE_ID=""
PRESET=""
TARGET_USER="${SUDO_USER:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --uid)
      UID_NAME="${2:?}"
      shift 2
      ;;
    --usb)
      USB_ID="${2:?}"
      shift 2
      ;;
    --storage)
      STORAGE_ID="${2:?}"
      shift 2
      ;;
    --preset)
      PRESET="${2:?}"
      shift 2
      ;;
    --user)
      TARGET_USER="${2:?}"
      shift 2
      ;;
    *)
      die "unknown argument: $1 (try --help)"
      ;;
  esac
done

if [[ "$(id -u)" -ne 0 ]]; then
  exec sudo -- "$0" "${ORIG_ARGS[@]}"
fi

if [[ -f /etc/os-release ]]; then
  # shellcheck source=/dev/null
  . /etc/os-release
  case "${ID:-}:${ID_LIKE:-}" in
    ubuntu:* | debian:* | *:debian* | linuxmint:* | pop:*) ;;
    *)
      echo "warning: this script is aimed at Ubuntu/Debian (id=${ID:-unknown})" >&2
      ;;
  esac
fi

case "$PRESET" in
  "") ;;
  dwm222)
    UID_NAME="${UID_NAME:-dwm222}"
    USB_ID="${USB_ID:-2001:7e3d}"
    STORAGE_ID="${STORAGE_ID:-2001:ac01}"
    ;;
  *)
    die "unknown preset: $PRESET (known: dwm222)"
    ;;
esac

if [[ -n "$STORAGE_ID" && -z "$USB_ID" ]]; then
  die "--storage requires --usb (or --preset)"
fi
if [[ -n "$USB_ID" && -z "$UID_NAME" ]]; then
  die "--usb requires --uid so MODEM_UID has a stable name"
fi
if [[ -n "$UID_NAME" && -z "$USB_ID" ]]; then
  die "--uid requires --usb (the modem-mode VENDOR:PRODUCT from lsusb)"
fi
if [[ -n "$UID_NAME" && ! "$UID_NAME" =~ ^[A-Za-z0-9._-]+$ ]]; then
  die "--uid must be a simple token (letters, digits, . _ -)"
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y usb-modeswitch modemmanager
systemctl enable --now ModemManager

if [[ -n "$TARGET_USER" ]] && id "$TARGET_USER" >/dev/null 2>&1; then
  usermod -aG dialout "$TARGET_USER"
  echo "Added $TARGET_USER to dialout (log out and back in for AT/qmicli)."
fi

if [[ -n "$USB_ID" ]]; then
  read -r VENDOR PRODUCT <<<"$(split_usb "$USB_ID")"
  if [[ -n "$STORAGE_ID" ]]; then
    read -r STOR_V STOR_P <<<"$(split_usb "$STORAGE_ID")"
    cat >"/usr/share/usb_modeswitch/${STOR_V}:${STOR_P}" <<EOF
# telesms-bot usb_modeswitch (${STOR_V}:${STOR_P} → ${VENDOR}:${PRODUCT})
TargetVendor=0x${VENDOR}
TargetProduct=0x${PRODUCT}
StandardEject=1
EOF
    chmod 644 "/usr/share/usb_modeswitch/${STOR_V}:${STOR_P}"
    echo "Wrote /usr/share/usb_modeswitch/${STOR_V}:${STOR_P}"
  fi

  UDEV_RULE=/etc/udev/rules.d/40-telesms-modem.rules
  {
    echo "# telesms-bot: stable ModemManager UID ${UID_NAME}"
    echo "# Modem-mode USB ${VENDOR}:${PRODUCT}"
    if [[ -n "$STORAGE_ID" ]]; then
      echo "# Zero-CD storage ${STOR_V}:${STOR_P} → eject to modem mode"
      echo "ACTION==\"add\", SUBSYSTEM==\"block\", ENV{ID_VENDOR_ID}==\"${STOR_V}\", ENV{ID_MODEL_ID}==\"${STOR_P}\", ENV{ID_CDROM}==\"1\", RUN+=\"/usr/bin/eject /dev/%k\""
      echo
    fi
    cat <<EOF
ACTION=="add|change|bind|move", SUBSYSTEM=="usb", ATTR{idVendor}=="${VENDOR}", ATTR{idProduct}=="${PRODUCT}", ENV{ID_MM_PHYSDEV_UID}="${UID_NAME}"
ACTION=="add|change|bind|move", SUBSYSTEMS=="usb", ATTRS{idVendor}=="${VENDOR}", ATTRS{idProduct}=="${PRODUCT}", ENV{ID_MM_PHYSDEV_UID}="${UID_NAME}"
ACTION=="add", SUBSYSTEM=="tty", ATTRS{idVendor}=="${VENDOR}", ATTRS{idProduct}=="${PRODUCT}", GROUP="dialout", MODE="0660", TAG+="uaccess"
ACTION=="add", SUBSYSTEM=="usbmisc", ATTRS{idVendor}=="${VENDOR}", ATTRS{idProduct}=="${PRODUCT}", GROUP="dialout", MODE="0660", TAG+="uaccess"
EOF
  } >"$UDEV_RULE"
  chmod 644 "$UDEV_RULE"
  echo "Wrote $UDEV_RULE"
  udevadm control --reload-rules
  udevadm trigger

  cat >"/usr/local/bin/telesms-modem" <<EOF
#!/usr/bin/env bash
# Address the stick by stable UID, not the incrementing ModemManager index.
exec mmcli -m ${UID_NAME} "\$@"
EOF
  chmod 755 /usr/local/bin/telesms-modem
  echo "Wrote /usr/local/bin/telesms-modem → mmcli -m ${UID_NAME}"
fi

echo
echo "ModemManager is running."
if [[ -n "$UID_NAME" ]]; then
  echo "Unplug and replug the stick, wait ~20s, then:"
  echo "  lsusb"
  echo "  mmcli -L"
  echo "  mmcli -m ${UID_NAME}"
  echo "Set MODEM_UID=${UID_NAME} in .env"
else
  echo "Plug the stick, wait ~20s, then:"
  echo "  mmcli -L"
  echo "  mmcli -m 0"
  echo "Copy the Device field into MODEM_UID in .env"
  echo "For a stable name across replugs, re-run with --uid and --usb VENDOR:PRODUCT"
fi
echo "See docs/ubuntu-modem-setup.md"
