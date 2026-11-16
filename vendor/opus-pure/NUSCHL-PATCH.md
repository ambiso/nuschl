# Receive decoder corrections

Base: the exact published `opus-pure` 0.2.3 archive, retrieved from
https://static.crates.io/crates/opus-pure/opus-pure-0.2.3.crate (547373 bytes).
SHA-256: `5afacd236411dddcc284983066ef66b27e306e53ce4de85cd745b3da9be8a0be`.
Published `.cargo_vcs_info.json` identifies upstream revision
`3f6077b8bdc0f6c090c3b585de9294065a50e2a4` in
https://github.com/stephenberry/opus-pure.
The original `LICENSE`, `Cargo.toml.orig` and VCS metadata are unchanged.
`LICENSE` SHA-256: `c9fa68646ff1dda4747667ca2ab58c73b199f4876bb31c48172810a2d8a9234b`.

This patch adds a default-false intensity-stereo decoder setting and public
`OpusDecoder::set_phase_inversion_disabled` / `phase_inversion_disabled` controls.
It threads that setting to the already existing `quant_all_bands` argument,
which was hardcoded false. The existing band decoder still consumes the inversion
bit before overriding the synthesis sign: entropy state is unchanged. No encoder,
bitstream parser, unsafe/SIMD path, allocation or default behavior is changed.

`CeltDecoder::reset` resets histories in place, preserving this setting alongside
its existing downsample configuration. All `OpusDecoder` reset, mode/redundancy
transition and channel-switch paths use that same instance; none replaces it.
The added API test covers reset/defaults at every API rate and channel count.
Independent native/WASM fixtures and final-range comparisons are in
`tests/opus_phase.py` and `docs/output-quality.md`.

Nuschl's native and browser voice receivers opt in separately to match stock
Mumble's `AudioOutputSpeech.cpp` setting. Generic and nonlive decoder users retain
the original default. This is a deliberate mono-downmix/stereo tradeoff, not a
claim that inversion-disabled rendering is universally better stereo.

Additional bounded receive fixes match libopus 1.6.1 `opus_decoder.c`:

- Validate all FEC packet framing before concealment or state changes, including
  CELT/short-request fallbacks and leading PLC. Invalid odd-sized CBR payloads
  return an error without consuming decoder history.
- Treat parsed zero- and one-byte payloads as PLC/DTX before changing layer,
  internal sample rate or channel state, including padded/mixed-frame packets.
  A bounded stack buffer reuses the ordinary decoder for mixed nonshort frames.
  PLC reports zero final entropy range and clears consumed redundancy state.
  A private accepted-packet mode remains distinct from the synthesis mode, so
  short-frame mode switches select subsequent FEC fallbacks as libopus does.
- Report the actual SILK FEC range coder or hybrid CELT concealment RNG instead
  of retaining the previous packet's public final range. The CELT accessor is
  crate-private; no public codec API is added.

A separate admission follow-up exposes `packet::validate(&[u8]) -> Result<()>`.
It reuses the decoder's complete ordinary-packet framing parser without
allocation or state mutation. Header-only duration helpers retain their existing
semantics; callers can validate before changing jitter/sequence/gain state, then
read the duration. All valid Opus durations and zero/one-byte DTX remain accepted;
this adds no Mumble duration policy or compressed-entropy validity claim. Three
unit regressions cover valid DTX/padding/all packing codes, malformed framing and
the difference from TOC-only duration inspection. An independent libopus 1.6.1
`opus_packet_parse` audit matched all 74,019 cases (all zero/one/two-byte packets,
seeded longer packets, frame-size limits and explicit padded/multiframe cases);
evidence is at `~/.cache/nuschl-output-quality/framing-api/proof.json`.

