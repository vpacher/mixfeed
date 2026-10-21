//! Playout: from the receive ring buffer to the output device callback.
//!
//! The receive thread writes interleaved f32 frames into an `rtrb` ring and
//! publishes the sender rate through [`SharedRate`]. The device callback calls
//! [`Playout::render`]: it measures the device clock, updates the ratio from
//! [`ClockBridge`], and resamples fixed-size output chunks from the ring.
//! Nothing in `render` allocates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType, Slip,
    WindowFunction,
};

use crate::clock::{BridgeConfig, ClockBridge, RateEstimator};

/// An `Option<f64>` shared between threads without locks (NaN = None).
#[derive(Debug, Default)]
pub struct SharedRate(AtomicU64);

impl SharedRate {
    pub fn new() -> Self {
        Self(AtomicU64::new(f64::NAN.to_bits()))
    }
    pub fn store(&self, v: Option<f64>) {
        self.0
            .store(v.unwrap_or(f64::NAN).to_bits(), Ordering::Relaxed);
    }
    pub fn load(&self) -> Option<f64> {
        let v = f64::from_bits(self.0.load(Ordering::Relaxed));
        (!v.is_nan()).then_some(v)
    }
}

/// Counters and last values, readable from another thread.
#[derive(Debug, Default)]
pub struct Stats {
    pub ratio: SharedRate,
    pub device_rate: SharedRate,
    pub fill_frames: SharedRate,
    /// Fill after the bridge's smoothing, and the fill it holds (input frames).
    pub fill_smoothed: SharedRate,
    pub fill_target: SharedRate,
    pub underruns: AtomicU32,
    /// Times the buffer ran far over the target and was cut back.
    pub resyncs: AtomicU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResamplerKind {
    /// Band-limited sinc interpolation; changes every sample a little.
    Sinc,
    /// Pass-through with an occasional crossfaded frame slip; bit-exact between slips.
    Slip,
}

pub struct Playout {
    channels: usize,
    sample_rate: f64,
    ring: rtrb::Consumer<f32>,
    resampler: Box<dyn Resampler<f32>>,
    bridge: ClockBridge,
    target_frames: usize,
    sender_rate: Arc<SharedRate>,
    device_est: RateEstimator,
    device_frames: u64,
    last_local: Option<f64>,
    running: bool,
    staging: Vec<Vec<f32>>,
    chunk: Vec<Vec<f32>>,
    chunk_len: usize,
    chunk_pos: usize,
    stats: Arc<Stats>,
}

impl Playout {
    /// `sample_rate`: device (output) rate; `in_rate`: rate of the frames in
    /// the ring. `chunk`: output frames per resampler call. `target_frames`:
    /// buffer fill (input frames) to start with and to hold. `Slip` needs
    /// equal rates.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ring: rtrb::Consumer<f32>,
        channels: usize,
        sample_rate: f64,
        in_rate: f64,
        kind: ResamplerKind,
        chunk: usize,
        target_frames: usize,
        sender_rate: Arc<SharedRate>,
    ) -> Result<Self, rubato::ResamplerConstructionError> {
        let base_ratio = sample_rate / in_rate;
        let resampler: Box<dyn Resampler<f32>> = match kind {
            ResamplerKind::Sinc => {
                let params = SincInterpolationParameters::new(256, WindowFunction::Blackman2)
                    .oversampling_factor(256)
                    .interpolation(SincInterpolationType::Cubic);
                Box::new(Async::<f32>::new_sinc(
                    base_ratio,
                    1.01,
                    &params,
                    chunk,
                    channels,
                    FixedAsync::Output,
                )?)
            }
            ResamplerKind::Slip if base_ratio != 1.0 => {
                return Err(rubato::ResamplerConstructionError::InvalidRatio(base_ratio));
            }
            ResamplerKind::Slip => Box::new(Slip::<f32>::new(chunk, channels, FixedAsync::Output)?),
        };
        let max_in = resampler.input_frames_max();
        Ok(Self {
            channels,
            sample_rate,
            ring,
            bridge: ClockBridge::new(BridgeConfig::for_target(target_frames as f64)),
            target_frames,
            sender_rate,
            device_est: RateEstimator::new(2.0, 150, 8),
            device_frames: 0,
            last_local: None,
            running: false,
            staging: vec![vec![0.0; max_in]; channels],
            chunk: vec![vec![0.0; chunk]; channels],
            chunk_len: 0,
            chunk_pos: 0,
            resampler,
            stats: Arc::new(Stats::default()),
        })
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    fn fill_frames(&self) -> usize {
        self.ring.slots() / self.channels
    }

    /// Fill `out` (interleaved, `out.len() / channels` frames). `local_s`: the
    /// local monotonic time of this callback.
    pub fn render(&mut self, out: &mut [f32], local_s: f64) {
        let frames = out.len() / self.channels;
        self.device_est
            .add(local_s, self.device_frames as f64 / self.sample_rate);
        self.device_frames += frames as u64;
        let dt = self.last_local.map_or(0.0, |p| (local_s - p).max(0.0));
        self.last_local = Some(local_s);

        if !self.running {
            if self.fill_frames() >= self.target_frames {
                self.running = true;
                self.bridge.reset_fill();
                // A receiver delivers its backlog in one burst at connect; drop
                // what is above the target once, instead of letting the weak
                // latency correction work it off for minutes at a wrong ratio.
                self.discard_excess(0);
            } else {
                out.fill(0.0);
                return;
            }
        } else if self.fill_frames() > 2 * self.target_frames + 4096 {
            // Burst after a network stall: same treatment, with some margin so
            // normal jitter never triggers it.
            self.discard_excess(0);
            self.bridge.reset_fill();
            self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
        }

        let device_rate = self.device_est.rate();
        let ratio = self.bridge.update(
            self.sender_rate.load(),
            device_rate,
            self.fill_frames() as f64,
            dt,
        );
        if let Some(adj) = self.resampler.as_adjustable() {
            // Ramp over the next chunk so the ratio never jumps.
            let _ = adj.set_resample_ratio_relative(ratio, true);
        }
        self.stats.ratio.store(Some(ratio));
        self.stats.device_rate.store(device_rate);
        self.stats
            .fill_frames
            .store(Some(self.fill_frames() as f64));
        self.stats.fill_smoothed.store(self.bridge.smoothed_fill());
        self.stats.fill_target.store(self.bridge.target());

        let mut written = 0;
        while written < frames {
            if self.chunk_pos == self.chunk_len && !self.next_chunk() {
                // Underrun: silence for the rest, prime again.
                out[written * self.channels..].fill(0.0);
                self.running = false;
                self.stats.underruns.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let n = (self.chunk_len - self.chunk_pos).min(frames - written);
            for i in 0..n {
                for c in 0..self.channels {
                    out[(written + i) * self.channels + c] = self.chunk[c][self.chunk_pos + i];
                }
            }
            self.chunk_pos += n;
            written += n;
        }
    }

    /// Drop whole frames from the ring until the fill is `target + margin`.
    fn discard_excess(&mut self, margin: usize) {
        let excess = self
            .fill_frames()
            .saturating_sub(self.target_frames + margin);
        if excess > 0
            && let Ok(rc) = self.ring.read_chunk(excess * self.channels)
        {
            rc.commit_all();
        }
    }

    /// Resample one output chunk from the ring. False if the ring has too little.
    fn next_chunk(&mut self) -> bool {
        let need = self.resampler.input_frames_next();
        let Ok(rc) = self.ring.read_chunk(need * self.channels) else {
            return false;
        };
        let (a, b) = rc.as_slices();
        for (k, s) in a.iter().chain(b.iter()).enumerate() {
            self.staging[k % self.channels][k / self.channels] = *s;
        }
        rc.commit_all();
        let input =
            SequentialSliceOfVecs::new(&self.staging, self.channels, need).expect("staging sized");
        let cap = self.chunk[0].len();
        let mut output = SequentialSliceOfVecs::new_mut(&mut self.chunk, self.channels, cap)
            .expect("chunk sized");
        match self
            .resampler
            .process_into_buffer(&input, &mut output, None)
        {
            Ok((_, produced)) => {
                self.chunk_len = produced;
                self.chunk_pos = 0;
                produced > 0
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(kind: ResamplerKind) -> (Vec<f32>, Vec<f32>, u32) {
        let (mut tx, rx) = rtrb::RingBuffer::<f32>::new(48_000 * 2);
        let rate = Arc::new(SharedRate::new());
        let mut p = Playout::new(rx, 2, 48_000.0, 48_000.0, kind, 64, 2048, rate).unwrap();
        // 2 s of a 997 Hz sine (more than consumed: the start drops the burst excess),
        // pushed in 1024-frame NDI-like frames, pulled in 512-frame callbacks for 1 s.
        let sine: Vec<f32> = (0..96_000)
            .flat_map(|i| {
                let v =
                    (2.0 * std::f64::consts::PI * 997.0 * i as f64 / 48_000.0).sin() as f32 * 0.25;
                [v, v]
            })
            .collect();
        let mut out_all = Vec::new();
        let mut buf = vec![0.0f32; 512 * 2];
        let mut pushed = 0;
        let mut t = 0.0;
        for _ in 0..(48_000 / 512) {
            while pushed < sine.len() && (pushed / 2) < ((t + 0.05) * 48_000.0) as usize {
                let end = (pushed + 2048).min(sine.len());
                tx.push_entire_slice(&sine[pushed..end]).unwrap();
                pushed = end;
            }
            p.render(&mut buf, t);
            out_all.extend_from_slice(&buf);
            t += 512.0 / 48_000.0;
        }
        (sine, out_all, p.stats().underruns.load(Ordering::Relaxed))
    }

    #[test]
    fn slip_output_is_a_bit_exact_section_of_the_input() {
        let (input, out, underruns) = run(ResamplerKind::Slip);
        assert_eq!(underruns, 0);
        // Skip the priming silence, then find where the output starts in the input.
        let first = out.iter().position(|v| *v != 0.0).expect("signal");
        let probe = &out[first..first + 16];
        let at = input
            .windows(16)
            .position(|w| w == probe)
            .expect("output start found in input");
        // At a ratio of 1.0 (no rate estimates yet, fill at target) nothing slips:
        // every following sample is copied unchanged.
        let n = (out.len() - first).min(input.len() - at);
        assert!(n > 40_000, "compared {n} samples");
        assert_eq!(&out[first..first + n], &input[at..at + n]);
    }

    #[test]
    fn sinc_converts_44k1_input_to_device_rate() {
        let (mut tx, rx) = rtrb::RingBuffer::<f32>::new(48_000 * 2);
        let rate = Arc::new(SharedRate::new());
        let mut p =
            Playout::new(rx, 2, 48_000.0, 44_100.0, ResamplerKind::Sinc, 64, 2048, rate).unwrap();
        // 1 s of 1 kHz at 44.1 kHz in, 0.5 s out at 48 kHz.
        let sine: Vec<f32> = (0..44_100)
            .flat_map(|i| {
                let v = (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44_100.0).sin() as f32;
                [v * 0.5, v * 0.5]
            })
            .collect();
        // Fed at real-time pace, 50 ms ahead (the start drops a larger backlog).
        let mut out = vec![0.0f32; 24_000 * 2];
        let mut pushed = 0;
        for (k, c) in out.chunks_mut(512 * 2).enumerate() {
            let t = k as f64 * 512.0 / 48_000.0;
            while pushed < sine.len() && (pushed / 2) < ((t + 0.05) * 44_100.0) as usize {
                let end = (pushed + 2048).min(sine.len());
                tx.push_entire_slice(&sine[pushed..end]).unwrap();
                pushed = end;
            }
            p.render(c, t);
        }
        assert_eq!(p.stats().underruns.load(Ordering::Relaxed), 0);
        // Rising zero crossings of the left channel in the second half: 1 kHz at 48 kHz.
        let left: Vec<f32> = out.iter().step_by(2).skip(12_000).copied().collect();
        let ups = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((249..=251).contains(&ups), "{ups} cycles in 0.25 s");
    }

    #[test]
    fn sinc_runs_without_underrun() {
        let (_, out, underruns) = run(ResamplerKind::Sinc);
        assert_eq!(underruns, 0);
        assert!(out.iter().any(|v| v.abs() > 0.2));
    }
}
