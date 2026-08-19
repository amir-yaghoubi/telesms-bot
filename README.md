# telesms-bot

Personal two-way SMS ↔ Telegram gateway. A USB LTE modem on the **host**
(via ModemManager) bridges to a Telegram **forum group**: one **General**
topic plus an on-demand topic per contact. Incoming SMS land in the right
topic; anything you type in a contact topic is sent as SMS. Contacts come
from Google.

Tested with a D-Link DWM-222. Any ModemManager-managed stick should work
if `MODEM_UID` equals that modem’s `System.device` field.

## Docs

| Doc | What |
|---|---|
| [Ubuntu modem setup](docs/ubuntu-modem-setup.md) | Packages, udev UID, enable, `MODEM_UID` |
| [Proxmox USB passthrough](docs/proxmox-usb-passthrough.md) | Pass the stick into a VM **by USB port** |
| [Docker Compose](docs/deploy-docker.md) | Run the bot next to host ModemManager |

`scripts/setup-ubuntu-modem.sh` installs packages and can pin a stable UID.

Do **not** run ModemManager inside Docker. The container only mounts the
host system D-Bus socket. On Ubuntu, Compose sets `apparmor:unconfined`
so that mount actually works.

## Run natively

1. BotFather: create bot, disable privacy mode, enable inline mode, add
   to a **forum** group as admin (manage topics).
2. Copy `.env.example` → `.env`. Fill token, your user id, group id (`-100…`).
3. Google Cloud: OAuth desktop client, Contacts readonly. Put client
   id/secret in `.env`.
4. `cargo run -- auth` → write `./secrets/google-token.json`.
5. Stick ready and **registered**: [ubuntu-modem-setup.md](docs/ubuntu-modem-setup.md).
6. `RUST_LOG=info cargo run`
7. In General: `/sms 09… hello`. Confirm phone + Telegram ✓.

## Run with Docker Compose

On the machine that owns the stick (bare metal or a VM with USB
passthrough): [docs/deploy-docker.md](docs/deploy-docker.md).
