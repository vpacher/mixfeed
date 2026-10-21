//! mfplay: receive NDI audio and play it out with clock bridging (test tool).
//!
//! Usage: mfplay <source name part> <seconds> <outdir> <output device name part> [sinc|slip] [target_ms] [--audible]
//!
//! The output device only provides the clock: without `--audible` it gets
//! silence. What the playout produces is written to <outdir>/mfplay.wav, and
//! once per second the state goes to <outdir>/mfplay_stats.csv.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use mixfeed_core::clock::RateEstimator;
use mixfeed_core::playout::{Playout, ResamplerKind, SharedRate};
use mixfeed_ndi::{Capture, Ndi};

const FS: u32 = 48_000;
const CHANNELS: usize = 2;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        bail!(
            "usage: mfplay <source> <seconds> <outdir> <device> [sinc|slip] [target_ms] [--audible]"
        );
    }
    let (name_part, out, dev_part) = (args[1].clone(), PathBuf::from(&args[3]), args[4].clone());
    let seconds: f64 = args[2].parse().context("seconds")?;
    let kind = match args.get(5).map(String::as_str) {
        None | Some("sinc") => ResamplerKind::Sinc,
        Some("slip") => ResamplerKind::Slip,
        Some(k) => bail!("unknown resampler {k}"),
    };
    let target_ms: f64 = args
        .get(6)
        .map_or(Ok(80.0), |s| s.parse())
        .context("target_ms")?;
    let audible = args.iter().any(|a| a == "--audible");
    fs::create_dir_all(&out)?;
    let t0 = Instant::now();

    // NDI receive thread -> ring.
    let ndi = Ndi::load()?;
    let source = ndi.find_source(&name_part, Duration::from_secs(15))?;
    println!(
        "NDI {} | source {} {}",
        ndi.version(),
        source.name,
        source.url
    );
    let mut rx = ndi.audio_receiver(&source, "mixfeed mfplay")?;
    let (mut ring_tx, ring_rx) = rtrb::RingBuffer::<f32>::new(FS as usize * CHANNELS * 2);
    let sender_rate = Arc::new(SharedRate::new());
    let overflows = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let rx_thread = {
        let (sender_rate, overflows, stop) = (sender_rate.clone(), overflows.clone(), stop.clone());
        std::thread::spawn(move || -> anyhow::Result<()> {
            let mut est = RateEstimator::new(2.0, 150, 8);
            let mut inter = Vec::with_capacity(4096 * CHANNELS);
            while !stop.load(Ordering::Relaxed) {
                let Capture::Audio(f) = rx.capture(100)? else {
                    continue;
                };
                let local = t0.elapsed().as_secs_f64();
                if f.sample_rate != FS || f.planes.len() != CHANNELS {
                    eprintln!("skip frame: {} Hz, {} ch", f.sample_rate, f.planes.len());
                    continue;
                }
                est.add(local, f.timecode as f64 / 1e7);
                sender_rate.store(est.rate());
                inter.clear();
                for i in 0..f.samples() {
                    for p in &f.planes {
                        inter.push(p[i]);
                    }
                }
                if ring_tx.push_entire_slice(&inter).is_err() {
                    overflows.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        })
    };

    // Output device = clock.
    let host = cpal::default_host();
    let device = host
        .output_devices()?
        .find(|d| {
            d.description()
                .map(|x| x.to_string().contains(&dev_part))
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("no output device containing {dev_part:?}"))?;
    println!("device: {}", device.description()?);
    let config = cpal::StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: FS,
        buffer_size: cpal::BufferSize::Default,
    };
    let target_frames = (target_ms / 1000.0 * FS as f64) as usize;
    let mut playout = Playout::new(
        ring_rx,
        CHANNELS,
        FS as f64,
        FS as f64,
        kind,
        64,
        target_frames,
        sender_rate.clone(),
    )?;
    let stats = playout.stats();
    let (mut tap_tx, mut tap_rx) = rtrb::RingBuffer::<f32>::new(FS as usize * CHANNELS * 4);
    let tap_overflows = Arc::new(AtomicU32::new(0));
    let stream = {
        let tap_overflows = tap_overflows.clone();
        device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                playout.render(data, t0.elapsed().as_secs_f64());
                if tap_tx.push_entire_slice(data).is_err() {
                    tap_overflows.fetch_add(1, Ordering::Relaxed);
                }
                if !audible {
                    data.fill(0.0);
                }
            },
            |e| eprintln!("stream error: {e}"),
            None,
        )?
    };
    stream.play()?;

    // Main thread: tap -> WAV, stats -> CSV.
    let spec = hound::WavSpec {
        channels: CHANNELS as u16,
        sample_rate: FS,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut wav = hound::WavWriter::create(out.join("mfplay.wav"), spec)?;
    let mut csv = BufWriter::new(File::create(out.join("mfplay_stats.csv"))?);
    writeln!(
        csv,
        "t_s,fill_frames,ratio_ppm,sender_ppm,device_ppm,underruns,resyncs,overflows,tap_overflows"
    )?;
    let ppm = |v: Option<f64>| v.map_or(String::new(), |r| format!("{:.3}", (r - 1.0) * 1e6));
    let mut next_stat = 1.0;
    while t0.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(20));
        let n = tap_rx.slots();
        if n > 0 {
            let rc = tap_rx.read_chunk(n)?;
            let (a, b) = rc.as_slices();
            for s in a.iter().chain(b.iter()) {
                wav.write_sample(*s)?;
            }
            rc.commit_all();
        }
        let t = t0.elapsed().as_secs_f64();
        if t >= next_stat {
            next_stat += 1.0;
            writeln!(
                csv,
                "{t:.2},{},{},{},{},{},{},{},{}",
                stats
                    .fill_frames
                    .load()
                    .map_or(String::new(), |v| format!("{v:.0}")),
                ppm(stats.ratio.load()),
                ppm(sender_rate.load()),
                ppm(stats.device_rate.load()),
                stats.underruns.load(Ordering::Relaxed),
                stats.resyncs.load(Ordering::Relaxed),
                overflows.load(Ordering::Relaxed),
                tap_overflows.load(Ordering::Relaxed),
            )?;
        }
    }
    drop(stream);
    stop.store(true, Ordering::Relaxed);
    rx_thread
        .join()
        .map_err(|_| anyhow!("receive thread panicked"))??;
    wav.finalize()?;
    csv.flush()?;
    println!(
        "done: underruns {}, resyncs {}, ring overflows {}, tap overflows {}, last ratio {} ppm, sender {} ppm, device {} ppm",
        stats.underruns.load(Ordering::Relaxed),
        stats.resyncs.load(Ordering::Relaxed),
        overflows.load(Ordering::Relaxed),
        tap_overflows.load(Ordering::Relaxed),
        ppm(stats.ratio.load()),
        ppm(sender_rate.load()),
        ppm(stats.device_rate.load()),
    );
    Ok(())
}
