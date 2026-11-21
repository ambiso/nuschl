// SPDX-License-Identifier: EUPL-1.2
//! Deep PLC against the classic concealment on the same received packets,
//! and the worker's real-time margin while speakers conceal losses.
//! Synthetic speech through the production encoder and the decoder set up
//! as `Speaker` sets it up. The tests print measurements; run them in release:
//! `cargo test --release -p nuschl-media-worker -- --ignored --nocapture concealment`.
//!
//! Instructions per frame: run `concealment::cost` alone under
//! `perf stat -e instructions:u`, once per `NUSCHL_PLC_COST` mode (`encode`,
//! `clean0`, `clean5`, `lossy0`, `lossy5`) at a `NUSCHL_PLC_BITRATE`, and
//! subtract: `encode` is the shared part, `clean*` adds a decode of every
//! packet at that complexity, `lossy*` conceals the frames it prints.
use super::*;
use crate::encoder::{
    self,
    cvbr::{Signal, synthesize},
};
use nuschl_core::{audio::DEFAULT_VOICE_BITRATE, bitrate::max_voice_frame_bytes};
use std::sync::atomic::{AtomicBool, Ordering};

const PACKET: usize = 2 * FRAME;

/// Packets of synthetic speech from a production encoder at `bitrate`,
/// each within the destination's per-frame allowance as `Transmit` sends it.
/// With `fec`, the live encoder's in-band FEC rebuilds single losses.
fn encode(bitrate: u32, fec: bool) -> Vec<Vec<u8>> {
    let mut encoder = if fec { encoder::live() } else { encoder::new() }.unwrap();
    encoder.bitrate_bps = bitrate as i32;
    let mut output = vec![0; max_voice_frame_bytes(bitrate)];
    synthesize(Signal::Speech)
        .as_chunks::<PACKET>()
        .0
        .iter()
        .map(|frame| {
            let size = encoder.encode(frame, PACKET, &mut output).unwrap();
            output[..size].to_vec()
        })
        .collect()
}

/// Which packets are lost: a two-state model with `rate` loss in bursts of
/// `burst` packets on average. The first ten packets arrive.
fn losses(count: usize, rate: f64, burst: usize, seed: u64) -> Vec<bool> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1_u64 << 53) as f64
    };
    let leave = 1. / burst as f64;
    let enter = rate * leave / (1. - rate);
    let mut lost = false;
    (0..count)
        .map(|index| {
            lost = if lost {
                random() >= leave
            } else {
                random() < enter
            };
            index >= 10 && lost
        })
        .collect()
}

/// The left channel as a speaker's decoder plays the packets at
/// `complexity`: a loss is rebuilt from the next packet's FEC when that
/// packet arrived, and concealed otherwise.
fn decode(packets: &[Vec<u8>], lost: &[bool], complexity: i32) -> Vec<f32> {
    let mut decoder = OpusDecoder::new(48_000, 2).unwrap();
    decoder.set_phase_inversion_disabled(true);
    decoder.set_complexity(complexity).unwrap();
    let mut pcm = [0.; 2 * PACKET];
    let mut left = Vec::with_capacity(packets.len() * PACKET);
    for (index, opus) in packets.iter().enumerate() {
        let count = if !lost[index] {
            decoder.decode(opus, PACKET, &mut pcm)
        } else if let Some(next) = packets.get(index + 1).filter(|_| !lost[index + 1]) {
            decoder.decode_fec(next, PACKET, &mut pcm)
        } else {
            decoder.decode(&[], PACKET, &mut pcm)
        }
        .unwrap();
        assert_eq!(count, PACKET);
        assert!(pcm.iter().all(|sample| sample.is_finite()));
        left.extend(pcm.iter().step_by(2));
    }
    left
}

/// Hann-windowed power at 50 Hz steps up to 20 kHz.
struct Spectrum {
    window: Vec<f64>,
    basis: Vec<(f64, f64)>,
}

const BINS: usize = 400;
/// The bins up to 8 kHz, the band Deep PLC models.
const SPEECH_BINS: usize = 160;

