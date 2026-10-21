//! mixfeed-cam: NDI video receive for a virtual camera (the Linux camera in
//! `linux/mixfeedd` and the macOS camera extension), with a C interface.
//!
//! The camera registers a frame callback with [`mixfeed_cam_init`] and calls
//! [`mixfeed_cam_start`] / [`mixfeed_cam_stop`] when its stream starts and stops.
//! One receive thread connects to the selected source while the stream runs
//! and calls the callback with each frame, on the receive thread. The pixels
//! are NDI's buffer: valid only during the call, so the camera copies or
//! scales them once into its own buffer.
//!
//! After the stream stops, the receiver stays connected for [`LINGER`] only:
//! full-bandwidth video costs network and CPU, unlike the audio linger.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use mixfeed_ndi::{Finder, Ndi, VideoCapture};

const RECV_NAME: &str = "mixfeed camera";
/// How long the receiver stays connected after the stream stops.
const LINGER: Duration = Duration::from_secs(5);
/// A receiver without a connection to the sender for this long is shown as
/// lost (NDI gives no error when the sender stops, and reconnects by itself).
/// A connected sender may send no frames for a long time: a still picture
/// (NDI Test Patterns, a scan converter) is sent once, and again on a change.
const NO_VIDEO: Duration = Duration::from_secs(2);
/// Wait before loading the NDI runtime again after a failure.
const NDI_RETRY: Duration = Duration::from_secs(5);
/// A receive thread whose heartbeat is older than this hangs (in an NDI call);
/// the status says so. The loop beats at least every 2 s.
const STALL: Duration = Duration::from_secs(10);

/// One video frame, as given to the frame callback.
#[repr(C)]
pub struct MixfeedVideoFrame {
    pub width: u32,
    pub height: u32,
    /// FourCC as NDI gives it, little-endian ("UYVY" or "BGRA").
    pub fourcc: u32,
    /// Bytes from one line to the next.
    pub stride: u32,
    pub data: *const u8,
    pub rate_n: u32,
    pub rate_d: u32,
    /// NDI timestamp in 100 ns units (sender clock), or i64::MAX if not set.
    pub timestamp: i64,
}

pub type FrameFn = extern "C" fn(ctx: *mut c_void, frame: *const MixfeedVideoFrame);
pub type LogFn = extern "C" fn(msg: *const c_char);

struct Callbacks {
    log: LogFn,
    frame: FrameFn,
    ctx: usize,
}

