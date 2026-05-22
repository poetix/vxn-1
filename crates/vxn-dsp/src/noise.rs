//! Noise sources. Copied from `patches-dsp::noise`.

/// Xorshift64 PRNG mapped to `[-1, 1]`. `state` must be non-zero (zero is a
/// stuck fixed point); seed with `instance_id + 1`.
#[inline]
pub fn xorshift64(state: &mut u64) -> f32 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state as i64 as f32) / (i64::MAX as f32)
}

/// Selectable noise colour for the mixer's noise source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoiseColor {
    White,
    Pink,
    Brown,
}

impl NoiseColor {
    pub const ALL: [NoiseColor; 3] = [NoiseColor::White, NoiseColor::Pink, NoiseColor::Brown];

    pub fn label(self) -> &'static str {
        match self {
            NoiseColor::White => "White",
            NoiseColor::Pink => "Pink",
            NoiseColor::Brown => "Brown",
        }
    }
}

/// 3-pole IIR pink-shaping filter (Voss–McCartney / Kellett). −3 dB/oct.
#[derive(Clone)]
pub struct PinkFilter {
    b0: f32,
    b1: f32,
    b2: f32,
}

impl PinkFilter {
    pub fn new() -> Self {
        Self { b0: 0.0, b1: 0.0, b2: 0.0 }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    #[inline]
    pub fn process(&mut self, white: f32) -> f32 {
        self.b0 = 0.99765 * self.b0 + white * 0.0990460;
        self.b1 = 0.96300 * self.b1 + white * 0.2965164;
        self.b2 = 0.57000 * self.b2 + white * 1.0526913;
        (self.b0 + self.b1 + self.b2 + white * 0.1848) * 0.11
    }
}

impl Default for PinkFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Leaky integrator for brown noise (−6 dB/oct), clamped to `[-1, 1]`.
#[derive(Clone)]
pub struct BrownFilter {
    pub state: f32,
}

impl BrownFilter {
    pub fn new() -> Self {
        Self { state: 0.0 }
    }

    pub fn reset(&mut self) {
        self.state = 0.0;
    }

    #[inline]
    pub fn process(&mut self, input: f32) -> f32 {
        self.state += input * 0.02;
        self.state = self.state.clamp(-1.0, 1.0);
        self.state
    }
}

impl Default for BrownFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// A complete noise generator: PRNG + colour shapers. One per voice (or one
/// shared mono source — VXN1 uses one per voice for decorrelation).
#[derive(Clone)]
pub struct NoiseSource {
    state: u64,
    pink: PinkFilter,
    brown: BrownFilter,
}

impl NoiseSource {
    pub fn new(seed: u64) -> Self {
        Self { state: seed | 1, pink: PinkFilter::new(), brown: BrownFilter::new() }
    }

    pub fn reset(&mut self) {
        self.pink.reset();
        self.brown.reset();
    }

    #[inline]
    pub fn next(&mut self, color: NoiseColor) -> f32 {
        let white = xorshift64(&mut self.state);
        match color {
            NoiseColor::White => white,
            NoiseColor::Pink => self.pink.process(white),
            NoiseColor::Brown => self.brown.process(white),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_variance_reasonable() {
        let mut s = 42u64;
        let n = 65536;
        let xs: Vec<f32> = (0..n).map(|_| xorshift64(&mut s)).collect();
        let mean = xs.iter().sum::<f32>() / n as f32;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n as f32;
        assert!((0.2..=0.4).contains(&var), "variance {var}");
    }

    #[test]
    fn colors_finite_and_bounded() {
        let mut ns = NoiseSource::new(7);
        for c in NoiseColor::ALL {
            for _ in 0..10_000 {
                let v = ns.next(c);
                assert!(v.is_finite() && v.abs() <= 1.5, "{c:?} {v}");
            }
        }
    }
}
