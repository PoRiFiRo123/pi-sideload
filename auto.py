#!/usr/bin/env python3
"""Keep an app on the iPhone fresh: install each new GitHub release, and re-sign before the 7-day free certificate runs out.

Run from cron every 2 hours (setup.sh adds the line). It does nothing unless:
  - the repo has a newer release than the one installed, or
  - the last install is older than REFRESH_DAYS.
When the iPhone isn't on the Wi-Fi it just tries again next time, and only messages you about it once a day
(or when the app is about to expire).

Config: ~/.pi-sideload/config.json   State: ~/.pi-sideload/state.json   Log: ~/.pi-sideload/auto.log
"""
import fcntl
import json
import os
import re
import subprocess
import sys
import time
import urllib.parse
import urllib.request

DIR = os.path.expanduser(os.environ.get("PI_SIDELOAD_DIR", "~/.pi-sideload"))
CONFIG = os.path.join(DIR, "config.json")
STATE = os.path.join(DIR, "state.json")
BIN = os.path.join(DIR, "bin", "pi-sideload")

DEFAULTS = {
    "repo": "PoRiFiRo123/paisa-pulse",   # GitHub repo with the releases
    "asset": "PaisaPulse.ipa",           # file name in each release
    "app_name": "Paisa Pulse",
    "iphone_host": "",                   # the iPhone's IP; empty = find it on the network (avahi)
    "iphone_rp_port": 0,                 # its _remotepairing._tcp port; 0 = find it (it can change)
    "refresh_days": 5,                   # re-sign when the last install is this old (free certs last 7 days)
    "extensions": "register",            # register: widget + share extension get their own App IDs
    "telegram_bot_token": "", "telegram_chat_id": "", "discord_webhook": "",
}

DAY = 86400


def log(msg):
    print(time.strftime("%Y-%m-%d %H:%M:%S"), msg, flush=True)


def load(path, default):
    try:
        with open(path) as f:
            return {**default, **json.load(f)}
    except (OSError, ValueError):
        return dict(default)


def save(path, data):
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
    os.replace(tmp, path)


def notify(cfg, text):
    sent = []
    if cfg.get("telegram_bot_token") and cfg.get("telegram_chat_id"):
        try:
            body = urllib.parse.urlencode({"chat_id": cfg["telegram_chat_id"], "text": text}).encode()
            urllib.request.urlopen(f"https://api.telegram.org/bot{cfg['telegram_bot_token']}/sendMessage", body, timeout=20)
            sent.append("telegram")
        except Exception as e:  # noqa: BLE001 - a failed message mustn't stop the install
            log(f"telegram failed: {e}")
    if cfg.get("discord_webhook"):
        try:
            req = urllib.request.Request(cfg["discord_webhook"].strip(), json.dumps({"content": text}).encode(),
                                         {"Content-Type": "application/json", "User-Agent": "pi-sideload"})
            urllib.request.urlopen(req, timeout=20)
            sent.append("discord")
        except Exception as e:  # noqa: BLE001
            log(f"discord failed: {e}")
    log(f"message ({', '.join(sent) or 'not sent'}): {text}")


def latest_release(cfg):
    req = urllib.request.Request(f"https://api.github.com/repos/{cfg['repo']}/releases/latest",
                                 headers={"Accept": "application/vnd.github+json", "User-Agent": "pi-sideload"})
    with urllib.request.urlopen(req, timeout=30) as r:
        rel = json.load(r)
    asset = next((a for a in rel.get("assets", []) if a["name"] == cfg["asset"]), None)
    if not asset:
        raise RuntimeError(f"release {rel.get('tag_name')} has no {cfg['asset']}")
    m = re.search(r"(\d+)", rel.get("tag_name", ""))
    return {"tag": rel["tag_name"], "number": int(m.group(1)) if m else 0, "url": asset["browser_download_url"]}


def summarize(stderr):
    """isideload errors are a tree of "● message" lines with source paths under them: keep the messages."""
    msgs = [l.strip(" │├╰●").strip() for l in stderr.splitlines() if "●" in l or l.startswith("error:")]
    msgs = [m.removeprefix("error:").strip().rstrip(":") for m in msgs]
    return ": ".join(m for m in msgs if m) or "unknown error"


