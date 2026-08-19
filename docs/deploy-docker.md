# Deploy with Docker Compose

Target: a Linux host where a USB LTE modem is attached (bare metal or a VM
with USB passthrough — [proxmox-usb-passthrough.md](proxmox-usb-passthrough.md)).
ModemManager owns the stick on that host. The container only talks to
the system D-Bus socket — no USB device nodes enter it.

## 1. Prerequisites (one-time, on the host)

- Modem ready per [ubuntu-modem-setup.md](ubuntu-modem-setup.md)
  (`sudo ./scripts/setup-ubuntu-modem.sh`).
- `mmcli -m "$MODEM_UID"` prints a registered modem.
- Docker Engine + Compose plugin.

## 2. Get the code

```bash
git clone <repo-url> telesms-bot
cd telesms-bot
```

## 3. Configure

```bash
cp .env.example .env
$EDITOR .env   # token, user id, group id, Google client id/secret
mkdir -p data secrets
```

Google OAuth needs a browser, so mint the token on a machine with a
browser and copy it over (never run `auth` on a headless host):

```bash
scp secrets/google-token.json <user>@<host>:telesms-bot/secrets/
```

## 4. Stop any other daemon using the same bot

Two instances of the same bot token conflict on Telegram `getUpdates`
(HTTP 409). Stop any native `cargo run` / systemd instance before
starting the container.

## 5. Start

```bash
docker compose up -d --build
```

First build compiles every crate: expect 10–20 minutes. Later builds hit
the BuildKit cache and take ~1–2 minutes.

## 6. Verify

```bash
docker compose logs -f
```

- `google contacts synced` appears (token + network OK).
- `docker compose run --rm telesms telesms-bot check-modem` prints a ModemManager object path (D-Bus + attached stick OK).
- Send one SMS to the stick → it lands in the right Telegram topic.
- Type in a contact topic → the SMS arrives on the phone.

## 7. Update

```bash
git pull
docker compose up -d --build
```

## Notes

- Config change: edit `.env`, then `docker compose up -d` (recreates the
  container with the new env).
- Logs rotate at 10 MB × 3 files (`json-file` driver).
- After a big dependency change, a stale cache mount can break the build;
  fix with `docker builder prune` and rebuild.
- The container is root-in-container on purpose (ModemManager's D-Bus
  policy admits root). Do not host untrusted containers on the same
  machine without revisiting that decision.
- Ubuntu AppArmor's `docker-default` profile blocks D-Bus `Hello` on the
  host system bus (`AccessDenied` / `sender="(null)"`). Compose sets
  `apparmor:unconfined` so the container can talk to host ModemManager.
- Host reboot: systemd starts Docker after dbus; the container auto-starts
  and the daemon's retry loops re-attach to the modem.
