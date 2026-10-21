//! Clock bridging between the NDI sender clock and the output device clock.
//!
//! Two clocks matter: the sender's (seen through the NDI timecode of each
//! frame) and the output device's (seen through the frames it consumes). Both
//! are compared with the local monotonic clock by [`RateEstimator`]. Arrival
//! jitter only adds delay, so the lower envelope of `local - remote` is close
//! to a straight line; measured on the LAN: 35-44 us residual against 2-3 ms
//! jitter. The slope of that line is the rate difference.
//!
//! [`ClockBridge`] turns both rates into a resampler ratio (feed-forward) and
//! adds a weak correction from the buffer fill, which only holds the latency.
//! The ratio follows the rubato convention for fixed-output resamplers: below
//! 1.0 consumes input faster, above 1.0 slower.

use std::collections::VecDeque;

/// Estimates the rate of a remote clock against the local clock from
/// (local, remote) time pairs, using the lower envelope per time bucket.
#[derive(Debug, Clone)]
pub struct RateEstimator {
    bucket_s: f64,
    max_buckets: usize,
    min_buckets: usize,
    origin: Option<f64>,
    /// Current bucket: index and the pair with the smallest `local - remote`.
    current: Option<(i64, f64, f64)>,
    /// Finished buckets: (remote time since origin, local - remote).
    buckets: VecDeque<(f64, f64)>,
    slope: Option<f64>,
}

impl RateEstimator {
    /// `bucket_s`: bucket length; `max_buckets`: regression window;
    /// `min_buckets`: buckets needed before a slope is reported.
    pub fn new(bucket_s: f64, max_buckets: usize, min_buckets: usize) -> Self {
        Self {
            bucket_s,
            max_buckets,
            min_buckets,
            origin: None,
            current: None,
            buckets: VecDeque::with_capacity(max_buckets),
            slope: None,
        }
    }

    /// Add one observation. Allocation-free after construction.
    pub fn add(&mut self, local_s: f64, remote_s: f64) {
        let origin = *self.origin.get_or_insert(remote_s);
        let r = remote_s - origin;
        let d = local_s - remote_s;
        let idx = (r / self.bucket_s).floor() as i64;
        match self.current {
            Some((i, _, best)) if i == idx => {
                if d < best {
                    self.current = Some((i, r, d));
                }
            }
            Some((_, br, bd)) => {
                if self.buckets.len() == self.max_buckets {
                    self.buckets.pop_front();
                }
                self.buckets.push_back((br, bd));
                self.current = Some((idx, r, d));
                self.refit();
            }
            None => self.current = Some((idx, r, d)),
        }
    }

    fn refit(&mut self) {
        let n = self.buckets.len();
        if n < self.min_buckets {
            return;
        }
        let nf = n as f64;
        let (sx, sy) = self
            .buckets
            .iter()
            .fold((0.0, 0.0), |(a, b), &(x, y)| (a + x, b + y));
        let (mx, my) = (sx / nf, sy / nf);
        let (sxy, sxx) = self.buckets.iter().fold((0.0, 0.0), |(a, b), &(x, y)| {
            (a + (x - mx) * (y - my), b + (x - mx) * (x - mx))
        });
        if sxx > 0.0 {
            self.slope = Some(sxy / sxx);
        }
    }

    /// d(local - remote) / d(remote): positive when the remote clock is slow.
    /// `None` until enough buckets are collected.
    pub fn slope(&self) -> Option<f64> {
        self.slope
    }

    /// Remote clock rate relative to the local clock (remote seconds per local second).
    pub fn rate(&self) -> Option<f64> {
        self.slope.map(|s| 1.0 / (1.0 + s))
    }

