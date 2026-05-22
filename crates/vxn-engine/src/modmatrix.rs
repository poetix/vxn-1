//! A small fixed modulation matrix — the reusable heart of VXN1's mod section,
//! shaped after the Jupiter-8 (assignable envelopes + LFO) and generalised so
//! every source can reach every destination.
//!
//! Sources: ENV-1, ENV-2, LFO, Velocity, KeyFollow.
//! Destinations: Pitch, Cutoff, PWM (additive), Amp (the VCA control sum).
//!
//! The matrix itself is pure data: `depth[source][destination]`. Destination
//! units are the destination's own (semitones for Pitch/Cutoff, fraction for
//! PWM, gain for Amp); each route's parameter range encodes a sensible span.

/// Modulation sources, in fixed order. Index = array row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum ModSource {
    Env1,
    Env2,
    Lfo,
    Velocity,
    KeyFollow,
}

impl ModSource {
    pub const COUNT: usize = 5;
    pub const ALL: [ModSource; Self::COUNT] = [
        ModSource::Env1,
        ModSource::Env2,
        ModSource::Lfo,
        ModSource::Velocity,
        ModSource::KeyFollow,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ModSource::Env1 => "Env 1",
            ModSource::Env2 => "Env 2",
            ModSource::Lfo => "LFO",
            ModSource::Velocity => "Velocity",
            ModSource::KeyFollow => "Key Follow",
        }
    }
}

/// Modulation destinations, in fixed order. Index = array column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum ModDest {
    Pitch,
    Cutoff,
    Amp,
    Pwm,
}

impl ModDest {
    pub const COUNT: usize = 4;
    pub const ALL: [ModDest; Self::COUNT] =
        [ModDest::Pitch, ModDest::Cutoff, ModDest::Amp, ModDest::Pwm];

    pub fn label(self) -> &'static str {
        match self {
            ModDest::Pitch => "Pitch",
            ModDest::Cutoff => "Cutoff",
            ModDest::Amp => "Amp",
            ModDest::Pwm => "PWM",
        }
    }
}

/// `depth[source][destination]` modulation amounts. Cheap to copy.
#[derive(Clone, Copy)]
pub struct ModMatrix {
    pub depth: [[f32; ModDest::COUNT]; ModSource::COUNT],
}

impl Default for ModMatrix {
    fn default() -> Self {
        Self::new()
    }
}

impl ModMatrix {
    pub fn new() -> Self {
        Self { depth: [[0.0; ModDest::COUNT]; ModSource::COUNT] }
    }

    #[inline]
    pub fn set(&mut self, src: ModSource, dest: ModDest, amount: f32) {
        self.depth[src as usize][dest as usize] = amount;
    }

    /// Total modulation reaching `dest`, given current source values.
    /// `srcs` is indexed by [`ModSource`] order.
    #[inline]
    pub fn dest(&self, dest: ModDest, srcs: &[f32; ModSource::COUNT]) -> f32 {
        let d = dest as usize;
        self.depth
            .iter()
            .zip(srcs.iter())
            .map(|(row, &v)| v * row[d])
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_matrix_passes_nothing() {
        let m = ModMatrix::new();
        let srcs = [1.0; ModSource::COUNT];
        for d in ModDest::ALL {
            assert_eq!(m.dest(d, &srcs), 0.0);
        }
    }

    #[test]
    fn routes_sum_per_destination() {
        let mut m = ModMatrix::new();
        m.set(ModSource::Env2, ModDest::Amp, 1.0);
        m.set(ModSource::Lfo, ModDest::Amp, 0.5);
        m.set(ModSource::Env1, ModDest::Cutoff, 24.0);
        // srcs: env1=0.5, env2=0.8, lfo=-1, vel=1, key=2
        let srcs = [0.5, 0.8, -1.0, 1.0, 2.0];
        assert!((m.dest(ModDest::Amp, &srcs) - (0.8 - 0.5)).abs() < 1e-6);
        assert!((m.dest(ModDest::Cutoff, &srcs) - 12.0).abs() < 1e-6);
        assert_eq!(m.dest(ModDest::Pitch, &srcs), 0.0);
    }
}
