//! mixfeed-audio: the NDI audio receive engine behind a virtual microphone,
//! shared by the Linux daemon (`linux/mixfeedd`) and the macOS audio server
//! plug-in.
//!
//! One receive thread connects to the selected NDI source when a client
//! starts IO. While IO runs, it builds a [`Playout`] for the source format,
//! installs it for the IO callback and fills its ring. After the last client
//! stops, the thread stays connected for [`LINGER`] and keeps the sender rate
//! estimate up to date, so the next start only has to fill the buffer. With
//! [`set_keep_warm`] on, it stays connected without a time limit and also
//! connects without a client: the microphone is ready at once and the clock
//! estimate is always settled.
//!
//! The source list comes from a second, short-lived finder: each call of
//! [`sources`] keeps it browsing for [`BROWSE_FOR`]. Changes of the list and
//! of the receive state go to the `changed` hook, so clients do not have to
//! poll.
//!
//! Times are [`host_seconds`] on both sides (mach host time on macOS,
//! `CLOCK_MONOTONIC` on Linux, which is also PipeWire's clock), so the sender
//! rate and the device rate are measured against the same clock.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use mixfeed_core::clock::RateEstimator;
use mixfeed_core::playout::{Playout, ResamplerKind, SharedRate, Stats};
use mixfeed_ctl::{Level, PEAK_FLOOR_DB, State as Kind, Status};
use mixfeed_ndi::{Capture, Finder, Ndi};

/// Output sample rate of the virtual microphone.
pub const FS: u32 = 48_000;
/// Output channels (interleaved).
pub const CHANNELS: usize = 2;
/// Playout buffer target. It also has to hold one frame of the sender plus its
/// jitter (see [`target_frames`]): NDI Test Patterns sends 100 ms frames, which
/// ran an 80 ms buffer dry before every frame (about 10 underruns/s).
const TARGET_S: f64 = 0.08;
/// Margin above the largest sender frame, for arrival jitter (measured p99 on
/// Wi-Fi: 2 ms on 21 ms frames, 23 ms on 100 ms frames).
const FRAME_MARGIN_S: f64 = 0.03;
const CHUNK: usize = 64;
const RECV_NAME: &str = "mixfeed mic";
/// How long the receiver stays connected after the last client stops IO.
const LINGER: Duration = Duration::from_secs(60);
/// How long the source list stays up to date after the last read.
const BROWSE_FOR: Duration = Duration::from_secs(30);
/// A receiver without a connection to the sender for this long is shown as
/// lost. NDI gives no error when the sender stops (seen 2026-10-20 with a
/// killed sender: the receiver stayed "connected" and silent); it reconnects
/// by itself when the sender comes back. A connected sender without audio
/// (video only) stays connected; the microphone plays silence.
const NO_AUDIO: Duration = Duration::from_secs(2);
/// Wait before loading the NDI runtime again after a failure.
const NDI_RETRY: Duration = Duration::from_secs(5);
/// A receive thread whose heartbeat is older than this hangs (in an NDI call):
/// the status says so instead of showing the last state. The loop beats at
/// least every 2 s, before each pause and after each capture.
const STALL: Duration = Duration::from_secs(10);

/// What changed, for the `changed` hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The list of [`sources`].
    Sources,
    /// The [`status`] state, text or connected source.
    Status,
}

