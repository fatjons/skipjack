#!/usr/bin/env bash
# Build, pair and deploy skipjack to the Pi from the workstation.
#   ./deploy.sh              test + ARMv6 build + push binary/unit/config + restart (code changes)
#   ./deploy.sh config       push config.json + restart, no build (settings changes, ~5 s)
#   ./deploy.sh pair CODE [NAME]  add the TV with that "Link with TV code" to config.json, push it
#                                 NAME shows in logs, e.g. "Living room" (default: name YouTube reports)
#   ./deploy.sh logs         follow the service log (RAM journal on the Pi)
set -euo pipefail
cd "$(dirname "$0")"
# Pi address: env SKIPJACK_PI, or a gitignored deploy.env with SKIPJACK_PI=root@<pi-ip>
[ -f deploy.env ] && . ./deploy.env
PI="${SKIPJACK_PI:?set SKIPJACK_PI=root@<pi-ip> (env or deploy.env)}"
BIN=target/arm-unknown-linux-musleabihf/release/skipjack

if [ "${1:-}" = logs ]; then
  exec ssh "$PI" journalctl --namespace=skipjack -u skipjack -n 50 -f
fi

push_config() {
  [ -f config.json ] || { echo "no config.json: run ./deploy.sh pair CODE first"; exit 1; }
  python3 -m json.tool config.json >/dev/null || { echo "config.json is not valid JSON"; exit 1; }
  scp -q config.json "$PI:/tmp/skipjack.json"
  ssh "$PI" '
    set -e
    install -d /etc/skipjack
    install -m 644 /tmp/skipjack.json /etc/skipjack/config.json && rm /tmp/skipjack.json
    systemctl daemon-reload
    systemctl enable -q skipjack
    systemctl restart skipjack
    sleep 3
    systemctl is-active skipjack
    journalctl --namespace=skipjack -u skipjack -n 5 --no-pager'
}

if [ "${1:-}" = config ]; then push_config; exit; fi

if [ "${1:-}" = pair ]; then
  code="${2:?usage: ./deploy.sh pair CODE}"
  ssh "$PI" test -x /usr/local/bin/skipjack || { echo "not installed yet: run ./deploy.sh first"; exit 1; }
  dev=$(ssh "$PI" /usr/local/bin/skipjack pair "$code")
  [ -f config.json ] || echo '{"devices":[],"skip_categories":["sponsor","selfpromo"]}' > config.json
  DEV="$dev" NAME="${3:-}" python3 -c '
import json, os
c = json.load(open("config.json")); d = json.loads(os.environ["DEV"])
if os.environ["NAME"]: d["name"] = os.environ["NAME"]
c["devices"] = [x for x in c["devices"] if x["screen_id"] != d["screen_id"]] + [d]
json.dump(c, open("config.json", "w"), indent=2)
print("paired:", d["name"])'
  push_config; exit
fi

docker run --rm -v "$PWD":/app -v skipjack-cargo:/usr/local/cargo/registry -w /app rust:slim bash -c '
  set -e
  cargo test --quiet
  apt-get update -qq && apt-get install -y -qq gcc-arm-linux-gnueabihf musl-tools >/dev/null 2>&1
  rustup target add arm-unknown-linux-musleabihf >/dev/null 2>&1
  cargo build --release --quiet --target arm-unknown-linux-musleabihf
  chown -R 1000:1000 /app'

scp -q "$BIN" "$PI:/tmp/skipjack.new"
ssh "$PI" chmod +x /tmp/skipjack.new

scp -q skipjack.service "$PI:/etc/systemd/system/skipjack.service"
scp -q journald@skipjack.conf "$PI:/etc/systemd/journald@skipjack.conf"
ssh "$PI" '[ -f /usr/local/bin/skipjack ] && cp /usr/local/bin/skipjack /usr/local/bin/skipjack.bak; mv -f /tmp/skipjack.new /usr/local/bin/skipjack'
if [ -f config.json ]; then push_config; else echo "installed; now run ./deploy.sh pair CODE"; fi
