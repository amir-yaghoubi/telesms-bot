# telesms-bot

Personal two-way SMS ↔ Telegram gateway. A USB LTE modem on the host
(via ModemManager) bridges to a Telegram **forum group**: one **General**
topic plus an on-demand topic per contact. Incoming SMS land in the right
topic; anything you type in a contact topic is sent as SMS. Contacts come
from Google.

Tested with a D-Link DWM-222. Any ModemManager-managed stick should work
if `MODEM_UID` matches that modem’s Device field.

Run it natively with `cargo run`, or in Docker Compose next to host
ModemManager — see [docs/deploy-docker.md](docs/deploy-docker.md).

## Run natively

1. BotFather: create bot, disable privacy mode, add to a **forum** group as admin (manage topics).
2. Enable inline mode on the bot.
3. Copy `.env.example` → `.env`. Fill token, your user id, group id (`-100…`).
4. Google Cloud: OAuth desktop client, Contacts readonly. Put client id/secret in `.env`.
5. `cargo run -- auth` → write `./secrets/google-token.json`.
6. Stick ready, `mmcli -m "$MODEM_UID"` registered — see [docs/ubuntu-modem-setup.md](docs/ubuntu-modem-setup.md).
7. `RUST_LOG=info cargo run`
8. In General: `/sms 09… hello`. Confirm phone + Telegram ✓.

## Run with Docker Compose

Build on the host that has the modem. Mount the host D-Bus socket,
`./data` + `./secrets` volumes, env from `.env`. No app changes.
Runbook: [docs/deploy-docker.md](docs/deploy-docker.md).

| Doc | What |
|---|---|
| [Ubuntu modem setup](docs/ubuntu-modem-setup.md) | Make the stick visible to ModemManager |
| [Proxmox USB passthrough](docs/proxmox-usb-passthrough.md) | Pass the stick into a VM by USB port |
| [Ubuntu VM host setup](docs/ubuntu-vm-host-setup.md) | D-Bus + Compose on the VM |
| [Docker deploy](docs/deploy-docker.md) | Compose runbook |
| `scripts/setup-ubuntu-modem.sh` | Installs packages, optional udev UID |