    pub fn reset(&mut self) {
        self.origin = None;
        self.current = None;
        self.buckets.clear();
        self.slope = None;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BridgeConfig {
    /// Target buffer fill in frames, until `hold_s` replaces it.
    pub target_frames: f64,
    /// After a start or [`ClockBridge::reset_fill`], the mean fill over this
    /// time, counted from when both rates are known, becomes the target; the
    /// correction is off until then. The
    /// bridge then holds the start latency: with sender frames of 1024, the
    /// fill swings up to one frame below the level it was primed to, and an
    /// absolute target would read that as a constant error. 0 = keep
    /// `target_frames`.
    pub hold_s: f64,
    /// Time constant of the fill smoothing, in seconds.
    pub smoothing_s: f64,
    /// Ratio correction per frame of smoothed fill error.
    pub kp: f64,
    /// Largest allowed deviation of the ratio from 1.0, in ppm.
    pub max_ppm: f64,
}

impl BridgeConfig {
    /// The fill correction is weak on purpose (1 ppm per 100 frames of error):
    /// the feed-forward rates do the tracking, the correction only trims the
    /// latency that remains from estimation errors and the start.
    pub fn for_target(target_frames: f64) -> Self {
        Self {
            target_frames,
            hold_s: 10.0,
            smoothing_s: 10.0,
            kp: 1e-6 / 100.0,
            max_ppm: 500.0,
        }
    }
}

/// Combines the sender rate and the device rate into a resampler ratio.
#[derive(Debug, Clone)]
pub struct ClockBridge {
    cfg: BridgeConfig,
    smoothed_fill: Option<f64>,
    ratio: f64,
    target: f64,
    /// Time-weighted fill sum and time while the target is measured.
    hold: Option<(f64, f64)>,
}

impl ClockBridge {
    pub fn new(cfg: BridgeConfig) -> Self {
        Self {
            cfg,
            smoothed_fill: None,
            ratio: 1.0,
            target: cfg.target_frames,
            hold: (cfg.hold_s > 0.0).then_some((0.0, 0.0)),
        }
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    pub fn smoothed_fill(&self) -> Option<f64> {
        self.smoothed_fill
    }

    /// The fill the correction holds; `None` while it is measured.
    pub fn target(&self) -> Option<f64> {
        self.hold.is_none().then_some(self.target)
    }

    /// Forget the fill history and measure the target again.
    pub fn reset_fill(&mut self) {
        self.smoothed_fill = None;
        if self.cfg.hold_s > 0.0 {
            self.hold = Some((0.0, 0.0));
        }
    }

    /// `sender_rate` and `device_rate`: each clock in its own seconds per local
    /// second (from [`RateEstimator::rate`]); `None` while unknown.
    /// `fill_frames`: input frames buffered now; `dt_s`: time since the last call.
    pub fn update(
        &mut self,
        sender_rate: Option<f64>,
        device_rate: Option<f64>,
        fill_frames: f64,
        dt_s: f64,
    ) -> f64 {
        let s = match self.smoothed_fill {
            None => fill_frames,
            Some(prev) => prev + (dt_s / self.cfg.smoothing_s).min(1.0) * (fill_frames - prev),
        };
        self.smoothed_fill = Some(s);
        // Feed-forward: output frames per local second = fs * device_rate;
        // input frames per local second = fs * sender_rate; input consumed per
        // output frame = 1 / ratio -> ratio = device_rate / sender_rate.
        let ff = match (sender_rate, device_rate) {
            (Some(s), Some(d)) => d / s,
            _ => 1.0,
        };
        let correction = match &mut self.hold {
            // Before the feed-forward works, the fill still drifts.
            Some(_) if sender_rate.is_none() || device_rate.is_none() => 0.0,
            Some((sum, time)) => {
                *sum += fill_frames * dt_s;
                *time += dt_s;
                if *time >= self.cfg.hold_s {
                    self.target = *sum / *time;
                    // Start the smoothing at the mean, not at the first value.
                    self.smoothed_fill = Some(self.target);
                    self.hold = None;
                }
                0.0
            }
            None => self.cfg.kp * (s - self.target),
        };
        let limit = self.cfg.max_ppm * 1e-6;
        let dev = (1.0 - ff + correction).clamp(-limit, limit);
        self.ratio = 1.0 - dev;
        self.ratio
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift, so the tests need no crate.
    struct Rng(u64);
    impl Rng {
        fn next_f64(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        /// One-sided jitter: mostly small, sometimes large (network-like).
        fn jitter(&mut self, max_s: f64) -> f64 {
            let u = self.next_f64();
            max_s * u * u * u
        }
    }

    #[test]
    fn estimator_finds_rate_through_one_sided_jitter() {
        let mut e = RateEstimator::new(2.0, 150, 8);
        let mut rng = Rng(42);
        // Remote clock 25 ppm fast: remote advances 1+25e-6 s per local second.
        let rate = 1.0 + 25e-6;
        for k in 0..(48_000 * 300 / 1024) {
            let local = k as f64 * 1024.0 / 48_000.0;
            let remote = local * rate;
            e.add(local + rng.jitter(0.012), remote);
        }
        let got = e.rate().unwrap();
        assert!(
            (got - rate).abs() * 1e6 < 0.5,
            "rate {} ppm",
            (got - 1.0) * 1e6
        );
    }

    #[derive(Debug)]
    struct SimResult {
        underruns: u32,
        mean_ratio: f64,
        std_ppm: f64,
        fill_min: f64,
        fill_max: f64,
        /// Mean fill over the last 120 s.
        end_fill: f64,
        /// Fill the bridge holds (measured once both rates are known).
        target: Option<f64>,
    }

    /// Sender: 1024-frame frames at fs * (1 + sender_ppm), arriving with
    /// one-sided jitter up to `jitter_ms`. Device: 512-frame callbacks at
    /// fs * (1 + device_ppm), with up to 1 ms scheduling jitter. At the start the
    /// fill is cut to the target, as `Playout` does. Statistics over the last 120 s.
    fn simulate(sender_ppm: f64, device_ppm: f64, jitter_ms: f64, seconds: f64) -> SimResult {
        let fs = 48_000.0;
        let target = 4096.0;
        let mut bridge = ClockBridge::new(BridgeConfig::for_target(target));
        let mut sender_est = RateEstimator::new(2.0, 150, 8);
        let mut device_est = RateEstimator::new(2.0, 150, 8);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let s_rate = 1.0 + sender_ppm * 1e-6;
        let d_rate = 1.0 + device_ppm * 1e-6;
        let send_period = 1024.0 / (fs * s_rate);
        let cb_period = 512.0 / (fs * d_rate);
        let n = (seconds / send_period) as usize + 2;
        // (arrival local time, sender time of the frame)
        let mut frames: Vec<(f64, f64)> = (0..n)
            .map(|i| {
                (
                    i as f64 * send_period + rng.jitter(jitter_ms / 1000.0),
                    i as f64 * 1024.0 / fs,
                )
            })
            .collect();
        frames.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let (mut fill, mut next, mut t) = (0.0f64, 0usize, 0.0f64);
        while fill < target {
            fill += 1024.0;
            sender_est.add(frames[next].0, frames[next].1);
            t = frames[next].0;
            next += 1;
        }
        fill = target;
        bridge.reset_fill();
        let start = t;
        let mut end_sum = 0.0f64;
        let (mut underruns, mut fill_min, mut fill_max) = (0u32, f64::MAX, f64::MIN);
        let mut ratios = Vec::new();
        let mut device_frames = 0.0f64;
        let mut cb = 0u64;
        while t < seconds {
            cb += 1;
            t = start + cb as f64 * cb_period;
            while next < frames.len() && frames[next].0 <= t {
                fill += 1024.0;
                sender_est.add(frames[next].0, frames[next].1);
                next += 1;
            }
            device_est.add(t + rng.jitter(0.001), device_frames / fs);
            device_frames += 512.0;
            let ratio = bridge.update(sender_est.rate(), device_est.rate(), fill, cb_period);
            // Fill as the bridge sees it, before this callback takes its frames.
            if t > seconds - 120.0 {
                end_sum += fill;
            }
            let need = 512.0 / ratio;
            if fill < need {
                underruns += 1;
                fill = 0.0;
                bridge.reset_fill();
            } else {
                fill -= need;
            }
            if t > seconds - 120.0 {
                ratios.push(ratio);
                fill_min = fill_min.min(fill);
                fill_max = fill_max.max(fill);
            }
        }
        let k = ratios.len() as f64;
        let mean = ratios.iter().sum::<f64>() / k;
        let var = ratios.iter().map(|r| (r - mean) * (r - mean)).sum::<f64>() / k;
        SimResult {
            underruns,
            mean_ratio: mean,
            std_ppm: var.sqrt() * 1e6,
            fill_min,
            fill_max,
            end_fill: end_sum / k,
            target: bridge.target(),
        }
    }

    fn check(sender_ppm: f64, device_ppm: f64) {
        let r = simulate(sender_ppm, device_ppm, 12.0, 900.0);
        eprintln!("sender {sender_ppm} ppm, device {device_ppm} ppm: {r:?}");
        assert_eq!(r.underruns, 0);
        let expected = (1.0 + device_ppm * 1e-6) / (1.0 + sender_ppm * 1e-6);
        assert!(
            (r.mean_ratio - expected).abs() * 1e6 < 1.0,
            "mean ratio off by {} ppm",
            (r.mean_ratio - expected) * 1e6
        );
        assert!(r.std_ppm < 1.0, "ratio std {} ppm", r.std_ppm);
        assert!(
            r.fill_min > 1024.0 && r.fill_max < 8192.0,
            "fill {}..{}",
            r.fill_min,
            r.fill_max
        );
        // The latency from when tracking starts is held: no creep towards an
        // absolute target.
        let target = r.target.expect("target measured");
        assert!(
            (r.end_fill - target).abs() < 50.0,
            "mean fill {:.0}, held target {target:.0}",
            r.end_fill
        );
    }

    #[test]
    fn small_clock_offsets() {
        check(4.0, -3.0);
    }

    #[test]
    fn large_clock_offsets() {
        check(150.0, -150.0);
    }

    #[test]
    fn ratio_is_limited() {
        let mut b = ClockBridge::new(BridgeConfig::for_target(1000.0));
        for _ in 0..1000 {
            b.update(Some(1.0), Some(1.01), 1000.0, 0.01);
        }
        assert!((b.ratio() - 1.0).abs() * 1e6 <= 500.0 + 1e-6);
    }
}
