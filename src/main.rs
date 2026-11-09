//! skipjack: Rust port of the iSponsorBlockTV core (https://github.com/dmunozv04/iSponsorBlockTV, GPL-3.0)
//! plus the parts of pyytlounge it uses. Reads the upstream config.json format.
//!
//! Usage:
//!   skipjack [config.json]     run (default /etc/skipjack/config.json)
//!   skipjack pair <TV code>    print a device entry for config.json
//!
//! Per device: one thread holds the Lounge long-poll and reacts to events,
//! one "skipper" thread fetches segments and sleeps until the next one.

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const LOUNGE: &str = "https://www.youtube.com/api/lounge";
const APP: &str = "ytios-phone-20.15.1";
// Same app version as APP; what the YouTube iOS app sends.
const UA: &str = "com.google.ios.youtube/20.15.1 (iPhone16,2; U; CPU iOS 18_4 like Mac OS X)";
const KIDS_CLIENT: &str = "TVHTML5_FOR_KIDS";
const SEG_CACHE: usize = 10;
const SEG_TTL: Duration = Duration::from_secs(300);
const RETRY: Duration = Duration::from_secs(10);
const AUTH_REFRESH: Duration = Duration::from_secs(24 * 3600);

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    devices: Vec<Device>,
    apikey: String,
    skip_categories: Option<Vec<String>>,
    channel_whitelist: Vec<Channel>,
    skip_count_tracking: bool,
    mute_ads: bool,
    skip_ads: bool,
    minimum_skip_length: f64,
    auto_play: bool,
    join_name: String,
    sponsorblock_api_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            devices: vec![],
            apikey: String::new(),
            skip_categories: None,
            channel_whitelist: vec![],
            skip_count_tracking: true,
            mute_ads: false,
            skip_ads: false,
            minimum_skip_length: 1.0,
            auto_play: true,
            join_name: "skipjack".into(),
            sponsorblock_api_url: "https://sponsor.ajay.app/api/".into(),
        }
    }
}

#[derive(Deserialize, Clone)]
struct Device {
    screen_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    offset: f64, // milliseconds
}

#[derive(Deserialize)]
struct Channel {
    id: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Seg {
    start: f64,
    end: f64,
    uuids: Vec<String>,
}

fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

fn num(v: &Value, k: &str) -> Option<f64> {
    st(v, k).parse().ok()
}

// ---------------------------------------------------------------- Lounge session

struct Session {
    token: Option<String>,
    sid: Option<String>,
    gsession: Option<String>,
    aid: Option<i64>,
    ofs: u64,
    volume: String,
}

/// Shared between the reader and skipper thread of one device.
struct Lounge {
    agent: ureq::Agent,
    cfg: Arc<Config>,
    name: String,
    screen_id: String,
    device_id: String,
    debug: bool,
    s: Mutex<Session>,
}

impl Lounge {
    fn s(&self) -> MutexGuard<'_, Session> {
        self.s.lock().unwrap()
    }

    fn connected(&self) -> bool {
        let s = self.s();
        s.sid.is_some() && s.gsession.is_some()
    }

    fn lost(&self, token_expired: bool) {
        let mut s = self.s();
        s.sid = None;
        s.gsession = None;
        s.aid = None;
        if token_expired {
            s.token = None;
        }
    }

    fn token(&self) -> Res<String> {
        self.s().token.clone().ok_or_else(|| "not linked".into())
    }

    fn refresh_auth(&self) -> Res<()> {
        let v: Value = self
            .agent
            .post(&format!("{LOUNGE}/pairing/get_lounge_token_batch"))
            .send_form(&[("screen_ids", &self.screen_id)])?
            .into_json()?;
        let t = v["screens"][0]["loungeToken"].as_str().ok_or("no loungeToken in reply")?;
        self.s().token = Some(t.into());
        Ok(())
    }

    fn available(&self) -> Res<bool> {
        let tok = self.token()?;
        let v: Value = self
            .agent
            .post(&format!("{LOUNGE}/pairing/get_screen_availability"))
            .send_form(&[("lounge_token", &tok)])?
            .into_json()?;
        Ok(v["screens"][0]["status"] == "online")
    }

