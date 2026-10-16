//! Algorithmic reverb: pre-delay → input diffusers → 8-line feedback delay network.
//!
//! The FDN mixes its lines through a Householder matrix (lossless, O(N)), each line has a
//! gain giving the wanted decay time and a one-pole low-pass so highs die faster, like a
//! real room. Left/right outputs tap the lines with orthogonal sign patterns for width.
//! Everything is allocated in `new`; `process` never allocates.

const LINES: usize = 8;
/// Delay line lengths (ms), spread so their echoes don't line up.
const LINE_MS: [f32; LINES] = [29.7, 37.1, 41.1, 43.7, 53.3, 59.9, 67.7, 79.3];
/// Series all-pass diffusers smearing the input before the tank.
const DIFFUSER_MS: [f32; 4] = [4.7, 3.6, 12.7, 9.3];
const DIFFUSION: f32 = 0.7;
const PREDELAY_MS: f32 = 20.0;
/// Rows of an 8×8 Hadamard matrix: mutually orthogonal, so input, left and right decorrelate.
const IN_SIGNS: [f32; LINES] = [1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
const LEFT_SIGNS: [f32; LINES] = [1.0, 1.0, -1.0, -1.0, 1.0, 1.0, -1.0, -1.0];
const RIGHT_SIGNS: [f32; LINES] = [1.0, -1.0, -1.0, 1.0, 1.0, -1.0, -1.0, 1.0];
/// Scales the tank output so that 100 % wet carries about as much energy as the dry signal.
const OUTPUT_GAIN: f32 = 0.25;
/// Keeps feedback paths out of the slow denormal range as the tail dies.
const ANTI_DENORMAL: f32 = 1e-20;

pub const DEFAULT_DECAY_S: f32 = 2.2;
/// One-pole coefficient in the loop: higher = darker tail.
pub const DEFAULT_DAMPING: f32 = 0.3;

struct Delay {
    buf: Vec<f32>,
    pos: usize,
}

impl Delay {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)], pos: 0 }
    }

    /// Oldest sample, i.e. the input from `len` samples ago.
    #[inline]
    fn read(&self) -> f32 {
        self.buf[self.pos]
    }

    #[inline]
    fn write(&mut self, x: f32) {
        self.buf[self.pos] = x;
        self.pos += 1;
        if self.pos == self.buf.len() {
            self.pos = 0;
        }
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
    }
}

pub struct Reverb {
    predelay: Delay,
    diffusers: [Delay; 4],
    lines: [Delay; LINES],
    gains: [f32; LINES],
    damp_state: [f32; LINES],
    damping: f32,
    sr: f32,
}

impl Reverb {
    pub fn new(sr: f32) -> Self {
        let samples = |ms: f32| next_prime((ms * sr / 1000.0).round() as usize);
        let mut r = Self {
            predelay: Delay::new((PREDELAY_MS * sr / 1000.0) as usize),
            diffusers: DIFFUSER_MS.map(|ms| Delay::new(samples(ms))),
            lines: LINE_MS.map(|ms| Delay::new(samples(ms))),
            gains: [0.0; LINES],
            damp_state: [0.0; LINES],
            damping: DEFAULT_DAMPING,
            sr,
        };
        r.set_decay(DEFAULT_DECAY_S);
        r
    }

    /// Time for the tail to fall by 60 dB (at low frequencies).
    pub fn set_decay(&mut self, seconds: f32) {
        for (g, line) in self.gains.iter_mut().zip(&self.lines) {
            *g = 10f32.powf(-3.0 * line.buf.len() as f32 / (seconds.max(0.05) * self.sr));
        }
    }

    pub fn clear(&mut self) {
        self.predelay.clear();
        for d in self.diffusers.iter_mut().chain(self.lines.iter_mut()) {
            d.clear();
        }
        self.damp_state = [0.0; LINES];
    }

    /// One mono sample in, one stereo wet sample out.
    #[inline]
    pub fn process(&mut self, input: f32) -> [f32; 2] {
        let mut x = self.predelay.read();
        self.predelay.write(input);
        for d in &mut self.diffusers {
            // Schroeder all-pass: w = x + g·w[n-M], y = w[n-M] - g·w.
            let delayed = d.read();
            let w = x + DIFFUSION * delayed;
            d.write(w);
            x = delayed - DIFFUSION * w;
        }

        let mut out = [0.0f32; LINES];
        let mut fed = [0.0f32; LINES];
        for i in 0..LINES {
            out[i] = self.lines[i].read();
            self.damp_state[i] = out[i] + self.damping * (self.damp_state[i] - out[i]);
            fed[i] = self.damp_state[i] * self.gains[i];
        }
        // Householder reflection: v - (2/N)·Σv. Energy preserving, so the gains alone set decay.
        let mix = fed.iter().sum::<f32>() * (2.0 / LINES as f32);
        for i in 0..LINES {
            let v = fed[i] - mix + x * IN_SIGNS[i] + ANTI_DENORMAL;
            self.lines[i].write(v);
        }

        let (mut l, mut r) = (0.0, 0.0);
        for i in 0..LINES {
            l += out[i] * LEFT_SIGNS[i];
            r += out[i] * RIGHT_SIGNS[i];
        }
        [l * OUTPUT_GAIN, r * OUTPUT_GAIN]
    }
}

fn next_prime(mut n: usize) -> usize {
    let is_prime = |n: usize| n >= 2 && (2..).take_while(|d| d * d <= n).all(|d| n % d != 0);
    while !is_prime(n) {
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn impulse_tail_decays_at_the_set_rate() {
        let mut r = Reverb::new(SR);
        let n = (SR * 3.0) as usize;
        let mut left = Vec::with_capacity(n);
        for i in 0..n {
            left.push(r.process(if i == 0 { 1.0 } else { 0.0 })[0]);
        }
        let win = (SR * 0.1) as usize;
        let at = |s: f32| rms(&left[(s * SR) as usize..(s * SR) as usize + win]);
        let drop_db = 20.0 * (at(1.5) / at(0.5)).log10();
        // Expect about -60 dB per 2.2 s → -27 dB per second; damping makes it a bit faster.
        assert!(drop_db < -22.0 && drop_db > -45.0, "1 s of tail fell {drop_db} dB");
        assert!(left.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn stays_stable_and_decorrelated_on_noise() {
        let mut r = Reverb::new(SR);
        let mut seed = 1u32;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let n = (SR * 4.0) as usize;
        let (mut l, mut rr, mut dry) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..n {
            let x = noise() * 0.5;
            let [a, b] = r.process(x);
            dry.push(x);
            l.push(a);
            rr.push(b);
        }
        let tail = n / 2..n;
        let (dl, wl, wr) = (rms(&dry[tail.clone()]), rms(&l[tail.clone()]), rms(&rr[tail.clone()]));
        let ratio_db = 20.0 * (wl / dl).log10();
        assert!(ratio_db.abs() < 2.0, "wet at 100% vs dry: {ratio_db} dB");
        let corr: f32 = l[tail.clone()].iter().zip(&rr[tail]).map(|(a, b)| a * b).sum::<f32>()
            / ((wl * wr) * (n / 2) as f32);
        assert!(corr.abs() < 0.3, "left/right correlation {corr}");
    }

    #[test]
    fn clear_silences_the_tail() {
        let mut r = Reverb::new(SR);
        for _ in 0..4800 {
            r.process(0.5);
        }
        r.clear();
        assert!((0..48_000).all(|_| r.process(0.0)[0].abs() < 1e-12));
    }
}
