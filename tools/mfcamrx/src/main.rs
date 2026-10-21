//! mfcamrx: receive NDI video and report what arrives (test tool).
//!
//! Usage: mfcamrx <source name part> <seconds> <outdir>
//! Prints format, frame rate and arrival jitter, writes <outdir>/frames.csv
//! (one line per frame) and <outdir>/frame.ppm (the first UYVY frame as RGB).

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use mixfeed_ndi::{FOURCC_UYVY, Ndi, VideoCapture, VideoFrame};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        bail!("usage: mfcamrx <source name part> <seconds> <outdir>");
    }
    let name_part = &args[1];
    let seconds: f64 = args[2].parse().context("seconds")?;
    let out = PathBuf::from(&args[3]);
    fs::create_dir_all(&out)?;

    let ndi = Ndi::load()?;
    println!("NDI {} ({})", ndi.version(), ndi.library_path().display());
    let source = ndi.find_source(name_part, Duration::from_secs(15))?;
    println!("source: {} {}", source.name, source.url);
    let t_connect = Instant::now();
    let mut rx = ndi.video_receiver(&source, "mixfeed mfcamrx")?;

    let mut csv = BufWriter::new(File::create(out.join("frames.csv"))?);
    writeln!(csv, "index,rx_s,timestamp_100ns,timecode_100ns,width,height,fourcc,stride")?;
    let end = Duration::from_secs_f64(seconds);
    let mut arrivals: Vec<f64> = Vec::new();
    let mut first_frame_ms = None;
    let mut wrote_ppm = false;
    while t_connect.elapsed() < end {
        let f = match rx.capture(100)? {
            VideoCapture::Video(f) => f,
            VideoCapture::Nothing => continue,
        };
        let t = t_connect.elapsed().as_secs_f64();
        if first_frame_ms.is_none() {
            first_frame_ms = Some(t * 1e3);
            let (n, d) = f.frame_rate();
            let cc = f.four_cc().to_le_bytes();
            println!(
                "first frame after {:.0} ms: {}x{} {} stride {} rate {n}/{d} progressive {}",
                t * 1e3,
                f.width(),
                f.height(),
                String::from_utf8_lossy(&cc),
                f.stride(),
                f.progressive()
            );
        }
        if !wrote_ppm && f.four_cc() == FOURCC_UYVY {
            write_ppm(&f, &out.join("frame.ppm"))?;
            wrote_ppm = true;
        }
        writeln!(
            csv,
            "{},{t:.6},{},{},{},{},{:08x},{}",
            arrivals.len(),
            f.timestamp(),
            f.timecode(),
            f.width(),
            f.height(),
            f.four_cc(),
            f.stride()
        )?;
        arrivals.push(t);
    }
    csv.flush()?;

    if arrivals.len() < 3 {
        bail!("only {} frames received", arrivals.len());
    }
    let span = arrivals[arrivals.len() - 1] - arrivals[0];
    let fps = (arrivals.len() - 1) as f64 / span;
    let mut gaps: Vec<f64> = arrivals.windows(2).map(|w| (w[1] - w[0]) * 1e3).collect();
    gaps.sort_by(f64::total_cmp);
    let pct = |p: f64| gaps[((gaps.len() - 1) as f64 * p) as usize];
    println!(
        "{} frames, {fps:.3} fps; interval ms min {:.1} median {:.1} p99 {:.1} max {:.1}",
        arrivals.len(),
        gaps[0],
        pct(0.5),
        pct(0.99),
        gaps[gaps.len() - 1]
    );
    Ok(())
}

/// Convert a UYVY frame (BT.709, video range) to an RGB PPM.
fn write_ppm(f: &VideoFrame<'_>, path: &Path) -> anyhow::Result<()> {
    let (w, h) = (f.width() as usize, f.height() as usize);
    let mut out = BufWriter::new(File::create(path)?);
    write!(out, "P6\n{w} {h}\n255\n")?;
    let data = f.data();
    let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    for y in 0..h {
        let line = &data[y * f.stride()..];
        for x in (0..w).step_by(2) {
            let p = &line[x * 2..x * 2 + 4];
            let (u, y0, v, y1) = (p[0] as f32 - 128.0, p[1], p[2] as f32 - 128.0, p[3]);
            for yy in [y0, y1] {
                let c = (yy as f32 - 16.0) * 1.164;
                out.write_all(&[
                    clamp(c + 1.793 * v),
                    clamp(c - 0.213 * u - 0.533 * v),
                    clamp(c + 2.112 * u),
                ])?;
            }
        }
    }
    out.flush()?;
    Ok(())
}