def browse(service):
    """[(ip, port)] for an mDNS service (avahi-utils), IPv4 only."""
    try:
        out = subprocess.run(["avahi-browse", "-rpt", service], capture_output=True, text=True, timeout=20).stdout
    except (OSError, subprocess.TimeoutExpired):
        return []
    found = []
    for line in out.splitlines():
        parts = line.split(";")
        if parts[0] == "=" and len(parts) > 8 and parts[2] == "IPv4":
            found.append((parts[7], int(parts[8])))
    return found


def find_iphone(cfg):
    """(ip, remote-pairing port) or (None, None). iOS 17+ copies apps over Wi-Fi only through the remote-pairing tunnel."""
    rp = browse("_remotepairing._tcp")
    host = cfg.get("iphone_host") or next((ip for ip, _ in browse("_apple-mobdev2._tcp")), None) or (rp[0][0] if rp else None)
    port = cfg.get("iphone_rp_port") or next((p for ip, p in rp if ip == host), None)
    return host, port


def run(cfg, state, force=False):
    now = time.time()
    rel = latest_release(cfg)
    last_install = state.get("last_install", 0)
    newer = rel["number"] > state.get("build", 0)
    due = now - last_install > cfg["refresh_days"] * DAY
    if not (force or newer or due):
        log(f"nothing to do (installed {state.get('tag', '?')}, {int((now - last_install) / 3600)} h ago)")
        return
    reason = "new build" if newer else "re-signing before it expires" if due else "asked to"
    expires_in = last_install + 7 * DAY - now if last_install else None

    def failed(msg):
        state["last_error"] = msg
        quiet = now - state.get("last_failure_message", 0) < DAY
        urgent = expires_in is not None and expires_in < DAY
        if not quiet or urgent:
            hint = f" It expires in {max(0, int(expires_in / 3600))} h." if expires_in is not None else ""
            notify(cfg, f"⚠️ {cfg['app_name']}: couldn't install {rel['tag']} ({msg}).{hint} Will keep trying.")
            state["last_failure_message"] = now
        log(f"failed: {msg}")

    host, rp_port = find_iphone(cfg)
    if not host:
        return failed("iPhone not found on the Wi-Fi")
    if not rp_port:
        return failed("found the iPhone but not its pairing port (_remotepairing._tcp); is it unlocked with Developer Mode on?")
    log(f"iPhone at {host}, pairing port {rp_port}")
    ipa = os.path.join(DIR, cfg["asset"])
    log(f"{reason}: downloading {rel['tag']}")
    urllib.request.urlretrieve(rel["url"], ipa)
    env = {**os.environ, **({"PI_SIDELOAD_DEBUG": "1"} if "--debug" in sys.argv else {})}
    p = subprocess.run([BIN, "install", ipa, "--host", host, "--rp-port", str(rp_port), "--dir", DIR, "--extensions", cfg["extensions"]],
                       capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=900, env=env)
    log(p.stdout.strip() + ("\n" + p.stderr.strip() if p.stderr.strip() else ""))
    if p.returncode != 0:
        return failed(summarize(p.stderr)[:300])
    state.update(build=rel["number"], tag=rel["tag"], last_install=now, last_error=None, last_failure_message=0)
    what = f"{rel['tag']} installed" if newer else f"re-signed ({rel['tag']})"
    notify(cfg, f"✅ {cfg['app_name']} {what}. Good until {time.strftime('%a %d %b', time.localtime(now + 7 * DAY))}.")


def main():
    os.makedirs(DIR, exist_ok=True)
    with open(os.path.join(DIR, ".lock"), "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            log("another run is in progress")
            return
        cfg = load(CONFIG, DEFAULTS)
        state = load(STATE, {})
        try:
            run(cfg, state, force="--now" in sys.argv)
        except Exception as e:  # noqa: BLE001 - log and try again next time
            log(f"error: {e}")
            state["last_error"] = str(e)
        save(STATE, state)


if __name__ == "__main__":
    main()
