//! Control socket and settings file.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Mutex;

use mixfeed_ctl::Settings;
use mixfeed_ctl::socket::{self, Request, Response};

/// Serialises settings changes and their file writes.
static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("mixfeed").join("settings.json"))
}

/// The saved settings, or the defaults. A file that cannot be read is moved to
/// `settings.json.bad` (with a log line), so the next save does not overwrite
/// it unseen.
pub fn load_settings() -> Settings {
    let s = settings_path().map_or_else(Settings::default, |p| {
        let parsed = match std::fs::read_to_string(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
            Err(e) => Err(e.to_string()),
            Ok(t) => serde_json::from_str(&t).map_err(|e| e.to_string()),
        };
        parsed.unwrap_or_else(|e| {
            let bad = p.with_extension("json.bad");
            let moved = std::fs::rename(&p, &bad).map_or_else(|e| format!("not moved: {e}"), |()| format!("moved to {}", bad.display()));
            eprintln!("settings {}: {e}; using the defaults ({moved})", p.display());
            Settings::default()
        })
    });
    *SETTINGS.lock().unwrap_or_else(|e| e.into_inner()) = Some(Settings::clone(&s));
    s
}

fn save(s: &Settings) {
    let Some(p) = settings_path() else { return };
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(p.parent().expect("settings path has a parent"))?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(s)? + "\n")?;
        std::fs::rename(tmp, &p)
    };
    if let Err(e) = write() {
        eprintln!("cannot save settings to {}: {e}", p.display());
    }
}

fn apply(
    source: Option<String>,
    level: Option<mixfeed_ctl::Level>,
    keep_warm: Option<bool>,
    muted: Option<bool>,
) -> Settings {
    let mut g = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(Settings::default);
    if let Some(src) = source {
        mixfeed_audio::set_source(&src);
        crate::camera::set_source(&src);
        s.source = src;
    }
    if let Some(l) = level {
        mixfeed_audio::set_level(l);
        s.level = l;
    }
    if let Some(w) = keep_warm {
        mixfeed_audio::set_keep_warm(w);
        s.keep_warm = w;
    }
    if let Some(m) = muted {
        mixfeed_audio::set_muted(m);
        s.muted = m;
    }
    save(s);
    s.clone()
}

fn handle(req: Request) -> Response {
    match req {
        Request::Status => status(),
        Request::Monitor { on, target } => match crate::monitor::set(on, target) {
            Ok(()) => status(),
            Err(e) => Response::Error(e),
        },
        Request::Sources => Response::Sources(mixfeed_audio::sources()),
        Request::Get => Response::Settings(Settings {
            source: mixfeed_audio::source(),
            level: mixfeed_audio::level(),
            keep_warm: mixfeed_audio::keep_warm(),
            muted: mixfeed_audio::muted(),
        }),
        Request::Set {
            source,
            level,
            keep_warm,
            muted,
        } => Response::Settings(apply(source, level, keep_warm, muted)),
    }
}

fn status() -> Response {
    Response::Status(Box::new(mixfeed_ctl::Status {
        camera: crate::camera::status(),
        // Muted, an application still takes audio, but gets silence.
        on_air: crate::source::on_air() && !mixfeed_audio::muted(),
        monitor: crate::monitor::target(),
        ..mixfeed_audio::status()
    }))
}

/// Longest request line. Requests are small; a longer line ends the connection
/// instead of growing a buffer without limit.
const MAX_REQUEST: u64 = 64 * 1024;

fn serve(stream: UnixStream) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if (&mut reader).take(MAX_REQUEST + 1).read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line.len() as u64 > MAX_REQUEST {
            let resp = Response::Error(format!("request longer than {MAX_REQUEST} bytes"));
            writeln!(out, "{}", serde_json::to_string(&resp)?)?;
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle(req),
            Err(e) => Response::Error(format!("bad request: {e}")),
        };
        writeln!(out, "{}", serde_json::to_string(&resp)?)?;
    }
}

/// Listen on the control socket in a background thread.
pub fn spawn() -> std::io::Result<()> {
    let path = socket::path().ok_or_else(|| std::io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    let dir = path.parent().expect("socket path has a parent");
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    // A socket file left by an earlier run.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    eprintln!("control socket {}", path.display());
    std::thread::Builder::new().name("mixfeedd-control".into()).spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(c) => {
                    std::thread::spawn(move || {
                        if let Err(e) = serve(c) {
                            eprintln!("control client: {e}");
                        }
                    });
                }
                Err(e) => eprintln!("control socket: {e}"),
            }
        }
    })?;
    Ok(())
}
