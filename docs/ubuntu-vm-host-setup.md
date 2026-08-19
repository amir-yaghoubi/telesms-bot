# Ubuntu VM: host + Docker

ModemManager on the VM owns the USB stick. Compose deploy:
[deploy-docker.md](deploy-docker.md). USB passthrough:
[proxmox-usb-passthrough.md](proxmox-usb-passthrough.md).

---

## 1. Make the modem ready

On the VM, from a checkout of this repo:

```bash
sudo ./scripts/setup-ubuntu-modem.sh
# D-Link DWM-222:
# sudo ./scripts/setup-ubuntu-modem.sh --preset dwm222
```

Details, UID pinning, and checks: [ubuntu-modem-setup.md](ubuntu-modem-setup.md).

Do **not** run ModemManager inside Docker. The VM host owns the USB
device; the container only talks D-Bus.

---

## 2. D-Bus for the container

The Compose service mounts:

```text
/var/run/dbus/system_bus_socket
```

and sets `DBUS_SYSTEM_BUS_ADDRESS=unix:path=/var/run/dbus/system_bus_socket`.

If the container is not root, add a `/etc/dbus-1/system.d/` policy so that
uid can call `org.freedesktop.ModemManager1`. Root-in-container on the
system bus is enough for a first deploy.

Do not pass `/dev/cdc-wdm0` or `ttyUSB*` into the container.

---

## 3. Compose

See [deploy-docker.md](deploy-docker.md). Shape: multi-stage build on the
VM, one service, `restart: unless-stopped`, D-Bus **directory** mount
(`/var/run/dbus`), `./data` + `./secrets` volumes, env from `.env`, no
published ports. Set `TZ` in `compose.yaml` to the host timezone.

Google OAuth is done once on a machine with a browser; copy the refresh
token file onto the VM. The daemon must not start an interactive OAuth
loop in production.

---

## 4. Cut-over from a native daemon

1. Confirm SMS still works on the current host (`mmcli -m "$MODEM_UID"`).
2. Stop the native daemon (same bot token cannot run twice).
3. Unplug the stick, pass the USB **port** through on Proxmox, apply this
   file on the VM.
4. Confirm `mmcli` on the VM (one inbound, one outbound).
5. Start Compose per [deploy-docker.md](deploy-docker.md).