A further bounded PLC correction keeps the accepted TOC's per-frame duration
separate from the public complete packet duration. This mirrors libopus
`st->frame_size` (initialized/reset to `Fs/400`) and caps each concealment piece
by the smaller of that duration and 20 ms. A 120 ms packet containing twelve
10 ms CELT frames therefore conceals a 20 ms gap as two 10 ms calls, preserving
filter/pitch history. The public duration API and jitter policy are unchanged;
malformed packets do not update the private cap. The independent CELT matrix in
`tests/opus_celt_plc.py` compares fresh/reused output buffers, ordinary and FEC
concealment, 10/20 ms frames grouped through 120 ms, mode/DTX/reset transitions
and framing-state preservation. See `docs/opus-silk.md` for exact scope/evidence.

The phase setting is unchanged. There are no new dependencies, unsafe code or
backend replacements; the only encoder change is the pitch prefilter clamp
described below. Original provenance bytes above remain
unchanged. Independent synthetic checks, evidence and known direct 8/12 kHz API
resampling limitations are documented in `docs/opus-silk.md`; the strict broad
audit remains failing until those separate limitations are corrected. These
patches do not claim general bitstream or perceptual parity.

# Encoder pitch prefilter period

The music bot played a FLAC file with an audible glitch around 31–33 s that
other players did not have. Symphonia decoded the file bit-exactly against
`flac -d`, and rubato's 48 kHz output matched a soxr resample within −86 dB
without clipping. The glitch came from the Opus encoder: 91 of 9713 packets of
that song (96 kb/s constrained VBR, 20 ms, all CELT fullband) decoded with
libopus 1.6.1 at about 0 dB waveform SNR, in bursts of 4–6 packets, where
libopus's own encoder on the same input stayed above 11 dB there and had no
such packet. Packet sizes were normal (at most 492 bytes). The bursts did not
depend on the bitrate (64, 96 and 192 kb/s) and disappeared below complexity 5,
and also with only the pitch prefilter disabled.

`run_prefilter` clamped the pitch period to `COMBFILTER_MAXPERIOD - 2` (1022)
before `remove_doubling`, which can return 1023. libopus clamps after it
(`celt_encoder.c`, `run_prefilter`). With 1024 samples of history `comb_filter`
cannot apply 1023 and clamps to 1022 on its own, while the bitstream signalled
1023. The decoder's postfilter then undid a comb at another period than the
one applied, at gains around 0.47, and the error was as loud as the music for
as long as the pitch held. All 47 frames of the song with the prefilter on at
period 1023 sat next to a bad packet and accounted for 87 of the 91. The clamp
now follows `remove_doubling`; the song then had no bad packet and an
unchanged median SNR. opus-pure's own decoder, which produced peaks of 1360×
full scale on those packets, stays within 1.13 of full scale. Upstream
opus-pure 0.2.3 and its main branch still clamp before `remove_doubling`.

`prefilter_postfilter_inversion` now shares its round trip with
`the_longest_pitch_period_is_applied_as_signalled`, which feeds noise
repeating every 1021, 1022 and 1023 samples. Before the fix period 1023
round-tripped at 20.4 dB with every engaged frame signalling 1023; now all
three are transparent (above 90 dB) and signal at most 1022.

Further differences from libopus 1.6.1's `run_prefilter` found on the way remain;
none of them caused this glitch:

- The prefilter is not switched off for a frame on strong transients without
  pitch continuity (`tf_estimate > 0.98`), nor when the filter makes a channel
  worse or neither better (`cancel_pitch`). Adding the latter changed nothing
  on the song above.
- The single-tone path (`toneishness`) is missing.
- When the prefilter is disabled for a frame (below complexity 5, hybrid
  mode, or 12 bytes per channel or less), `run_prefilter` is skipped, so the
  previous comb is not faded out and its history goes stale. libopus always
  runs it with `enabled = 0`.
- After a frame without the prefilter the encoder keeps
  `COMBFILTER_MINPERIOD` as the previous period, libopus the searched one. This
  only affects later decisions, not what is applied.
