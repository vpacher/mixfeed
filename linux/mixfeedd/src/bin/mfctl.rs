//! mfctl: command-line client of mixfeedd.
//!
//! Usage: mfctl status | info | sources | source [<NDI source name part>] | level [raw|smpte|ebu] | warm [on|off] | mute [on|off]
//!        mfctl monitor [on [<output node name>] | off]   (headphone monitor)
//!        mfctl json [--sources]   (one JSON line for scripts and the bar widget)
//!        mfctl ndi   (load the NDI runtime as mixfeedd does; used by install.sh)

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;

use mixfeed_ctl::Level;
use mixfeed_ctl::socket::{self, Request, Response};

const USAGE: &str =
    "usage: mfctl status | info | sources | source [<NDI source name part>] | level [raw|smpte|ebu] | warm [on|off] | mute [on|off] | monitor [on [<output>]|off] | json [--sources] | ndi";

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    exit(1)
}

fn ask(req: &Request) -> Response {
    let path = socket::path().unwrap_or_else(|| fail("XDG_RUNTIME_DIR is not set"));
    let mut s = UnixStream::connect(&path)
        .unwrap_or_else(|e| fail(&format!("mixfeedd not running ({}: {e})", path.display())));
    let line = serde_json::to_string(req).expect("request serialises");
    writeln!(s, "{line}").unwrap_or_else(|e| fail(&e.to_string()));
    let mut resp = String::new();
    BufReader::new(s)
        .read_line(&mut resp)
        .unwrap_or_else(|e| fail(&e.to_string()));
    match serde_json::from_str(&resp) {
        Ok(Response::Error(e)) => fail(&e),
        Ok(r) => r,
        Err(e) => fail(&format!("bad response: {e}")),
    }
}

fn main() {
    // Rust ignores SIGPIPE, so printing to a closed pipe (`mfctl info | head -1`)
    // panics; with the default action mfctl ends quietly like other tools.
    // SAFETY: called before any other thread exists.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |v: Option<f64>| v.map_or("-".into(), |x| format!("{x:.2}"));
    match args.first().map(String::as_str) {
        Some("status") | Some("info") => {
            let Response::Status(s) = ask(&Request::Status) else { fail("unexpected response") };
            if args[0] == "status" {
                println!("{}", s.text);
                return;
            }
            println!(
                "state={} source={} io={} on_air={} muted={} monitor={}",
                serde_json::to_string(&s.state).unwrap().trim_matches('"'),
                s.source,
                s.io,
                s.on_air,
                s.muted,
                s.monitor.as_deref().unwrap_or("off")
            );
            println!("sender_ppm={} ratio_ppm={} fill_ms={}", num(s.sender_ppm), num(s.ratio_ppm), num(s.fill_ms));
            println!("fill_avg_ms={} target_ms={}", num(s.fill_avg_ms), num(s.target_ms));
            println!("underruns={} resyncs={} overflows={}", s.underruns, s.resyncs, s.overflows);
            println!("peak_db={}", num(s.peak_db));
            if let Some(c) = s.camera.as_ref().filter(|c| c.error.is_some()) {
                println!("camera {} off: {}", c.device, c.error.as_deref().unwrap_or_default());
            } else if let Some(c) = &s.camera {
                println!(
                    "camera {} live={} frames={} source={}x{} {} {}/{} fps",
                    c.device,
                    c.live,
                    c.frames,
                    c.source_width,
                    c.source_height,
                    c.source_fourcc,
                    c.source_rate_n,
                    c.source_rate_d
                );
                println!("camera {}", c.text);
            }
        }
        Some("json") => {
            // Status and settings; with --sources also the source list as the
            // daemon has it now (the request keeps it browsing).
            let Response::Status(status) = ask(&Request::Status) else { fail("unexpected response") };
            let Response::Settings(settings) = ask(&Request::Get) else { fail("unexpected response") };
            let mut v = serde_json::json!({ "status": status, "settings": settings });
            if args.get(1).map(String::as_str) == Some("--sources") {
                let Response::Sources(names) = ask(&Request::Sources) else { fail("unexpected response") };
                v["sources"] = serde_json::json!(names);
            }
            println!("{v}");
        }
        Some("sources") => {
            // The first request starts the browsing; give it time to find sources.
            ask(&Request::Sources);
            std::thread::sleep(std::time::Duration::from_secs(2));
            let Response::Sources(names) = ask(&Request::Sources) else { fail("unexpected response") };
            for n in names {
                println!("{n}");
            }
        }
        Some("source") => {
            let source = (args.len() > 1).then(|| args[1..].join(" "));
            let req = if source.is_some() {
                Request::Set { source, level: None, keep_warm: None, muted: None }
            } else {
                Request::Get
            };
            let Response::Settings(s) = ask(&req) else { fail("unexpected response") };
            println!("{}", s.source);
        }
        Some("level") => {
            let level = args.get(1).map(|l| Level::from_name(l).unwrap_or_else(|| fail(USAGE)));
            let req = if level.is_some() {
                Request::Set { source: None, level, keep_warm: None, muted: None }
            } else {
                Request::Get
            };
            let Response::Settings(s) = ask(&req) else { fail("unexpected response") };
            println!("{}", s.level.name());
        }
        Some("monitor") => {
            let req = match args.get(1).map(String::as_str) {
                None => Request::Status,
                Some("on") => Request::Monitor { on: true, target: args.get(2).cloned() },
                Some("off") => Request::Monitor { on: false, target: None },
                Some(_) => fail(USAGE),
            };
            let Response::Status(s) = ask(&req) else { fail("unexpected response") };
            println!("{}", s.monitor.as_deref().unwrap_or("off"));
        }
        Some("warm") => {
            // Keep the microphone receiver connected without a client.
            let keep_warm = args.get(1).map(|w| match w.as_str() {
                "on" => true,
                "off" => false,
                _ => fail(USAGE),
            });
            let req = if keep_warm.is_some() {
                Request::Set { source: None, level: None, keep_warm, muted: None }
            } else {
                Request::Get
            };
            let Response::Settings(s) = ask(&req) else { fail("unexpected response") };
            println!("{}", if s.keep_warm { "on" } else { "off" });
        }
        Some("mute") => {
            // The microphone sends silence and stays connected.
            let muted = args.get(1).map(|w| match w.as_str() {
                "on" => true,
                "off" => false,
                _ => fail(USAGE),
            });
            let req = if muted.is_some() {
                Request::Set { source: None, level: None, keep_warm: None, muted }
            } else {
                Request::Get
            };
            let Response::Settings(s) = ask(&req) else { fail("unexpected response") };
            println!("{}", if s.muted { "on" } else { "off" });
        }
        Some("ndi") => {
            // Runs without mixfeedd: same library search and symbol check.
            match mixfeed_ndi::Ndi::load() {
                Ok(ndi) => println!("{} ({})", ndi.version(), ndi.library_path().display()),
                Err(e) => fail(&format!(
                    "NDI runtime: {e}\nInstall the NDI runtime, version 6 (Arch: AUR package ndi-sdk)."
                )),
            }
        }
        _ => fail(USAGE),
    }
}
