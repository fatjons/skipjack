# skipjack

Skips sponsor segments in YouTube videos playing on your TV (Google TV, Android TV, smart TVs,
consoles: anything with the YouTube TV app), using [SponsorBlock](https://sponsor.ajay.app) data.
It can also mute YouTube ads and press "Skip ad" as soon as it is allowed.

A Rust port of the core of [iSponsorBlockTV](https://github.com/dmunozv04/iSponsorBlockTV)
by dmunozv04, built to run on very small hardware. On a Raspberry Pi 1 B+ (ARMv6, 700 MHz) it is
a 1.4 MB static binary using a few MB of RAM, idle most of the time, next to Pi-hole.

## How it works

skipjack pairs with the TV like a phone does ("Link with TV code") and listens to the YouTube
Lounge API for playback events. When a video plays it fetches the video's segments from
SponsorBlock (by a 4-character hash prefix of the video id, so SponsorBlock never sees which
video you watch) and sends a seek when a segment starts.

Per TV: one thread holds the Lounge connection, one sleeps until the next segment.

## Build

Needs Docker (cross-compiles for ARMv6 musl, static):

```bash
./deploy.sh           # cargo test, ARMv6 build, install on the Pi, restart
```

Native build for testing: `cargo build --release`.

## Install on a Pi

`deploy.sh` runs on your workstation and talks to the Pi over ssh (root, key auth).
Tell it where the Pi is, in a `deploy.env` file next to it (gitignored):

```bash
SKIPJACK_PI=root@192.168.1.10
```

Then:

```bash
./deploy.sh                          # install binary, systemd unit, RAM-only journal
./deploy.sh pair 1234-5678-9012 "Living room"   # code from YouTube on the TV: Settings > Link with TV code
./deploy.sh config                   # after editing config.json: push + restart, no build
./deploy.sh logs                     # follow the log
```

Logs go to a separate systemd journal namespace kept in RAM only (2 MB cap), so nothing is
written to the SD card.

## Configuration

`config.json` uses the same format as iSponsorBlockTV, so an existing config works as is.

| Key | Default | Meaning |
|---|---|---|
| `devices` | `[]` | TVs: `screen_id` (from `pair`), `name` (for logs), `offset` (ms, skip earlier on slow TVs) |
| `skip_categories` | `["sponsor"]` | SponsorBlock categories to skip, see below |
| `mute_ads` | `false` | mute YouTube ads, unmute when the video starts |
| `skip_ads` | `false` | press "Skip ad" when it becomes available |
| `minimum_skip_length` | `1` | ignore segments shorter than this (s) |
| `auto_play` | `true` | keep the TV's autoplay on or off |
| `join_name` | `"skipjack"` | name shown on the TV's linked devices list |
| `skip_count_tracking` | `true` | tell SponsorBlock a segment was skipped (credits the submitter) |
| `channel_whitelist` + `apikey` | `[]`, `""` | never skip on these channels (needs a YouTube Data API key) |
| `sponsorblock_api_url` | `https://sponsor.ajay.app/api/` | SponsorBlock server |

Categories: `sponsor`, `selfpromo`, `interaction`, `intro`, `outro`, `preview`, `hook`,
`filler`, `music_offtopic`. See [SponsorBlock's guidelines](https://wiki.sponsor.ajay.app/w/Guidelines).

Keep `config.json` private: a `screen_id` lets anyone control that TV's YouTube app.

Debug: set `SKIPJACK_DEBUG=1` in the service to log every Lounge event.

## Not included (vs iSponsorBlockTV)

Setup TUI, automatic TV discovery (DIAL/Chromecast), proxy support. Use `pair` instead of
discovery. YouTube ads cannot be removed: unskippable ones are muted, not skipped.

## License

GPL-3.0, like iSponsorBlockTV, from which this is derived. Not affiliated with YouTube, Google,
SponsorBlock or iSponsorBlockTV.
