# Ubuntu: make a USB LTE modem ready for telesms-bot

The bot talks to **host** ModemManager over D-Bus. It does not open
`/dev/cdc-wdm0` itself. Any stick ModemManager can register is usable:
set `MODEM_UID` to that modem’s Device field.

Do **not** run ModemManager inside Docker. The container only mounts the
system D-Bus socket.

Tested with a D-Link DWM-222 (QMI). Other QMI/MBIM sticks that show up in
`mmcli -L` should work the same way.

---

## 1. Packages and udev

From a checkout of this repo:

```bash
sudo ./scripts/setup-ubuntu-modem.sh
```

That installs `usb-modeswitch` + `modemmanager`, enables ModemManager, and
adds your user to `dialout`.

### Stable name (recommended)

ModemManager indexes (`/Modem/0`) change on every replug. Pin a UID so
`MODEM_UID` stays valid:

```bash
# After the stick is in modem mode, copy VENDOR:PRODUCT from lsusb:
lsusb
sudo ./scripts/setup-ubuntu-modem.sh --uid stick --usb abcd:1234
```

Zero-CD sticks (virtual CD first, then modem) also need the storage-mode
id:

```bash
sudo ./scripts/setup-ubuntu-modem.sh --uid stick --usb abcd:1234 --storage abcd:5678
```

D-Link DWM-222 (sold as “DWM-22”):

```bash
sudo ./scripts/setup-ubuntu-modem.sh --preset dwm222
```

That sets UID `dwm222`, modem-mode `2001:7e3d`, storage `2001:ac01`.

The script writes:

| Path | Role |
|---|---|
| `/etc/udev/rules.d/40-telesms-modem.rules` | UID + device perms (`dialout`) |
| `/usr/share/usb_modeswitch/…` | StandardEject when `--storage` is set |
| `/usr/local/bin/telesms-modem` | `mmcli -m <uid>` wrapper |

Unplug and replug after a UID rule. Then:

```bash
lsusb
mmcli -L
mmcli -m stick          # or: telesms-modem
```

`System.device` must equal the UID you passed. Put that value in `.env`
as `MODEM_UID`.

---

## 2. Without a UID rule

```bash
mmcli -L
mmcli -m 0
```

Copy the **Device** line into `MODEM_UID`. It may be a sysfs path and can
change if you move the stick to another USB port. Prefer `--uid` + `--usb`.

---

## 3. Check the stick

Wait ~20 s after plug for probe + network registration.

```bash
mmcli -m "$MODEM_UID"     # or mmcli -m 0
# expect: state registered (or connected)
```

If PIN locked:

```bash
mmcli -i any --pin=1234
```

List / read / send (sanity check before starting the bot):

```bash
mmcli -m "$MODEM_UID" --messaging-list-sms
SMS=$(mmcli -m "$MODEM_UID" --messaging-create-sms="number=+98912xxxxxxx,text='hello'" | awk '{print $NF}')
mmcli -s "$SMS" --send
```

SIM missing or poorly seated shows up as ModemManager `failed` /
`sim-missing` and QMI `no-atr-received`. Reseat the tray.

Do **not** open `/dev/cdc-wdm0` with `qmicli` while ModemManager is running
unless you use `qmi-proxy`. Direct open returns `endpoint hangup`.

---

## 4. Point the bot at it

```bash
# .env
MODEM_UID=stick          # Device field / ID_MM_PHYSDEV_UID
```

Then run natively (`cargo run`) or with Compose
([deploy-docker.md](deploy-docker.md)).

VM USB passthrough: [proxmox-usb-passthrough.md](proxmox-usb-passthrough.md).
Docker + D-Bus on the VM: [ubuntu-vm-host-setup.md](ubuntu-vm-host-setup.md).

---

## Troubleshooting

| Symptom | What to do |
|---|---|
| Stuck as a USB CD / mass storage | Pass `--storage` with that VID:PID; `eject /dev/sr0`; check udev |
| `mmcli`: `sim-missing` | Reseat mini-SIM |
| `Couldn't get SIM lock status after 7 retries` | Same as missing SIM, or probe raced — unplug, wait 5 s, replug |
| `mmcli -m 0` gone after replug | Use a UID (`--uid` / `--usb`) |
| `mmcli -m <uid>` not found | `udevadm info` on the USB device should show `ID_MM_PHYSDEV_UID`; restart ModemManager |
| `qmicli`: endpoint hangup | Stop ModemManager or use `--device-open-proxy` |
| `telesms-bot`: modem not found | `MODEM_UID` must match `mmcli` Device exactly |
| Docker: AppArmor `AccessDenied` on D-Bus `Hello` | Compose must set `apparmor:unconfined` ([deploy-docker.md](deploy-docker.md)) |

---

## D-Link DWM-222 notes

This firmware stays in **QMI** on Linux. SMS goes through ModemManager, not
the stick’s RNDIS web UI (`192.168.0.1`).

| | |
|---|---|
| Zero-CD (first plug) | `2001:ac01` |
| Modem mode | `2001:7e3d` — `option` + `qmi_wwan` |
| Preset | `--preset dwm222` → UID `dwm222` |

`lsusb` after a good plug: `2001:7e3d D-Link Corp. Mobile Connect`.