/// Callbacks into the platform layer. They may be called from any engine
/// thread, never from [`read_input`].
pub struct Hooks {
    pub log: fn(&str),
    pub changed: fn(Change),
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

fn log(msg: &str) {
    if let Some(h) = HOOKS.get() {
        (h.log)(msg);
    }
}

fn changed(c: Change) {
    if let Some(h) = HOOKS.get() {
        (h.changed)(c);
    }
}

/// Host time in seconds: mach absolute time on macOS.
#[cfg(target_os = "macos")]
pub fn host_seconds() -> f64 {
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    static NS_PER_TICK: OnceLock<f64> = OnceLock::new();
    let ns_per_tick = *NS_PER_TICK.get_or_init(|| {
        let mut tb = Timebase { numer: 0, denom: 0 };
        // SAFETY: tb is a valid out-parameter.
        unsafe { mach_timebase_info(&mut tb) };
        f64::from(tb.numer) / f64::from(tb.denom)
    });
    // SAFETY: plain libSystem call.
    let t = unsafe { mach_absolute_time() };
    t as f64 * ns_per_tick / 1e9
}

/// Host time in seconds: `CLOCK_MONOTONIC` on Linux (PipeWire's clock).
#[cfg(target_os = "linux")]
pub fn host_seconds() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid out-parameter.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// Name part of the NDI source to connect to; empty = none.
static SOURCE: Mutex<String> = Mutex::new(String::new());
static LEVEL: Mutex<Level> = Mutex::new(Level::Smpte);
/// Gain applied to the received samples, as f32 bits (0.1 = Level::Smpte).
static GAIN: AtomicU32 = AtomicU32::new(0x3dcc_cccd);
/// What the receive thread is doing, for the status.
static STATE: Mutex<State> = Mutex::new(State {
    kind: Kind::Idle,
    text: String::new(),
    source: String::new(),
});
/// A client does IO.
static IO_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Send silence instead of the source (see [`set_muted`]).
static MUTED: AtomicBool = AtomicBool::new(false);
/// Stay connected without a client (see [`set_keep_warm`]).
static KEEP_WARM: AtomicBool = AtomicBool::new(false);
/// Counts IO starts, so a stop and start between two frames still gets a new playout.
static IO_GEN: AtomicU32 = AtomicU32::new(0);
/// Time of `start_io`, for the start latency log.
static IO_START: Mutex<Option<Instant>> = Mutex::new(None);
/// The receive thread runs.
static RX_ALIVE: AtomicBool = AtomicBool::new(false);
/// Heartbeat of the receive thread, ms since [`clock_ms`] started.
static RX_BEAT: AtomicU64 = AtomicU64::new(0);
/// Installed by the receive thread; locked with `try_lock` in the IO callback.
static PLAYOUT: Mutex<Option<Playout>> = Mutex::new(None);
/// Stats of the installed playout and its input rate, for the status.
static STATS: Mutex<Option<(Arc<Stats>, u32)>> = Mutex::new(None);
static SENDER_RATE: OnceLock<Arc<SharedRate>> = OnceLock::new();
static OVERFLOWS: AtomicU64 = AtomicU64::new(0);
static PEAK: Mutex<Option<Peak>> = Mutex::new(None);

/// Peak level of the audio given to the playout, in two windows of
/// [`Peak::WINDOW`]: the status shows the larger of the current and the last
/// one, so a read every second sees each sample.
#[derive(Debug, Clone, Copy)]
struct Peak {
    start: Instant,
    current: f32,
    last: f32,
}

impl Peak {
    const WINDOW: Duration = Duration::from_secs(1);

    fn add(peak: &mut Option<Peak>, now: Instant, v: f32) {
        let p = peak.get_or_insert(Peak { start: now, current: 0.0, last: 0.0 });
        let age = now.saturating_duration_since(p.start);
        if age >= Self::WINDOW * 2 {
            *p = Peak { start: now, current: 0.0, last: 0.0 };
        } else if age >= Self::WINDOW {
            *p = Peak { start: now, current: 0.0, last: p.current };
        }
        p.current = p.current.max(v);
    }

