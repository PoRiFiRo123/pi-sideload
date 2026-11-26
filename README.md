# pi-sideload

Keeps a sideloaded app on your iPhone up to date from a Raspberry Pi, with a free Apple ID and no cable:

- installs every new GitHub release of the app (default: [Paisa Pulse](https://github.com/PoRiFiRo123/paisa-pulse) `PaisaPulse.ipa`),
- re-signs it every 5 days, before the free 7-day certificate expires,
- installs over Wi-Fi, using a pairing file made once over USB,
- messages you on Telegram and/or Discord.

It takes no app slot on the phone: the signing happens on the Pi. Built on [isideload](https://github.com/nab138/isideload)
(the library [iloader](https://iloader.app) uses) and [idevice](https://github.com/jkcoxson/idevice).

## Setup

1. **On the computer the iPhone is plugged into** (unlocked, trusted, Developer Mode on):
   ```bash
   curl -fsSL https://raw.githubusercontent.com/PoRiFiRo123/pi-sideload/main/setup.sh | bash -s pair
   ssh admin@frame.local 'mkdir -p ~/.pi-sideload' && scp ~/.pi-sideload/pairing.plist admin@frame.local:~/.pi-sideload/
   ```
2. **On the always-on Pi** (64-bit Raspberry Pi OS):
   ```bash
   curl -fsSL https://raw.githubusercontent.com/PoRiFiRo123/pi-sideload/main/setup.sh -o setup.sh && bash setup.sh
   python3 ~/.pi-sideload/auto.py --now && tail -20 ~/.pi-sideload/auto.log
   ```
   Setup signs in to your Apple ID once (2FA), copies Telegram/Discord settings from `~/trader/notify.json` if present,
   and adds a cron job that checks every 2 hours.

## Files (`~/.pi-sideload`, readable only by you)

| File | What |
|---|---|
| `config.json` | repo, asset, `iphone_host` (blank = find it with avahi), `refresh_days`, Telegram/Discord |
| `account.json` | Apple ID and password, for unattended sign-in |
| `storage/` | the Pi's Apple "device" identity (so 2FA isn't asked again) and the signing certificate |
| `pairing.plist` | lets the Pi talk to the iPhone over Wi-Fi. Re-make it after resetting or restoring the phone |
| `state.json`, `auto.log` | what's installed, and what happened |

## Commands

```
pi-sideload login
pi-sideload pair
pi-sideload check --host <iphone-ip>
pi-sideload install app.ipa --host <iphone-ip> [--usb] [--extensions register|reuse]
```

## Notes

- The iPhone has to be on the same Wi-Fi when a run happens; otherwise it retries every 2 hours and only tells you once a
  day (or when the app is about to expire). A DHCP reservation for the phone + `iphone_host` is the most reliable.
- A free Apple ID allows 2 signing certificates. If both are taken (say by iloader and another computer), run
  `~/.pi-sideload/bin/pi-sideload install <ipa> --host <ip>` once in a terminal to choose which to revoke.
  Revoking a certificate stops the apps signed with it.
- If Apple asks for 2FA again, the cron run stops and tells you; run `pi-sideload login` once.
