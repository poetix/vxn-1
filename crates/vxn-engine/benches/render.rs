//! Steady-state render benchmark. Measures the cost of rendering a full
//! 16-voice load so we can judge real-time headroom before deciding whether
//! SoA/SIMD vectorisation is worth the complexity.
//!
//! Throughput is reported in samples/sec. Divide by the sample rate (48 000)
//! to get the real-time factor at full 16-voice polyphony: e.g. 4.8 M
//! samples/sec ÷ 48 000 = 100× real-time → ~1% of one core for one instance.

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::time::Duration;
use vxn_engine::{ParamId, Synth};

const SR: f32 = 48_000.0;
const FRAMES: usize = 512;

/// Build a synth with 16 voices held at sustain. `fx` toggles chorus + delay;
/// `res` sets filter resonance (high resonance exercises the ladder hardest).
fn setup(fx: bool, res: f32) -> Synth {
    let mut s = Synth::new(SR);
    s.set_param(ParamId::ChorusOn.index(), if fx { 1.0 } else { 0.0 });
    s.set_param(ParamId::DelayOn.index(), if fx { 1.0 } else { 0.0 });
    s.set_param(ParamId::Resonance.index(), res);
    s.set_param(ParamId::NoiseLevel.index(), 0.2);
    // Route ENV-1 -> cutoff and LFO -> pitch so the matrix is doing real work.
    s.set_param(ParamId::Env1Cutoff.index(), 24.0);
    s.set_param(ParamId::LfoPitch.index(), 3.0);
    for n in 48..64u8 {
        s.note_on(n, 1.0);
    }
    // Warm past the attack so we measure the sustained steady state.
    let mut l = vec![0.0; FRAMES];
    let mut r = vec![0.0; FRAMES];
    for _ in 0..40 {
        s.process(&mut l, &mut r);
    }
    s
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("render_16_voices");
    group.throughput(Throughput::Elements(FRAMES as u64));
    group.measurement_time(Duration::from_secs(3));
    group.sample_size(60);

    for (name, fx, res) in [
        ("dry_lowres", false, 0.2),
        ("dry_selfosc", false, 1.0),
        ("with_fx", true, 0.2),
    ] {
        let mut s = setup(fx, res);
        let mut l = vec![0.0; FRAMES];
        let mut r = vec![0.0; FRAMES];
        group.bench_function(name, |b| {
            b.iter(|| s.process(black_box(&mut l), black_box(&mut r)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
