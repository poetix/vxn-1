//! ADSR envelope state machine. Copied from `patches-dsp::adsr` (DADSR with
//! linear/exponential segment shapes), trimmed to what VXN1 v1 uses.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdsrStage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// Segment shape. `Linear` is a constant-slope ramp; `Exponential` is an
/// RC-style asymptotic approach with analog-style 1.2× attack overshoot
/// (output clamped at 1.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdsrShape {
    #[default]
    Linear,
    Exponential,
}

const EXP_ATTACK_TARGET: f32 = 1.2;
const EXP_N_TAU: f32 = 5.0;
const EXP_SNAP_EPS: f32 = 1.0e-4;

fn exp_k(secs: f32, sample_rate: f32) -> f32 {
    let samples = (secs * sample_rate).max(1.0);
    (1.0 - (-EXP_N_TAU / samples).exp()).clamp(0.0, 1.0)
}

/// Core ADSR. One instance per modulation destination per voice.
#[derive(Clone)]
pub struct AdsrCore {
    pub stage: AdsrStage,
    pub level: f32,
    shape: AdsrShape,
    attack_inc: f32,
    decay_inc: f32,
    sustain: f32,
    release_secs: f32,
    release_inc: f32,
    attack_k: f32,
    decay_k: f32,
    release_k: f32,
    sample_rate: f32,
}

impl AdsrCore {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            stage: AdsrStage::Idle,
            level: 0.0,
            shape: AdsrShape::Linear,
            attack_inc: 0.0,
            decay_inc: 0.0,
            sustain: 0.0,
            release_secs: 0.0,
            release_inc: 0.0,
            attack_k: 0.0,
            decay_k: 0.0,
            release_k: 0.0,
            sample_rate,
        }
    }

    pub fn set_params(&mut self, attack_secs: f32, decay_secs: f32, sustain: f32, release_secs: f32) {
        let a = attack_secs.max(1.0e-4);
        let d = decay_secs.max(1.0e-4);
        self.attack_inc = 1.0 / (a * self.sample_rate);
        self.sustain = sustain;
        self.decay_inc = (1.0 - sustain) / (d * self.sample_rate);
        self.release_secs = release_secs.max(1.0e-4);
        self.attack_k = exp_k(a, self.sample_rate);
        self.decay_k = exp_k(d, self.sample_rate);
    }

    pub fn set_shape(&mut self, shape: AdsrShape) {
        self.shape = shape;
    }

    pub fn reset(&mut self) {
        self.stage = AdsrStage::Idle;
        self.level = 0.0;
    }

    /// True once the envelope has fully released (safe to free the voice).
    #[inline]
    pub fn is_idle(&self) -> bool {
        self.stage == AdsrStage::Idle
    }

    fn enter_release(&mut self) {
        match self.shape {
            AdsrShape::Linear => {
                self.release_inc = self.level / (self.release_secs * self.sample_rate);
                self.level -= self.release_inc;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.stage = AdsrStage::Idle;
                } else {
                    self.stage = AdsrStage::Release;
                }
            }
            AdsrShape::Exponential => {
                self.release_k = exp_k(self.release_secs, self.sample_rate);
                if self.level <= EXP_SNAP_EPS {
                    self.level = 0.0;
                    self.stage = AdsrStage::Idle;
                } else {
                    self.stage = AdsrStage::Release;
                }
            }
        }
    }

    /// One sample. `triggered` fires on the rising-edge sample; `gate_high`
    /// stays true while the note is held. Returns level clamped to `[0, 1]`.
    #[inline]
    pub fn tick(&mut self, triggered: bool, gate_high: bool) -> f32 {
        if triggered {
            self.stage = AdsrStage::Attack;
        }
        match self.shape {
            AdsrShape::Linear => self.tick_linear(gate_high),
            AdsrShape::Exponential => self.tick_exponential(gate_high),
        }
        self.level.clamp(0.0, 1.0)
    }

    fn tick_linear(&mut self, gate_high: bool) {
        match self.stage {
            AdsrStage::Idle => {}
            AdsrStage::Attack => {
                if !gate_high {
                    self.enter_release();
                } else {
                    self.level += self.attack_inc;
                    if self.level >= 1.0 {
                        self.level = 1.0;
                        self.stage = AdsrStage::Decay;
                    }
                }
            }
            AdsrStage::Decay => {
                if !gate_high {
                    self.enter_release();
                } else {
                    self.level -= self.decay_inc;
                    if self.level <= self.sustain {
                        self.level = self.sustain;
                        self.stage = AdsrStage::Sustain;
                    }
                }
            }
            AdsrStage::Sustain => {
                self.level = self.sustain;
                if !gate_high {
                    self.enter_release();
                }
            }
            AdsrStage::Release => {
                self.level -= self.release_inc;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.stage = AdsrStage::Idle;
                }
            }
        }
    }

    fn tick_exponential(&mut self, gate_high: bool) {
        match self.stage {
            AdsrStage::Idle => {}
            AdsrStage::Attack => {
                if !gate_high {
                    self.enter_release();
                } else {
                    self.level += self.attack_k * (EXP_ATTACK_TARGET - self.level);
                    if self.level >= 1.0 {
                        self.level = 1.0;
                        self.stage = AdsrStage::Decay;
                    }
                }
            }
            AdsrStage::Decay => {
                if !gate_high {
                    self.enter_release();
                } else {
                    self.level += self.decay_k * (self.sustain - self.level);
                    if self.level <= self.sustain + EXP_SNAP_EPS {
                        self.level = self.sustain;
                        self.stage = AdsrStage::Sustain;
                    }
                }
            }
            AdsrStage::Sustain => {
                self.level = self.sustain;
                if !gate_high {
                    self.enter_release();
                }
            }
            AdsrStage::Release => {
                self.level += self.release_k * (0.0 - self.level);
                if self.level <= EXP_SNAP_EPS {
                    self.level = 0.0;
                    self.stage = AdsrStage::Idle;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(a: f32, d: f32, s: f32, r: f32, sr: f32) -> AdsrCore {
        let mut c = AdsrCore::new(sr);
        c.set_params(a, d, s, r);
        c
    }

    #[test]
    fn attack_sustain_release_cycle() {
        let sr = 48_000.0;
        let mut c = make(0.01, 0.02, 0.5, 0.03, sr);
        let mut peak = c.tick(true, true);
        for _ in 0..(0.01 * sr) as usize + 10 {
            peak = peak.max(c.tick(false, true));
        }
        assert!((peak - 1.0).abs() < 1e-3, "attack peak {peak}");
        for _ in 0..(0.02 * sr) as usize + 10 {
            c.tick(false, true);
        }
        let v = c.tick(false, true);
        assert!((v - 0.5).abs() < 1e-3, "sustain {v}");
        for _ in 0..(0.03 * sr) as usize + 100 {
            c.tick(false, false);
        }
        assert!(c.is_idle());
    }

    #[test]
    fn rapid_gate_stays_in_range() {
        let mut c = make(0.01, 0.01, 0.5, 0.01, 44100.0);
        for _ in 0..50 {
            let v = c.tick(true, true);
            assert!(v.is_finite() && (0.0..=1.0).contains(&v));
            for _ in 0..3 {
                let v = c.tick(false, false);
                assert!(v.is_finite() && (0.0..=1.0).contains(&v));
            }
        }
    }
}