static CALLBACKS: OnceLock<Callbacks> = OnceLock::new();
/// Name part of the NDI source; empty = none.
static SOURCE: Mutex<String> = Mutex::new(String::new());
static STATE: Mutex<String> = Mutex::new(String::new());
/// The receiver has a connection to the sender (also between frames).
static CONNECTED: AtomicBool = AtomicBool::new(false);
/// Counts the receivers; tells the frames of one connection from the next.
static CONNECTION: AtomicU64 = AtomicU64::new(0);
/// The stream runs.
static STREAMING: AtomicBool = AtomicBool::new(false);
static RX_ALIVE: AtomicBool = AtomicBool::new(false);
/// Heartbeat of the receive thread, ms since [`clock_ms`] started.
static RX_BEAT: AtomicU64 = AtomicU64::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);
/// Last frame of the current connection: width << 32 | height, FourCC, and
/// frame rate numerator << 32 | denominator; 0 before the first.
static VIDEO_SIZE: AtomicU64 = AtomicU64::new(0);
static VIDEO_FOURCC: AtomicU64 = AtomicU64::new(0);
/// Frames dropped because their format or line length is wrong.
static MALFORMED: AtomicU64 = AtomicU64::new(0);
static VIDEO_RATE: AtomicU64 = AtomicU64::new(0);
/// How long "lost" stays shown after a connection error before the search
/// starts again (else it flickers straight to "searching").
const LOST_HOLD: Duration = Duration::from_secs(1);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f`; a panic is caught and returned as its message. A panic must not
/// unwind into the extension (that aborts it) or leave the receive thread
/// marked alive (no new thread would start).
fn catch<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into())
    })
}

/// Body of an exported function: a panic is logged, not unwound into the caller.
fn guard(name: &str, f: impl FnOnce()) {
    if let Err(e) = catch(f) {
        log(&format!("{name}: panic: {e}"));
    }
}

fn log(msg: &str) {
    if let (Some(cb), Ok(c)) = (CALLBACKS.get(), CString::new(msg)) {
        (cb.log)(c.as_ptr());
    }
}

/// Milliseconds on a monotonic clock that starts at the first call.
fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn beat() {
    RX_BEAT.store(clock_ms(), Ordering::Relaxed);
}

/// The current receiver while it has a connection to the sender, also while
/// the sender sends no new frames (a still picture): a camera repeats its last
/// picture of this connection then.
pub fn connection() -> Option<u64> {
    CONNECTED.load(Ordering::SeqCst).then(|| CONNECTION.load(Ordering::SeqCst))
}

/// The state for the status; a hanging receiver shows as an error.
fn current_state() -> String {
    let st = lock(&STATE).clone();
    let stalled = RX_ALIVE.load(Ordering::SeqCst)
        && clock_ms().saturating_sub(RX_BEAT.load(Ordering::Relaxed)) > STALL.as_millis() as u64;
    if stalled && !st.starts_with("error") {
        return "error: NDI receiver not responding".into();
    }
    st
}

fn set_state(s: impl Into<String>) {
    let s = s.into();
    CONNECTED.store(s.starts_with("connected"), Ordering::SeqCst);
    let mut st = lock(&STATE);
    if *st != s {
        log(&format!("state: {s}"));
        *st = s;
    }
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

/// Register the callbacks. Call once, before anything else; later calls are ignored.
/// `ctx` is passed to `frame` unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn mixfeed_cam_init(log_fn: LogFn, frame_fn: FrameFn, ctx: *mut c_void) {
    let ctx = ctx as usize;
    guard("init", || {
        let first = CALLBACKS
            .set(Callbacks {
                log: log_fn,
                frame: frame_fn,
                ctx,
            })
            .is_ok();
        if first {
            set_state("idle");
            log(&format!("mixfeed-cam {} init", env!("CARGO_PKG_VERSION")));
        }
    })
}

/// Select the NDI source by name part (UTF-8). Empty disconnects.
///
/// # Safety
/// `name` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mixfeed_cam_set_source(name: *const c_char) {
    // SAFETY: guaranteed by the caller.
    let s = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
    guard("set_source", || {
        log(&format!("source set to {s:?}"));
        *lock(&SOURCE) = s;
    })
}

/// The stream starts: connect (or keep the lingering connection).
#[unsafe(no_mangle)]
pub extern "C" fn mixfeed_cam_start() {
    guard("start", || {
        STREAMING.store(true, Ordering::SeqCst);
        log("start stream");
        if RX_ALIVE.swap(true, Ordering::SeqCst) {
            return;
        }
        beat();
        let spawned = std::thread::Builder::new()
            .name("mixfeed-ndi-video".into())
            .spawn(receive_thread);
        if let Err(e) = spawned {
            RX_ALIVE.store(false, Ordering::SeqCst);
            log(&format!("receive thread: {e}"));
        }
    })
}

/// The stream stops: frames are no longer delivered; the receiver lingers.
#[unsafe(no_mangle)]
pub extern "C" fn mixfeed_cam_stop() {
    guard("stop", || {
        STREAMING.store(false, Ordering::SeqCst);
        log("stop stream");
    })
}

/// The current NDI connection while it has a connection to the sender, else
/// 0 (see [`connection`]): a camera repeats the last picture of this
/// connection between frames.
#[unsafe(no_mangle)]
pub extern "C" fn mixfeed_cam_connection() -> u64 {
    connection().unwrap_or(0)
}

/// Write the status line (UTF-8, NUL-terminated, cut to `len`) into `buf`:
/// `state=<state> source="<name part>" streaming=<bool> frames=<n>`, then
/// ` dropped=<n>` after malformed frames (wrong format or line length), and
/// ` video=<W>x<H> <FourCC> <rate_n>/<rate_d>` once a frame came.
///
/// # Safety
/// `buf` must point to `len` writable bytes, `len > 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mixfeed_cam_status(buf: *mut c_char, len: usize) {
    let s = catch(|| {
        let mut s = format!(
            "state={} source={:?} streaming={} frames={}",
            current_state(),
            lock(&SOURCE),
            STREAMING.load(Ordering::Relaxed),
            FRAMES.load(Ordering::Relaxed)
        );
        let dropped = MALFORMED.load(Ordering::Relaxed);
        if dropped != 0 {
            s += &format!(" dropped={dropped}");
        }
        let size = VIDEO_SIZE.load(Ordering::Relaxed);
        if size != 0 {
            let cc = (VIDEO_FOURCC.load(Ordering::Relaxed) as u32).to_le_bytes();
            let rate = VIDEO_RATE.load(Ordering::Relaxed);
            s += &format!(
                " video={}x{} {} {}/{}",
                size >> 32,
                size as u32,
                String::from_utf8_lossy(&cc),
                rate >> 32,
                rate as u32
            );
        }
        s
    })
    .unwrap_or_else(|e| format!("state=error: status: {e}"));
    let mut n = s.len().min(len - 1);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    // SAFETY: buf has len bytes; n < len.
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf.cast::<u8>(), n);
        *buf.add(n) = 0;
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
                set_state(format!("error: receiver failed: {e}"));
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        // Exit, unless the stream started again while the loop was ending.
        RX_ALIVE.store(false, Ordering::SeqCst);
        if !STREAMING.load(Ordering::SeqCst) || RX_ALIVE.swap(true, Ordering::SeqCst) {
            break;
        }
    }
    set_state("idle");
}