    /// bind URL with the session parameters (pyytlounge _common_connection_parameters).
    fn bind(&self, s: &Session, r: ureq::Request) -> ureq::Request {
        let mut r = r
            .query("name", &self.cfg.join_name)
            .query("loungeIdToken", s.token.as_deref().unwrap_or(""))
            .query("SID", s.sid.as_deref().unwrap_or(""))
            .query("gsessionid", s.gsession.as_deref().unwrap_or(""))
            .query("device", "REMOTE_CONTROL")
            .query("app", APP)
            .query("VER", "8")
            .query("v", "2");
        if let Some(a) = s.aid {
            r = r.query("AID", &a.to_string());
        }
        r
    }

    /// Send a command to the screen. Errors are logged, never fatal.
    fn cmd(&self, cmd: &str, params: &[(&str, &str)]) {
        let mut s = self.s(); // held for the request: commands must not interleave (ofs/RID)
        if s.sid.is_none() || s.gsession.is_none() {
            return;
        }
        let ofs = s.ofs.to_string();
        s.ofs += 1;
        let keys: Vec<String> = params.iter().map(|(k, _)| format!("req0_{k}")).collect();
        let mut body = vec![("count", "1"), ("ofs", ofs.as_str()), ("req0__sc", cmd)];
        body.extend(keys.iter().map(String::as_str).zip(params.iter().map(|p| p.1)));
        let req = self.bind(&s, self.agent.post(&format!("{LOUNGE}/bc/bind")))
            .query("RID", &s.ofs.to_string());
        match req.send_form(&body) {
            Ok(_) => {}
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                println!("{}: {cmd} failed: {code} {}", self.name, text.trim());
                if matches!(code, 400 | 401 | 410) {
                    drop(s);
                    self.lost(code == 401);
                }
            }
            Err(e) => println!("{}: {cmd} failed: {e}", self.name),
        }
    }

    fn mute(&self, mute: bool) {
        let vol = self.s().volume.clone();
        self.cmd("setVolume", &[("volume", &vol), ("muted", if mute { "true" } else { "false" })]);
    }
}

// ---------------------------------------------------------------- reader thread

enum Msg {
    State { video: String, time: f64, playing: bool, at: Instant, speed: f64 },
    Prefetch(String),
}

struct Dev {
    l: Arc<Lounge>,
    tx: Sender<Msg>,
    video: String,
    time: f64,
    state: i64,
    speed: f64,
    shorts: bool,
}

/// BrowserChannel framing: a line with a length, then that many chars of JSON.
fn read_chunks(
    lines: impl Iterator<Item = std::io::Result<String>>,
    mut f: impl FnMut(Value) -> bool,
) -> Res<()> {
    let mut remaining: i64 = 0;
    let mut cur = String::new();
    for line in lines {
        let line = line?;
        if remaining <= 0 {
            remaining = line.trim().parse()?;
            cur.clear();
        } else {
            // ponytail: length counted in UTF-16 units (JS string length); upstream counts code points
            remaining -= line.encode_utf16().count() as i64 + 1;
            cur.push_str(&line);
            if remaining <= 0 && !f(serde_json::from_str(&cur)?) {
                break;
            }
        }
    }
    Ok(())
}

impl Dev {
    fn run(&mut self) {
        let l = self.l.clone();
        let mut refreshed: Option<Instant> = None;
        let mut last_msg = String::new();
        let mut say = |m: String| {
            if m != last_msg {
                println!("{}: {m}", l.name);
                last_msg = m;
            }
        };
        loop {
            if l.s().token.is_none() || refreshed.map_or(true, |t| t.elapsed() > AUTH_REFRESH) {
                match l.refresh_auth() {
                    Ok(()) => refreshed = Some(Instant::now()),
                    Err(e) => {
                        say(format!("auth refresh failed: {e}"));
                        thread::sleep(RETRY);
                        continue;
                    }
                }
            }
            match l.available() {
                Ok(true) => {}
                Ok(false) => {
                    say("waiting for device to be available".into());
                    thread::sleep(RETRY);
                    continue;
                }
                Err(e) => {
                    say(format!("availability check failed: {e}"));
                    thread::sleep(RETRY);
                    continue;
                }
            }
            match self.connect() {
                Ok(true) => say("connected".into()),
                Ok(false) => {
                    say("waiting for device to be connected".into());
                    thread::sleep(RETRY);
                    continue;
                }
                Err(e) => {
                    say(format!("connect failed: {e}"));
                    thread::sleep(RETRY);
                    continue;
                }
            }
            // Ends when YouTube closes the long-poll, on error, or after 65 s of silence
            // (YouTube sends something at least every 30 s; this is upstream's watchdog).
            if let Err(e) = self.subscribe() {
                if l.debug {
                    println!("{}: subscribe ended: {e}", l.name);
                }
            }
        }
    }