impl Spectrum {
    fn new() -> Self {
        let tau = std::f64::consts::TAU;
        Self {
            window: (0..PACKET)
                .map(|n| 0.5 - 0.5 * (tau * n as f64 / PACKET as f64).cos())
                .collect(),
            basis: (1..=BINS)
                .flat_map(|bin| {
                    (0..PACKET).map(move |n| {
                        let phase = tau * (bin * n) as f64 / PACKET as f64;
                        (phase.cos(), phase.sin())
                    })
                })
                .collect(),
        }
    }

    fn power(&self, frame: &[f32]) -> Vec<f64> {
        let windowed: Vec<f64> = frame
            .iter()
            .zip(&self.window)
            .map(|(sample, weight)| f64::from(*sample) * weight)
            .collect();
        self.basis
            .as_chunks::<PACKET>()
            .0
            .iter()
            .map(|basis| {
                let (re, im) = windowed
                    .iter()
                    .zip(basis)
                    .fold((0., 0.), |(re, im), (x, (c, s))| (re + x * c, im + x * s));
                re * re + im * im
            })
            .collect()
    }
}

/// Over the lost frames where the loss-free decode is not silent: the mean
/// log-spectral distance to that decode in dB up to 8 kHz and up to 20 kHz,
/// and the mean level error in dB.
fn quality(spectrum: &Spectrum, reference: &[f32], test: &[f32], lost: &[bool]) -> [f64; 3] {
    let (mut speech, mut full, mut level, mut frames) = (0., 0., 0., 0);
    for (index, _) in lost.iter().enumerate().filter(|(_, lost)| **lost) {
        let range = index * PACKET..(index + 1) * PACKET;
        let energy = |pcm: &[f32]| pcm.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();
        let reference_energy = energy(&reference[range.clone()]);
        if reference_energy < PACKET as f64 * 1e-6 {
            continue;
        }
        let (a, b) = (
            spectrum.power(&reference[range.clone()]),
            spectrum.power(&test[range.clone()]),
        );
        let floor = a.iter().copied().fold(0., f64::max) * 1e-6;
        let squared: Vec<f64> = a
            .iter()
            .zip(&b)
            .map(|(a, b)| (10. * ((a + floor) / (b + floor)).log10()).powi(2))
            .collect();
        speech += (squared[..SPEECH_BINS].iter().sum::<f64>() / SPEECH_BINS as f64).sqrt();
        full += (squared.iter().sum::<f64>() / BINS as f64).sqrt();
        level += 10. * (energy(&test[range]).max(1e-20) / reference_energy).log10();
        frames += 1;
    }
    [speech, full, level].map(|sum| sum / f64::from(frames))
}

#[test]
#[ignore = "measurement; run in release with --ignored --nocapture"]
fn quality_of_deep_plc_against_classic_concealment() {
    let spectrum = Spectrum::new();
    println!("Every loss concealed (encoder without FEC).");
    println!(
        "bitrate loss burst lost | log-spectral distance dB to 8 kHz: classic neural | to 20 kHz: classic neural | level error dB: classic neural"
    );
    let mut sums = [[0.; 3]; 2];
    let mut cases = 0;
    for bitrate in [24_000, DEFAULT_VOICE_BITRATE] {
        let packets = encode(bitrate, false);
        let reference = decode(&packets, &vec![false; packets.len()], 0);
        for rate in [0.05, 0.1, 0.2] {
            for burst in [1, 2, 4] {
                let lost = losses(
                    packets.len(),
                    rate,
                    burst,
                    (rate * 100.) as u64 + burst as u64,
                );
                let [classic, neural] = [0, NEURAL_COMPLEXITY].map(|complexity| {
                    quality(
                        &spectrum,
                        &reference,
                        &decode(&packets, &lost, complexity),
                        &lost,
                    )
                });
                for (sum, value) in sums.iter_mut().zip([classic, neural]) {
                    for (sum, value) in sum.iter_mut().zip(value) {
                        *sum += value;
                    }
                }
                cases += 1;
                println!(
                    "{bitrate:>7} {:>3.0}% {burst:>5} {:>4} | {:>6.2} {:>6.2} | {:>6.2} {:>6.2} | {:>6.2} {:>6.2}",
                    rate * 100.,
                    lost.iter().filter(|lost| **lost).count(),
                    classic[0],
                    neural[0],
                    classic[1],
                    neural[1],
                    classic[2],
                    neural[2],
                );
            }
        }
    }
    let [classic, neural] = sums.map(|sum| sum.map(|value| value / f64::from(cases)));
    println!(
        "mean: to 8 kHz classic {:.2} neural {:.2} dB; to 20 kHz classic {:.2} neural {:.2} dB; level classic {:.2} neural {:.2} dB",
        classic[0], neural[0], classic[1], neural[1], classic[2], neural[2]
    );
}