/// True while the stream runs or stopped less than [`LINGER`] ago.
struct Linger {
    idle_since: Option<Instant>,
}

impl Linger {
    fn keep(&mut self) -> bool {
        if STREAMING.load(Ordering::SeqCst) {
            self.idle_since = None;
            return true;
        }
        self.idle_since.get_or_insert_with(Instant::now).elapsed() < LINGER
    }
}

fn source() -> String {
    lock(&SOURCE).clone()
}

/// Find the source, connect and deliver frames while the stream runs.
/// Reconnects when the connection drops or the source changes. Returns
/// when the linger time is over.
fn receive() {
    let ndi = match ndi() {
        Ok(n) => n,
        Err(e) => {
            set_state(format!("error: {e}"));
            // receive_thread calls again at once while the stream runs.
            beat();
            std::thread::sleep(NDI_RETRY);
            return;
        }
    };
    let Some(cb) = CALLBACKS.get() else {
        return set_state("error: not initialised");
    };
    let mut linger = Linger { idle_since: None };
    // One finder for the whole session: a new finder starts the network
    // discovery again (mDNS queries), every 2 s while a source is offline.
    let mut finder: Option<Finder> = None;
    while linger.keep() {
        beat();
        let part = source();
        if part.is_empty() {
            set_state("no source set");
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        set_state(format!("searching {part}"));
        let found = match finder.as_mut() {
            Some(f) => f.find(&part, Duration::from_secs(2)),
            None => ndi.finder().and_then(|f| finder.insert(f).find(&part, Duration::from_secs(2))),
        };
        let src = match found {
            Ok(s) => s,
            Err(mixfeed_ndi::Error::SourceNotFound { .. }) => continue,
            Err(e) => {
                set_state(format!("error: {e}"));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let mut rx = match ndi.video_receiver(&src, RECV_NAME) {
            Ok(r) => r,
            Err(e) => {
                set_state(format!("error: {e}"));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        CONNECTION.fetch_add(1, Ordering::SeqCst);
        for v in [&VIDEO_SIZE, &VIDEO_FOURCC, &VIDEO_RATE] {
            v.store(0, Ordering::Relaxed);
        }
        set_state(format!("connected {} {}", src.name, src.url));
        let mut last_connected = Instant::now();
        let mut silent = false;
        while linger.keep() && source() == part {
            beat();
            // Read before the capture: a frame borrows the receiver.
            let connections = rx.connections();
            let f = match rx.capture(100) {
                Ok(VideoCapture::Video(f)) => f,
                Ok(VideoCapture::Nothing) => {
                    if connections > 0 {
                        last_connected = Instant::now();
                        if silent {
                            silent = false;
                            set_state(format!("connected {} {}", src.name, src.url));
                        }
                    } else if !silent && last_connected.elapsed() >= NO_VIDEO {
                        silent = true;
                        set_state(format!("lost: no video from {}", src.name));
                    }
                    continue;
                }
                Err(e) => {
                    set_state(format!("lost: {e}"));
                    std::thread::sleep(LOST_HOLD);
                    break;
                }
            };
            last_connected = Instant::now();
            if silent {
                silent = false;
                set_state(format!("connected {} {}", src.name, src.url));
            }
            if !STREAMING.load(Ordering::SeqCst) {
                continue;
            }
            if !f.well_formed() {
                if MALFORMED.fetch_add(1, Ordering::Relaxed) == 0 {
                    let cc = f.four_cc().to_le_bytes();
                    log(&format!(
                        "dropping malformed frames (first: {}x{} {} stride {})",
                        f.width(),
                        f.height(),
                        String::from_utf8_lossy(&cc),
                        f.stride()
                    ));
                }
                continue;
            }
            let (rate_n, rate_d) = f.frame_rate();
            VIDEO_SIZE.store(u64::from(f.width()) << 32 | u64::from(f.height()), Ordering::Relaxed);
            VIDEO_FOURCC.store(u64::from(f.four_cc()), Ordering::Relaxed);
            VIDEO_RATE.store(u64::from(rate_n) << 32 | u64::from(rate_d), Ordering::Relaxed);
            let frame = MixfeedVideoFrame {
                width: f.width(),
                height: f.height(),
                fourcc: f.four_cc(),
                stride: f.stride() as u32,
                data: f.data().as_ptr(),
                rate_n,
                rate_d,
                timestamp: f.timestamp(),
            };
            (cb.frame)(cb.ctx as *mut c_void, &frame);
            FRAMES.fetch_add(1, Ordering::Relaxed);
        }
    }
}