    /// The peak in dBFS, `None` when no audio came for two windows.
    fn db(peak: Option<Peak>, now: Instant) -> Option<f64> {
        let p = peak?;
        let age = now.saturating_duration_since(p.start);
        let v = if age >= Self::WINDOW * 2 {
            return None;
        } else if age >= Self::WINDOW {
            p.current
        } else {
            p.current.max(p.last)
        };
        Some((20.0 * f64::from(v).log10()).max(PEAK_FLOOR_DB))
    }
}
/// Names of the sources the browse thread sees.
static SOURCES: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Any host on the network can announce sources: the list that goes to the
/// clients (control socket, widget, menu) has a fixed upper size.
const MAX_SOURCES: usize = 64;
/// NDI names are "HOST (Source)"; a longer name is skipped, not cut, because a
/// cut name does not select its source.
const MAX_SOURCE_NAME: usize = 256;

/// The first [`MAX_SOURCES`] names of at most [`MAX_SOURCE_NAME`] bytes, and the
/// number of names left out.
fn bounded_sources(names: impl IntoIterator<Item = String>) -> (Vec<String>, usize) {
    let mut kept = Vec::new();
    let mut left_out = 0;
    for n in names {
        if kept.len() < MAX_SOURCES && n.len() <= MAX_SOURCE_NAME {
            kept.push(n);
        } else {
            left_out += 1;
        }
    }
    (kept, left_out)
}
/// The browse thread stops after this time.
static BROWSE_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
static BROWSE_ALIVE: AtomicBool = AtomicBool::new(false);

#[derive(PartialEq, Eq)]
struct State {
    kind: Kind,
    /// Status line text, e.g. "connected STUDIO (OBS) 192.0.2.10:5961".
    text: String,
    /// Full name of the connected source; empty when not connected.
    source: String,
}

/// Register the platform hooks. Call once, first; later calls are ignored.
pub fn init(hooks: Hooks) {
    if HOOKS.set(hooks).is_ok() {
        set_state(Kind::Idle, "idle");
        log(&format!("mixfeed-audio {} init", env!("CARGO_PKG_VERSION")));
    }
}

/// Select the NDI source by name part. Empty disconnects.
pub fn set_source(name: &str) {
    log(&format!("source set to {name:?}"));
    *lock(&SOURCE) = name.to_owned();
}

/// The source name part.
pub fn source() -> String {
    lock(&SOURCE).clone()
}

pub fn set_level(level: Level) {
    log(&format!("level set to {} (x{})", level.name(), level.gain()));
    *lock(&LEVEL) = level;
    GAIN.store(level.gain().to_bits(), Ordering::Relaxed);
}

/// Keep the receiver connected while no client does IO. Turning it off
/// starts the normal linger from now.
pub fn set_keep_warm(on: bool) {
    log(&format!("keep warm {}", if on { "on" } else { "off" }));
    KEEP_WARM.store(on, Ordering::SeqCst);
    if on {
        ensure_receiver();
    }
}

pub fn keep_warm() -> bool {
    KEEP_WARM.load(Ordering::SeqCst)
}

/// Mute the microphone: the clients get silence, the receiver and the
/// playout keep running, so unmuting is instant and the clock stays locked.
pub fn set_muted(on: bool) {
    log(&format!("mute {}", if on { "on" } else { "off" }));
    MUTED.store(on, Ordering::SeqCst);
}

pub fn muted() -> bool {
    MUTED.load(Ordering::SeqCst)
}

pub fn level() -> Level {
    *lock(&LEVEL)
}

/// One status line with the main numbers, for logs and simple tools.
pub fn status_line() -> String {
    let ppm = |v: Option<f64>| v.map_or("-".into(), |r| format!("{:.2}", (r - 1.0) * 1e6));
    let mut s = format!(
        "state={} source={:?} gain=x{} io={} sender_ppm={} overflows={}",
        current_state().1,
        source(),
        f32::from_bits(GAIN.load(Ordering::Relaxed)),
        IO_ACTIVE.load(Ordering::Relaxed),
        ppm(sender_rate().load()),
        OVERFLOWS.load(Ordering::Relaxed),
    );
    if let Some((st, _)) = lock(&STATS).as_ref() {
        s += &format!(
            " fill={} ratio_ppm={} device_ppm={} underruns={} resyncs={}",
            st.fill_frames.load().map_or("-".into(), |v| format!("{v:.0}")),
            ppm(st.ratio.load()),
            ppm(st.device_rate.load()),
            st.underruns.load(Ordering::Relaxed),
            st.resyncs.load(Ordering::Relaxed),
        );
    }
    s
}

/// The current status.
pub fn status() -> Status {
    let ppm = |v: Option<f64>| v.map(|r| (r - 1.0) * 1e6);
    let mut s = {
        let (state, text, source) = current_state();
        Status {
            state,
            text,
            source,
            ..Status::default()
        }
    };
    s.io = IO_ACTIVE.load(Ordering::Relaxed);
    s.muted = MUTED.load(Ordering::Relaxed);
    s.sender_ppm = ppm(sender_rate().load());
    s.overflows = OVERFLOWS.load(Ordering::Relaxed);
    if let Some((st, rate)) = lock(&STATS).as_ref() {
        let ms = |v: Option<f64>| v.map(|v| v / f64::from(*rate) * 1e3);
        s.ratio_ppm = ppm(st.ratio.load());
        s.fill_ms = ms(st.fill_frames.load());
        s.fill_avg_ms = ms(st.fill_smoothed.load());
        s.target_ms = ms(st.fill_target.load());
        s.underruns = u64::from(st.underruns.load(Ordering::Relaxed));
        s.resyncs = u64::from(st.resyncs.load(Ordering::Relaxed));
    }
    if s.io {
        s.peak_db = Peak::db(*lock(&PEAK), Instant::now());
    }
    s
}

/// Names of the NDI sources seen. Keeps the browse thread running for
/// [`BROWSE_FOR`]; the first call after a pause gives the last known list,
/// and a [`Change::Sources`] follows when the list changes.
pub fn sources() -> Vec<String> {
    *lock(&BROWSE_UNTIL) = Some(Instant::now() + BROWSE_FOR);
    ensure_browser();
    lock(&SOURCES).clone()
}

/// First client starts IO: wake or start the receive thread.
pub fn start_io() {
    *lock(&IO_START) = Some(Instant::now());
    IO_GEN.fetch_add(1, Ordering::SeqCst);
    IO_ACTIVE.store(true, Ordering::SeqCst);
    ensure_receiver();
    log("start io");
}

/// Last client stops IO: remove the playout; the receiver lingers.
pub fn stop_io() {
    IO_ACTIVE.store(false, Ordering::SeqCst);
    *lock(&PLAYOUT) = None;
    *lock(&STATS) = None;
    log("stop io");
}

/// Fill `out` (interleaved stereo) for the IO cycle whose time is `host_s`
/// ([`host_seconds`]). Real-time thread: no allocation, no blocking lock, no
/// logging.
pub fn read_input(out: &mut [f32], host_s: f64) {
    // A panic of the receive thread leaves the lock poisoned; the playout in
    // it is still whole (the thread recovers and replaces it).
    let mut g = match PLAYOUT.try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
        Err(TryLockError::WouldBlock) => return out.fill(0.0),
    };
    match g.as_mut() {
        Some(p) => p.render(out, host_s),
        None => out.fill(0.0),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f`; a panic is caught and returned as its message. An engine thread
/// that panics must not stay marked alive (no new thread would start) or take
/// its host process down (the macOS audio server).
fn catch(f: impl FnOnce()) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into())
    })
}

/// Milliseconds on a monotonic clock that starts at the first call.
fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn beat() {
    RX_BEAT.store(clock_ms(), Ordering::Relaxed);
}

/// The receive thread runs, but has not beaten for [`STALL`].
fn stalled() -> bool {
    RX_ALIVE.load(Ordering::SeqCst)
        && clock_ms().saturating_sub(RX_BEAT.load(Ordering::Relaxed)) > STALL.as_millis() as u64
}

/// State, text and source for the status; a hanging receiver shows as an error.
fn current_state() -> (Kind, String, String) {
    let st = lock(&STATE);
    if stalled() && st.kind != Kind::Error {
        return (Kind::Error, "error: NDI receiver not responding".into(), String::new());
    }
    (st.kind, st.text.clone(), st.source.clone())
}

fn set_state(kind: Kind, text: impl Into<String>) {
    set_state_source(kind, text, String::new());
}

fn set_state_source(kind: Kind, text: impl Into<String>, source: String) {
    let new = State {
        kind,
        text: text.into(),
        source,
    };
    {
        let mut st = lock(&STATE);
        if *st == new {
            return;
        }
        log(&format!("state: {}", new.text));
        *st = new;
    }
    changed(Change::Status);
}

fn sender_rate() -> &'static Arc<SharedRate> {
    SENDER_RATE.get_or_init(|| Arc::new(SharedRate::new()))
}

/// The shared NDI runtime. Logs the first load and each new load error (the
/// receive thread tries again every [`NDI_RETRY`]); the error is the short
/// text for the status.
fn ndi() -> Result<&'static Ndi, String> {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    static LAST_ERROR: Mutex<String> = Mutex::new(String::new());
    match Ndi::shared() {
        Ok(n) => {
            if !LOGGED.swap(true, Ordering::Relaxed) {
                log(&format!("loaded {} from {}", n.version(), n.library_path().display()));
            }
            Ok(n)
        }
        Err(e) => {
            let detail = e.to_string();
            let mut last = lock(&LAST_ERROR);
            if *last != detail {
                log(&format!("NDI load failed: {detail}"));
                *last = detail;
            }
            Err(e.summary())
        }
    }
}

fn ensure_browser() {
    if BROWSE_ALIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("mixfeed-ndi-browse".into())
        .spawn(browse_thread);
    if let Err(e) = spawned {
        BROWSE_ALIVE.store(false, Ordering::SeqCst);
        log(&format!("browse thread: {e}"));
    }
}

fn browse_until() -> bool {
    lock(&BROWSE_UNTIL).is_some_and(|t| Instant::now() < t)
}

fn browse_thread() {
    loop {
        if let Err(e) = catch(browse) {
            log(&format!("browse: panic: {e}"));
            std::thread::sleep(Duration::from_secs(1));
        }
        // Exit, unless a read came in while the loop was ending.
        BROWSE_ALIVE.store(false, Ordering::SeqCst);
        if !browse_until() || BROWSE_ALIVE.swap(true, Ordering::SeqCst) {
            break;
        }
    }
}

fn browse() {
    let finder = ndi().and_then(|n| n.finder().map_err(|e| e.to_string()));
    let mut finder = match finder {
        Ok(f) => f,
        Err(e) => {
            log(&format!("browse: {e}"));
            // browse_thread calls again at once while the list is read.
            std::thread::sleep(NDI_RETRY);
            return;
        }
    };
    log("browse start");
    let mut first = true;
    while browse_until() {
        if !finder.wait(500) && !first {
            continue;
        }
        first = false;
        let (names, left_out) = bounded_sources(finder.sources().into_iter().map(|s| s.name));
        let mut cur = lock(&SOURCES);
        if *cur != names {
            if left_out > 0 {
                log(&format!("browse: {left_out} sources left out (more than {MAX_SOURCES}, or name longer than {MAX_SOURCE_NAME} bytes)"));
            }
            *cur = names;
            drop(cur);
            changed(Change::Sources);
        }
    }
    log("browse stop");
}

fn ensure_receiver() {
    if RX_ALIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    beat();
    let spawned = std::thread::Builder::new()
        .name("mixfeed-ndi-rx".into())
        .spawn(receive_thread);
    if let Err(e) = spawned {
        RX_ALIVE.store(false, Ordering::SeqCst);
        log(&format!("receive thread: {e}"));
    }
}

fn receive_thread() {
    // Wait before a new attempt after a panic; doubles while it repeats.
    let mut backoff = Duration::from_secs(1);
    loop {
        beat();
        match catch(receive) {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                *lock(&PLAYOUT) = None;
                *lock(&STATS) = None;
                PLAYOUT.clear_poison();
                set_state(Kind::Error, format!("error: receiver failed: {e}"));
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        // Exit, unless a client started IO or keep warm came on while the loop was ending.
        RX_ALIVE.store(false, Ordering::SeqCst);
        let wanted = IO_ACTIVE.load(Ordering::SeqCst) || KEEP_WARM.load(Ordering::SeqCst);
        if !wanted || RX_ALIVE.swap(true, Ordering::SeqCst) {
            break;
        }
    }
    set_state(Kind::Idle, "idle");
}

/// True while the receiver should stay connected: IO runs, keep warm is on,
/// or the last of both ended less than [`LINGER`] ago.
struct Linger {
    idle_since: Option<Instant>,
}

impl Linger {
    fn keep(&mut self) -> bool {
        if IO_ACTIVE.load(Ordering::SeqCst) || KEEP_WARM.load(Ordering::SeqCst) {
            self.idle_since = None;
            return true;
        }
        self.idle_since.get_or_insert_with(Instant::now).elapsed() < LINGER
    }
}

/// The ring of the installed playout, its input rate, IO generation and
/// buffer target (input frames).
struct Feed {
    ring: rtrb::Producer<f32>,
    rate: u32,
    io_gen: u32,
    target: usize,
}

/// Buffer target for a sender whose largest frame so far has `frame` samples.
fn target_frames(in_rate: u32, frame: usize) -> usize {
    let base = (TARGET_S * f64::from(in_rate)) as usize;
    base.max(frame + (FRAME_MARGIN_S * f64::from(in_rate)) as usize)
}

/// Find the source, connect, keep the sender rate estimate, and feed the
/// installed playout while IO runs. Reconnects when the connection drops or
/// the source setting changes. Returns when the linger time is over.
fn receive() {
    let ndi = match ndi() {
        Ok(n) => n,
        Err(e) => {
            set_state(Kind::Error, format!("error: {e}"));
            // receive_thread calls again at once while a client wants audio.
            beat();
            std::thread::sleep(NDI_RETRY);
            return;
        }
    };
    let rate = sender_rate();
    let mut linger = Linger { idle_since: None };
    let mut est = RateEstimator::new(2.0, 150, 8);
    let mut inter = Vec::with_capacity(4096 * CHANNELS);
    // One finder for the whole session: a new finder starts the network
    // discovery again (mDNS queries), every 2 s while a source is offline.
    let mut finder: Option<Finder> = None;
    while linger.keep() {
        beat();
        let part = source();
        if part.is_empty() {
            set_state(Kind::NoSource, "no source set");
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        set_state(Kind::Searching, format!("searching {part}"));
        let found = match finder.as_mut() {
            Some(f) => f.find(&part, Duration::from_secs(2)),
            None => ndi.finder().and_then(|f| finder.insert(f).find(&part, Duration::from_secs(2))),
        };
        let src = match found {
            Ok(s) => s,
            Err(mixfeed_ndi::Error::SourceNotFound { .. }) => continue,
            Err(e) => {
                set_state(Kind::Error, format!("error: {e}"));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let mut rx = match ndi.audio_receiver(&src, RECV_NAME) {
            Ok(r) => r,
            Err(e) => {
                set_state(Kind::Error, format!("error: {e}"));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        set_state_source(
            Kind::Connected,
            format!("connected {} {}", src.name, src.url),
            src.name.clone(),
        );
        est.reset();
        rate.store(None);
        // A new connection gets a new playout (fresh bridge state).
        let mut feed: Option<Feed> = None;
        *lock(&PLAYOUT) = None;
        let mut last_connected = Instant::now();
        let mut silent = false;
        // Largest frame of this connection; a larger one rebuilds the playout
        // with a target that holds it.
        let mut max_frame = 0usize;
        while linger.keep() && source() == part {
            beat();
            let f = match rx.capture(100) {
                Ok(Capture::Audio(f)) => f,
                Ok(Capture::Nothing) => {
                    if rx.connections() > 0 {
                        last_connected = Instant::now();
                        if silent {
                            silent = false;
                            set_state_source(
                                Kind::Connected,
                                format!("connected {} {}", src.name, src.url),
                                src.name.clone(),
                            );
                        }
                    } else if !silent && last_connected.elapsed() >= NO_AUDIO {
                        silent = true;
                        set_state(Kind::Lost, format!("lost: no audio from {}", src.name));
                    }
                    continue;
                }
                Err(e) => {
                    set_state(Kind::Lost, format!("lost: {e}"));
                    // Show "lost" for a moment before the search starts again.
                    std::thread::sleep(Duration::from_secs(1));
                    break;
                }
            };
            last_connected = Instant::now();
            if silent {
                silent = false;
                set_state_source(
                    Kind::Connected,
                    format!("connected {} {}", src.name, src.url),
                    src.name.clone(),
                );
            }
            let local = host_seconds();
            if f.sample_rate == 0 || f.planes.is_empty() {
                continue;
            }
            est.add(local, f.timecode as f64 / 1e7);
            rate.store(est.rate());

            if !IO_ACTIVE.load(Ordering::SeqCst) {
                feed = None;
                continue;
            }
            let io_gen = IO_GEN.load(Ordering::SeqCst);
            max_frame = max_frame.max(f.samples());
            let target = target_frames(f.sample_rate, max_frame);
            if feed
                .as_ref()
                .is_none_or(|fd| fd.rate != f.sample_rate || fd.io_gen != io_gen || fd.target < target)
            {
                feed = install_playout(f.sample_rate, io_gen, target, rate.clone());
            }
            let Some(fd) = feed.as_mut() else {
                continue;
            };
            // Mono goes to both channels; channels above 2 are dropped.
            let g = if MUTED.load(Ordering::Relaxed) {
                0.0
            } else {
                f32::from_bits(GAIN.load(Ordering::Relaxed))
            };
            let right = f.planes.get(1).unwrap_or(&f.planes[0]);
            inter.clear();
            for (l, r) in f.planes[0].iter().zip(right) {
                inter.extend_from_slice(&[*l * g, *r * g]);
            }
            let peak = inter.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            Peak::add(&mut lock(&PEAK), Instant::now(), peak);
            if fd.ring.push_entire_slice(&inter).is_err() {
                OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            }
        }
        *lock(&PLAYOUT) = None;
        *lock(&STATS) = None;
    }
}

/// Build a ring and playout for `in_rate` and install them for the IO callback.
fn install_playout(in_rate: u32, io_gen: u32, target: usize, sender_rate: Arc<SharedRate>) -> Option<Feed> {
    let (ring, consumer) = rtrb::RingBuffer::<f32>::new(in_rate as usize * CHANNELS * 2);
    let p = match Playout::new(
        consumer,
        CHANNELS,
        FS as f64,
        in_rate as f64,
        ResamplerKind::Sinc,
        CHUNK,
        target,
        sender_rate,
    ) {
        Ok(p) => p,
        Err(e) => {
            set_state(Kind::Error, format!("error: playout for {in_rate} Hz: {e}"));
            return None;
        }
    };
    *lock(&STATS) = Some((p.stats(), in_rate));
    *lock(&PLAYOUT) = Some(p);
    // IO stopped while this was built: do not leave a playout behind.
    if !IO_ACTIVE.load(Ordering::SeqCst) || IO_GEN.load(Ordering::SeqCst) != io_gen {
        *lock(&PLAYOUT) = None;
        *lock(&STATS) = None;
        return None;
    }
    let since = lock(&IO_START).map_or(0.0, |t| t.elapsed().as_secs_f64() * 1e3);
    log(&format!(
        "playout for {in_rate} Hz (target {:.0} ms) installed {since:.0} ms after start io",
        target as f64 * 1e3 / f64::from(in_rate)
    ));
    Some(Feed {
        ring,
        rate: in_rate,
        io_gen,
        target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_holds_the_last_window() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut p = None;
        assert_eq!(Peak::db(p, t0), None);
        Peak::add(&mut p, t0, 0.5);
        Peak::add(&mut p, ms(500), 0.25);
        let db = Peak::db(p, ms(600)).unwrap();
        assert!((db + 6.02).abs() < 0.01, "{db}");
        // A new window keeps the last one's peak.
        Peak::add(&mut p, ms(1100), 0.1);
        assert!((Peak::db(p, ms(1200)).unwrap() + 6.02).abs() < 0.01);
        // After a full window only the new peak is left.
        assert!((Peak::db(p, ms(2200)).unwrap() + 20.0).abs() < 0.01);
        // No audio for two windows: unknown.
        assert_eq!(Peak::db(p, ms(3200)), None);
        // Silence is the floor, not minus infinity.
        let mut q = None;
        Peak::add(&mut q, t0, 0.0);
        assert_eq!(Peak::db(q, t0), Some(PEAK_FLOOR_DB));
    }

    #[test]
    fn catch_returns_the_panic_message() {
        assert_eq!(catch(|| {}), Ok(()));
        assert_eq!(catch(|| panic!("boom")), Err("boom".to_owned()));
        assert_eq!(catch(|| panic!("{} {}", "fmt", 1)), Err("fmt 1".to_owned()));
    }

    #[test]
    fn read_input_gives_silence_with_a_poisoned_lock() {
        let _ = std::thread::spawn(|| {
            let _g = PLAYOUT.lock().unwrap();
            panic!("poison the playout lock");
        })
        .join();
        assert!(PLAYOUT.is_poisoned());
        let mut out = [1.0f32; 8];
        read_input(&mut out, 0.0);
        assert_eq!(out, [0.0; 8]);
        PLAYOUT.clear_poison();
    }

    #[test]
    fn target_holds_one_sender_frame() {
        // 1024-sample frames (OBS/DistroAV): the normal 80 ms.
        assert_eq!(target_frames(48_000, 1024), 3840);
        // 100 ms frames (NDI Test Patterns): frame + 30 ms = 130 ms.
        assert_eq!(target_frames(48_000, 4800), 6240);
    }

    #[test]
    fn a_hanging_receiver_shows_as_an_error() {
        set_state(Kind::Connected, "connected test");
        RX_ALIVE.store(true, Ordering::SeqCst);
        beat();
        assert_eq!(status().state, Kind::Connected);
        RX_BEAT.store(0, Ordering::Relaxed);
        std::thread::sleep(STALL + Duration::from_millis(50));
        let s = status();
        assert_eq!(s.state, Kind::Error);
        assert_eq!(s.text, "error: NDI receiver not responding");
        RX_ALIVE.store(false, Ordering::SeqCst);
        assert_eq!(status().state, Kind::Connected);
    }

    #[test]
    fn source_list_has_an_upper_size() {
        let long = "H (".to_string() + &"x".repeat(MAX_SOURCE_NAME) + ")";
        let names = (0..MAX_SOURCES + 5).map(|i| format!("HOST (S{i})"));
        let (kept, left_out) = bounded_sources(std::iter::once(long).chain(names));
        assert_eq!(kept.len(), MAX_SOURCES);
        assert_eq!(kept[0], "HOST (S0)");
        assert_eq!(left_out, 6);
    }
}