    fn connect(&mut self) -> Res<bool> {
        let l = self.l.clone();
        let tok = l.token()?;
        let r = l.agent.post(&format!("{LOUNGE}/bc/bind")).send_form(&[
            ("id", &l.device_id),
            ("mdx-version", "3"),
            ("TYPE", "xmlhttp"),
            ("theme", "cl"),
            ("sessionSource", "MDX_SESSION_SOURCE_UNKNOWN"),
            ("connectParams", r#"{"setStatesParams": "{"playbackSpeed":0}"}"#),
            ("RID", "1"),
            ("CVER", "1"),
            ("capabilities", "que,dsdtr,atp,vsp"),
            ("ui", "false"),
            ("app", APP),
            ("pairing_type", "manual"),
            ("VER", "8"),
            ("loungeIdToken", &tok),
            ("device", "REMOTE_CONTROL"),
            ("name", &l.cfg.join_name),
        ]);
        let text = match r {
            Ok(r) => r.into_string()?,
            Err(ureq::Error::Status(401, r)) => {
                if r.into_string().unwrap_or_default().contains("Connection denied") {
                    println!("{}: connection denied: force close the YouTube app on the TV", l.name);
                }
                l.lost(true);
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        l.lost(false);
        read_chunks(text.lines().map(|s| Ok(s.to_string())), |v| {
            self.chunk(v);
            true
        })?;
        l.s().ofs = 1;
        Ok(l.connected())
    }

    fn subscribe(&mut self) -> Res<()> {
        let l = self.l.clone();
        let req = l.bind(&l.s(), l.agent.get(&format!("{LOUNGE}/bc/bind")));
        let req = req.query("RID", "rpc").query("CI", "0").query("TYPE", "xmlhttp");
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => {
                l.lost(code == 401);
                return Err(format!("subscribe status {code}").into());
            }
            Err(e) => return Err(e.into()),
        };
        let res = read_chunks(BufReader::new(resp.into_reader()).lines(), |v| self.chunk(v));
        if res.is_err() {
            l.lost(false); // reconnect with a fresh session
        }
        res
    }

    /// One chunk = array of [eventId, [type, args...]]. Returns false once disconnected.
    fn chunk(&mut self, v: Value) -> bool {
        for ev in v.as_array().into_iter().flatten() {
            let kind = ev[1][0].as_str().unwrap_or("");
            let a = &ev[1][1];
            match kind {
                "c" => self.l.s().sid = a.as_str().map(Into::into),
                "S" => self.l.s().gsession = a.as_str().map(Into::into),
                _ => self.event(kind, a),
            }
            if let Some(id) = ev[0].as_i64() {
                self.l.s().aid = Some(id);
            }
        }
        self.l.connected()
    }

    fn send_state(&self) {
        let _ = self.tx.send(Msg::State {
            video: self.video.clone(),
            time: self.time,
            playing: self.state == 1,
            at: Instant::now(),
            speed: self.speed,
        });
    }

    fn prefetch(&self, video: &str) {
        if !video.is_empty() {
            let _ = self.tx.send(Msg::Prefetch(video.into()));
        }
    }

    fn event(&mut self, kind: &str, a: &Value) {
        let l = self.l.clone();
        let c = &l.cfg;
        if l.debug {
            println!("{}: {kind} {a}", l.name);
        }
        match kind {
            "onStateChange" => {
                if c.mute_ads && st(a, "state") == "1" {
                    l.mute(false);
                }
                self.time = num(a, "currentTime").unwrap_or(0.0);
                self.state = st(a, "state").parse().unwrap_or(-1);
                self.send_state();
            }
            "nowPlaying" => {
                if c.mute_ads && st(a, "state") == "1" {
                    l.mute(false);
                }
                if let Some(t) = num(a, "currentTime") {
                    self.time = t;
                }
                self.video = st(a, "videoId").into();
                self.state = st(a, "state").parse().unwrap_or(-1);
                self.send_state();
            }
            "onAdStateChange" | "adPlaying" => {
                if kind == "adPlaying" {
                    self.prefetch(st(a, "contentVideoId"));
                }
                let ended = kind == "onAdStateChange"
                    && matches!(st(a, "adState"), "0" | "1082")
                    && st(a, "currentTime") != "0";
                if ended {
                    if c.mute_ads {
                        println!("{}: ad ended, unmuting", l.name);
                        l.mute(false);
                    }
                } else if c.skip_ads && st(a, "isSkipEnabled") == "true" {
                    println!("{}: ad can be skipped, skipping", l.name);
                    l.cmd("skipAd", &[]);
                    if c.mute_ads {
                        l.mute(false);
                    }
                } else if c.mute_ads {
                    println!("{}: ad started, muting", l.name);
                    l.mute(true);
                }
            }
            "onVolumeChanged" => {
                if let Some(v) = a["volume"].as_str() {
                    l.s().volume = v.into();
                }
            }
            "autoplayUpNext" => self.prefetch(st(a, "videoId")),
            "loungeStatus" => {
                let devs: Value = serde_json::from_str(st(a, "devices")).unwrap_or_default();
                for d in devs.as_array().into_iter().flatten() {
                    if d["type"] == "LOUNGE_SCREEN" {
                        let info: Value = serde_json::from_str(d["deviceInfo"].as_str().unwrap_or("{}"))
                            .unwrap_or_default();
                        if st(&info, "clientName") == KIDS_CLIENT {
                            println!("{}: YouTube Kids is not supported, disconnecting", l.name);
                            l.lost(false);
                        }
                    }
                }
            }
            // Shorts make the TV drop the remote; replay the video once it reconnects.
            "loungeScreenDisconnected" => {
                if st(a, "reason") == "disconnectedByUserScreenInitiated" {
                    self.shorts = true;
                }
                l.lost(true);
            }
            "onSubtitlesTrackChanged" => {
                if self.shorts {
                    self.shorts = false;
                    if let Some(v) = a["videoId"].as_str() {
                        l.cmd("setPlaylist", &[("videoId", v)]);
                    }
                }
            }
            "onAutoplayModeChanged" => {
                let want = if c.auto_play { "ENABLED" } else { "DISABLED" };
                let mode = st(a, "autoplayMode");
                if mode != want && mode != "UNSUPPORTED" {
                    l.cmd("setAutoplayMode", &[("autoplayMode", want)]);
                }
            }
            "onPlaybackSpeedChanged" => {
                self.speed = num(a, "playbackSpeed").unwrap_or(1.0);
                l.cmd("getNowPlaying", &[]);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- SponsorBlock + skipper thread

fn whitelisted(l: &Lounge, video: &str) -> Res<bool> {
    let c = &l.cfg;
    if c.apikey.is_empty() || c.channel_whitelist.is_empty() {
        return Ok(false);
    }
    let v: Value = l
        .agent
        .get("https://www.googleapis.com/youtube/v3/videos")
        .query("id", video)
        .query("key", &c.apikey)
        .query("part", "snippet")
        .call()?
        .into_json()?;
    let ch = st(&v["items"][0]["snippet"], "channelId");
    Ok(c.channel_whitelist.iter().any(|w| w.id == ch))
}

/// Returns (segments, cache forever).
fn fetch_segments(l: &Lounge, video: &str) -> Res<(Vec<Seg>, bool)> {
    let cats = l.cfg.skip_categories.as_deref().unwrap_or_default();
    if cats.is_empty() || whitelisted(l, video)? {
        return Ok((vec![], true));
    }
    // k-anonymity: send only the first 4 hex chars of sha256(videoId)
    let hash: String = Sha256::digest(video.as_bytes())[..2].iter().map(|b| format!("{b:02x}")).collect();
    let mut req = l
        .agent
        .get(&format!("{}skipSegments/{hash}", l.cfg.sponsorblock_api_url))
        .set("Accept", "application/json")
        .query("actionType", "skip")
        .query("service", "youtube");
    for c in cats {
        req = req.query("category", c);
    }
    let v: Value = match req.call() {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(404, _)) => return Ok((vec![], false)),
        Err(e) => return Err(e.into()),
    };
    let raw = v
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| x["videoID"] == video)
        .and_then(|x| x["segments"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some((
                s["segment"][0].as_f64()?,
                s["segment"][1].as_f64()?,
                s["UUID"].as_str()?.to_string(),
                s["locked"] == 1,
            ))
        })
        .collect();
    Ok(process_segments(raw, l.cfg.minimum_skip_length))
}

/// Port of ApiHelper.process_segments: merge overlaps, join segments < 1 s apart,
/// drop short ones. Cache forever only if every segment is locked.
fn process_segments(mut raw: Vec<(f64, f64, String, bool)>, min_len: f64) -> (Vec<Seg>, bool) {
    let n = raw.len();
    raw.sort_by(|a, b| a.1.total_cmp(&b.1));
    for i in 0..n {
        for j in 0..n {
            if raw[j].0 <= raw[i].1 && raw[i].1 <= raw[j].1 {
                raw[i].1 = raw[j].1;
            }
        }
    }
    raw.sort_by(|a, b| a.0.total_cmp(&b.0));
    for i in (0..n).rev() {
        for j in (0..n).rev() {
            if raw[j].0 <= raw[i].0 && raw[i].0 <= raw[j].1 {
                raw[i].0 = raw[j].0;
            }
        }
    }
    let forever = n > 0 && raw.iter().all(|r| r.3);
    let mut out: Vec<Seg> = vec![];
    for (start, end, uuid, _) in raw {
        let mut seg = Seg { start, end, uuids: vec![uuid] };
        if out.last().map_or(false, |p| seg.start - p.end < 1.0) {
            let p = out.pop().unwrap();
            seg.start = p.start;
            seg.uuids.extend(p.uuids);
        }
        if seg.end - seg.start > min_len {
            out.push(seg);
        }
    }
    (out, forever)
}

/// Next segment to skip: the first one ahead, or one we are inside at video start (pos < 1 s).
/// Returns (time the skip should happen, segment).
fn next_segment(segs: &[Seg], pos: f64) -> Option<(f64, &Seg)> {
    segs.iter().find_map(|s| {
        if pos < 1.0 && 1.0 < s.end && s.start <= pos && pos < s.end {
            Some((pos, s))
        } else if s.start > pos {
            Some((s.start, s))
        } else {
            None
        }
    })
}

#[derive(Default)]
struct Cache(Vec<(String, Vec<Seg>, Option<Instant>)>); // ponytail: FIFO, linear scan over 10 entries

impl Cache {
    fn get(&mut self, l: &Lounge, video: &str) -> Vec<Seg> {
        let now = Instant::now();
        self.0.retain(|e| e.2.map_or(true, |t| t > now));
        if let Some(e) = self.0.iter().find(|e| e.0 == video) {
            return e.1.clone();
        }
        match fetch_segments(l, video) {
            Ok((segs, forever)) => {
                if self.0.len() >= SEG_CACHE {
                    self.0.remove(0);
                }
                self.0.push((video.into(), segs.clone(), (!forever).then(|| now + SEG_TTL)));
                segs
            }
            Err(e) => {
                println!("{}: segments for {video} failed: {e}", l.name);
                vec![]
            }
        }
    }
}

fn skipper(l: Arc<Lounge>, rx: Receiver<Msg>, offset: f64) {
    let mut cache = Cache::default();
    let mut pending = None;
    loop {
        let msg = match pending.take() {
            Some(m) => m,
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => return,
            },
        };
        let Msg::State { video, time, playing, at, speed } = msg else {
            if let Msg::Prefetch(v) = msg {
                cache.get(&l, &v);
            }
            continue;
        };
        let segs = if video.is_empty() { vec![] } else { cache.get(&l, &video) };
        if !playing {
            continue;
        }
        if l.debug {
            println!("{}: playing {video} with {} segments", l.name, segs.len());
        }
        let Some((start, seg)) = next_segment(&segs, time) else { continue };
        let wait = ((start - time - at.elapsed().as_secs_f64()) / speed - offset).max(0.0);
        let deadline = Instant::now() + Duration::from_secs_f64(wait);
        // Any new playback state cancels the pending skip (seek, pause, next video...).
        // Prefetches during the wait are dropped: they are only an optimisation.
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(m @ Msg::State { .. }) => {
                    pending = Some(m);
                    break;
                }
                Ok(Msg::Prefetch(_)) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    println!("{}: skipping {video} {:.1}s -> {:.1}s", l.name, seg.start, seg.end);
                    l.cmd("seekTo", &[("newTime", &seg.end.to_string())]);
                    if l.cfg.skip_count_tracking {
                        for u in &seg.uuids {
                            let url = format!("{}viewedVideoSponsorTime/", l.cfg.sponsorblock_api_url);
                            if let Err(e) = l.agent.post(&url).query("UUID", u).call() {
                                println!("{}: viewed-segment report failed: {e}", l.name);
                            }
                        }
                    }
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

// ---------------------------------------------------------------- main

fn pair(agent: &ureq::Agent, code: &str) -> Res<()> {
    let code: String = code.chars().filter(char::is_ascii_digit).collect();
    let v: Value = match agent
        .post(&format!("{LOUNGE}/pairing/get_screen"))
        .send_form(&[("pairing_code", &code)])
    {
        Ok(r) => r.into_json()?,
        Err(ureq::Error::Status(404, _)) => return Err("no screen for that code (expired? get a new one on the TV)".into()),
        Err(e) => return Err(e.into()),
    };
    let s = &v["screen"];
    if s["screenId"].is_null() {
        return Err("no screen for that code".into());
    }
    println!("{}", json!({"screen_id": s["screenId"], "name": s["name"], "offset": 0}));
    Ok(())
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(65))
        .timeout_write(Duration::from_secs(10))
        .user_agent(UA)
        .build();
    if args.get(1).map(String::as_str) == Some("pair") {
        return pair(&agent, args.get(2).ok_or("usage: skipjack pair <code>")?);
    }
    let path = args.get(1).map_or("/etc/skipjack/config.json", String::as_str);
    let mut cfg: Config = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if cfg.devices.is_empty() {
        return Err("no devices in config".into());
    }
    if cfg.apikey.is_empty() && !cfg.channel_whitelist.is_empty() {
        return Err("channel_whitelist needs apikey".into());
    }
    if cfg.skip_categories.is_none() {
        println!("no skip_categories, using default: sponsor");
        cfg.skip_categories = Some(vec!["sponsor".into()]);
    }
    let debug = std::env::var_os("SKIPJACK_DEBUG").is_some();
    let cfg = Arc::new(cfg);
    let mut handles = vec![];
    for d in cfg.devices.clone() {
        // Upstream's seed, so a TV paired with iSponsorBlockTV sees the same remote.
        let id: String = Sha256::digest(format!("iSponsorBlockTV{}", d.screen_id))[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let name = if d.name.is_empty() { d.screen_id.clone() } else { d.name.clone() };
        let l = Arc::new(Lounge {
            agent: agent.clone(),
            cfg: cfg.clone(),
            name,
            screen_id: d.screen_id.clone(),
            device_id: id,
            debug,
            s: Mutex::new(Session {
                token: None,
                sid: None,
                gsession: None,
                aid: None,
                ofs: 1,
                volume: "100".into(),
            }),
        });
        let (tx, rx) = mpsc::channel();
        let l2 = l.clone();
        let offset = d.offset / 1000.0;
        handles.push(thread::spawn(move || skipper(l2, rx, offset)));
        handles.push(thread::spawn(move || {
            Dev { l, tx, video: String::new(), time: 0.0, state: -1, speed: 1.0, shorts: false }.run()
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: f64, e: f64, u: &str, locked: bool) -> (f64, f64, String, bool) {
        (s, e, u.into(), locked)
    }

    #[test]
    fn segments_merge_join_and_filter() {
        let (segs, forever) = process_segments(
            vec![
                r(10.0, 20.0, "a", true),
                r(15.0, 25.0, "b", true),    // overlaps a -> 10..25
                r(25.5, 30.0, "c", true),    // < 1 s after -> joined 10..30
                r(100.0, 100.5, "d", true),  // shorter than min -> dropped
            ],
            1.0,
        );
        assert!(forever);
        assert_eq!(segs.len(), 1);
        assert_eq!((segs[0].start, segs[0].end), (10.0, 30.0));
        assert!(segs[0].uuids.contains(&"a".to_string()) && segs[0].uuids.contains(&"c".to_string()));
        let (_, forever) = process_segments(vec![r(1.0, 5.0, "x", false)], 1.0);
        assert!(!forever);
        assert!(!process_segments(vec![], 1.0).1);
    }

    #[test]
    fn next_segment_rules() {
        let s = vec![Seg { start: 0.0, end: 10.0, uuids: vec![] }, Seg { start: 50.0, end: 60.0, uuids: vec![] }];
        assert_eq!(next_segment(&s, 0.0).map(|x| x.0), Some(0.0)); // inside first at video start
        assert_eq!(next_segment(&s, 5.0).map(|x| x.0), Some(50.0)); // seeked into it: not skipped
        assert_eq!(next_segment(&s, 70.0), None);
    }

    #[test]
    fn chunk_framing() {
        let body = "19\n[[0,[\"c\",\"SID1\"]]]\n16\n[[1,[\"noop\"]]\n]\n";
        let mut got = vec![];
        read_chunks(body.lines().map(|l| Ok(l.to_string())), |v| {
            got.push(v);
            true
        })
        .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0][0][1][1], "SID1");
        assert_eq!(got[1][0][1][0], "noop");
    }
}
