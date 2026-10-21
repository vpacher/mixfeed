//! mfrx: receive NDI audio and write it as received (test tool).
//!
//! Usage: mfrx <source name part> <seconds> <outdir>
//! Writes <outdir>/ndi_rx.wav (32-bit float) and <outdir>/ndi_rx_frames.csv,
//! in the same format as the Python reference receiver that verified it.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use mixfeed_ndi::{Capture, FOURCC_FLTP, Ndi};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        bail!("usage: mfrx <source name part> <seconds> <outdir>");
    }
    let name_part = &args[1];
    let seconds: f64 = args[2].parse().context("seconds")?;
    let out = PathBuf::from(&args[3]);
    fs::create_dir_all(&out)?;

    let ndi = Ndi::load()?;
    println!("NDI {} ({})", ndi.version(), ndi.library_path().display());
    let source = ndi.find_source(name_part, Duration::from_secs(15))?;
    println!("source: {} {}", source.name, source.url);
    let mut rx = ndi.audio_receiver(&source, "mixfeed mfrx")?;

    let mut wav: Option<hound::WavWriter<BufWriter<File>>> = None;
    let mut csv = BufWriter::new(File::create(out.join("ndi_rx_frames.csv"))?);
    writeln!(
        csv,
        "index,rx_monotonic_ns,timestamp_100ns,timecode_100ns,sample_rate,channels,samples"
    )?;
    let t0 = Instant::now();
    let end = Duration::from_secs_f64(seconds);
    let (mut frames, mut samples, mut peak) = (0u64, 0u64, 0f32);
    let mut sizes = std::collections::BTreeSet::new();
    while t0.elapsed() < end {
        let Capture::Audio(f) = rx.capture(100)? else {
            continue;
        };
        let rx_ns = t0.elapsed().as_nanos();
        if f.four_cc != FOURCC_FLTP {
            eprintln!("unexpected FourCC {:#x}", f.four_cc);
        }
        let nch = f.planes.len();
        let w = match &mut wav {
            Some(w) => w,
            None => {
                let spec = hound::WavSpec {
                    channels: nch as u16,
                    sample_rate: f.sample_rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                };
                wav.insert(hound::WavWriter::create(out.join("ndi_rx.wav"), spec)?)
            }
        };
        for i in 0..f.samples() {
            for plane in &f.planes {
                w.write_sample(plane[i])?;
                peak = peak.max(plane[i].abs());
            }
        }
        writeln!(
            csv,
            "{frames},{rx_ns},{},{},{},{nch},{}",
            f.timestamp,
            f.timecode,
            f.sample_rate,
            f.samples()
        )?;
        sizes.insert(f.samples());
        frames += 1;
        samples += f.samples() as u64;
    }
    if let Some(w) = wav {
        w.finalize()?;
    }
    csv.flush()?;
    if frames == 0 {
        bail!("no audio frames received");
    }
    println!(
        "frames {frames}, samples {samples}, sizes {sizes:?}, peak {:.1} dBFS",
        20.0 * peak.max(1e-12).log10()
    );
    Ok(())
}