#[test]
#[ignore = "measurement under perf stat; see the module documentation"]
fn cost() {
    let Ok(mode) = std::env::var("NUSCHL_PLC_COST") else {
        return;
    };
    let bitrate = std::env::var("NUSCHL_PLC_BITRATE").map_or(24_000, |rate| rate.parse().unwrap());
    let packets = encode(bitrate, false);
    let clean = vec![false; packets.len()];
    let lossy = losses(packets.len(), 0.2, 1, 1);
    let (lost, complexity) = match mode.as_str() {
        "encode" => return,
        "clean0" => (&clean, 0),
        "clean5" => (&clean, NEURAL_COMPLEXITY),
        "lossy0" => (&lossy, 0),
        "lossy5" => (&lossy, NEURAL_COMPLEXITY),
        _ => panic!("unknown NUSCHL_PLC_COST mode {mode}"),
    };
    std::hint::black_box(decode(&packets, lost, complexity));
    println!(
        "{} packets, {} lost and concealed (encoder without FEC)",
        packets.len(),
        lossy.iter().filter(|lost| **lost).count()
    );
}

#[test]
#[ignore = "measurement; run in release with --ignored --nocapture"]
fn real_time_margin_with_concealing_speakers() {
    let packets = encode(DEFAULT_VOICE_BITRATE, true);
    println!(
        "speakers load concealment | mix time per 10 ms frame: median p99 max | frames over 10 ms"
    );
    for speakers in [1_u32, 8, 32] {
        let lost: Vec<_> = (0..speakers)
            .map(|speaker| losses(packets.len(), 0.2, 2, u64::from(speaker) + 7))
            .collect();
        for load in [false, true] {
            let stop = AtomicBool::new(false);
            std::thread::scope(|scope| {
                if load {
                    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
                    for _ in 0..cpus {
                        scope.spawn(|| {
                            while !stop.load(Ordering::Relaxed) {
                                std::hint::spin_loop();
                            }
                        });
                    }
                }
                for (name, limit) in [
                    ("classic", Duration::ZERO),
                    ("budgeted", NEURAL_BUDGET),
                    ("neural", Duration::from_secs(1)),
                ] {
                    let mut mixer = Mixer::default();
                    mixer.budget.limit = limit;
                    let start = Instant::now();
                    let mut wide = [[0.; 2]; FRAME];
                    let mut times = Vec::with_capacity(2 * packets.len());
                    for tick in 0..2 * packets.len() {
                        if tick % 2 == 0 {
                            let index = tick / 2;
                            for speaker in 0..speakers {
                                if !lost[speaker as usize][index] {
                                    let voice = Voice {
                                        stream: 1,
                                        sender: speaker,
                                        sequence: 2 * index as u64,
                                        opus: packets[index].clone(),
                                        end: false,
                                        volume: 1.,
                                    };
                                    let now = start + Duration::from_millis(10 * tick as u64);
                                    mixer.push(voice, now).unwrap();
                                }
                            }
                        }
                        let began = Instant::now();
                        mixer.mix(&mut wide);
                        times.push(began.elapsed());
                    }
                    times.sort_unstable();
                    let over = times
                        .iter()
                        .filter(|time| **time > Duration::from_millis(10))
                        .count();
                    println!(
                        "{speakers:>8} {:>4} {name:>11} | {:>7.3} {:>7.3} {:>7.3} ms | {over}",
                        if load { "yes" } else { "no" },
                        times[times.len() / 2].as_secs_f64() * 1e3,
                        times[times.len() * 99 / 100].as_secs_f64() * 1e3,
                        times[times.len() - 1].as_secs_f64() * 1e3,
                    );
                }
                stop.store(true, Ordering::Relaxed);
            });
        }
    }
}
