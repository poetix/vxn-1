# VXN1

**VXN1** ("vixen 1") is a Jupiter-8-style analogue polysynth by [Vulpus Labs](https://github.com/Vulpus-Labs),
built in Rust as a [CLAP](https://cleveraudio.org/) plugin.

16-voice polyphony, dual oscillators with cross-modulation, a 4-pole ladder
filter, and a vintage-flavoured chorus — packaged as a single `.clap` bundle.

## Features

- **16-voice polyphony** with per-voice envelopes and a global LFO.
- **Dual oscillators** with hard-sync and phase/cross-modulation, plus optional
  8× oversampling as an anti-aliasing escape hatch for non-sine cross-mod.
- **4-pole ladder VCF** (Moog-style), with a separate high-pass filter.
- **Pitch envelope** alongside the amplitude and filter envelopes.
- **Modulated-delay chorus** (authentic BBD bucket-brigade model planned).
- **Vizia** GUI embedded via CLAP's `gui` extension.

## Architecture

VXN1 is a Cargo workspace:

| Crate        | Role                                                                       |
| ------------ | -------------------------------------------------------------------------- |
| `vxn-dsp`    | Framework-free, allocation-free DSP kernels (oscillators, filters, ADSR…). |
| `vxn-engine` | Parameter model, voice allocation, and block-rate render loop.             |
| `vxn-ui`     | Vizia-based plugin GUI.                                                     |
| `vxn-clap`   | [clack](https://github.com/prokopyl/clack) cdylib — the CLAP entry point.  |
| `xtask`      | Bundler / build tooling.                                                   |

**Processing model:** DSP kernels run per-sample (the recurrences are serial and
kept bit-faithful to their [`patches`](https://github.com/Vulpus-Labs) origins).
The engine drives fixed 32-sample control blocks (`CONTROL_BLOCK`), recomputing
modulation and filter coefficients once per block while the inner per-sample
loop stays branch-light.

clack was chosen over nih-plug specifically for its fully-permissive license
(MIT OR Apache-2.0); nih-plug's VST3 export pulls in GPLv3.

## Building

```sh
cargo build --release
cargo xtask bundle --release            # produce the VXN1.clap bundle
cargo xtask bundle --release --install  # …and install it locally
```

Requires Rust 1.85+ (edition 2024).

## License

Licensed under the [MIT License](LICENSE.txt).
