#!/usr/bin/env bash
# pi-sideload setup.
#
#   On the computer the iPhone is plugged into (USB, unlocked, trusted):
#     curl -fsSL https://raw.githubusercontent.com/PoRiFiRo123/pi-sideload/main/setup.sh | bash -s pair
#     then copy the file it prints to the always-on Pi.
#
#   On the always-on Pi (e.g. Frame):
#     curl -fsSL https://raw.githubusercontent.com/PoRiFiRo123/pi-sideload/main/setup.sh -o setup.sh && bash setup.sh
set -euo pipefail

REPO="PoRiFiRo123/pi-sideload"
DIR="${PI_SIDELOAD_DIR:-$HOME/.pi-sideload}"
BIN="$DIR/bin/pi-sideload"
mode="${1:-install}"

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

get_binary() {
  case "$(uname -m)" in
    aarch64|arm64) asset=pi-sideload-linux-arm64 ;;
    x86_64) asset=pi-sideload-linux-x64 ;;
    *) echo "This needs a 64-bit OS (uname -m says $(uname -m)). Raspberry Pi OS 64-bit works."; exit 1 ;;
  esac
  mkdir -p "$DIR/bin"
  say "Downloading pi-sideload ($asset)"
  curl -fsSL "https://github.com/$REPO/releases/latest/download/$asset" -o "$BIN.new"
  chmod +x "$BIN.new" && mv "$BIN.new" "$BIN"
  curl -fsSL "https://raw.githubusercontent.com/$REPO/main/auto.py" -o "$DIR/auto.py"
}

if [[ $mode == pair ]]; then
  get_binary
  say "Pairing with the iPhone on USB (unlock it; tap Trust if asked)"
  "$BIN" pair --dir "$DIR"
  say "Now copy the pairing file to the always-on Pi, for example:"
  echo "  ssh admin@frame.local 'mkdir -p ~/.pi-sideload' && scp $DIR/pairing.plist $DIR/rppairing.plist admin@frame.local:~/.pi-sideload/"
  exit 0
fi

get_binary

if ! command -v avahi-browse >/dev/null; then
  say "Installing avahi-utils (to find the iPhone on the Wi-Fi)"
  sudo apt-get install -y avahi-utils
fi

if [[ ! -f $DIR/config.json ]]; then
  say "Writing $DIR/config.json"
  python3 - "$DIR/config.json" <<'PY'
import json, os, sys
cfg = {"repo": "PoRiFiRo123/paisa-pulse", "asset": "PaisaPulse.ipa", "app_name": "Paisa Pulse",
       "iphone_host": "", "refresh_days": 5, "extensions": "register",
       "telegram_bot_token": "", "telegram_chat_id": "", "discord_webhook": ""}
# Reuse the trading bot's Telegram / Discord settings if they're on this Pi.
for p in ("~/trader/notify.json", "~/trader/india-trader-skill/pi/notify.json"):
    p = os.path.expanduser(p)
    if os.path.exists(p):
        old = json.load(open(p))
        for k in ("telegram_bot_token", "telegram_chat_id", "discord_webhook"):
            cfg[k] = old.get(k, "") or cfg[k]
        print("  Telegram/Discord settings copied from", p)
        break
json.dump(cfg, open(sys.argv[1], "w"), indent=2)
os.chmod(sys.argv[1], 0o600)
PY
fi

if [[ ! -f $DIR/account.json ]]; then
  say "Sign in with the Apple ID you use for sideloading (you'll get a 2FA code once)"
  "$BIN" login --dir "$DIR" </dev/tty
fi

line="17 */2 * * * python3 $DIR/auto.py >> $DIR/auto.log 2>&1"
if ! crontab -l 2>/dev/null | grep -qF "$DIR/auto.py"; then
  (crontab -l 2>/dev/null; echo "# pi-sideload: install new Paisa Pulse builds, re-sign every few days"; echo "$line") | crontab -
  say "Added the cron job (every 2 hours)"
fi

if [[ ! -f $DIR/pairing.plist || ! -f $DIR/rppairing.plist ]]; then
  say "Last step: the pairing file"
  echo "On the computer the iPhone is plugged into, run:"
  echo "  curl -fsSL https://raw.githubusercontent.com/$REPO/main/setup.sh | bash -s pair"
  echo "and copy pairing.plist and rppairing.plist here to $DIR/. Then test with:"
else
  say "All set. Test it now:"
fi
echo "  python3 $DIR/auto.py --now && tail -20 $DIR/auto.log"
