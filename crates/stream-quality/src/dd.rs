// SPDX-License-Identifier: EUPL-1.2
//! The AV1 Dependency Descriptor RTP header extension (AV1 RTP specification,
//! appendix A) and per-receiver layer choice. [`Layers`] keeps a source's template
//! structure and measures the rate of each decode target; a [`Selector`] per
//! receiver forwards the frames of the decode target it was given and switches
//! targets only at switch points.

use crate::Kind;
use std::time::Duration;

pub const URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";

/// Templates per structure; the template id has six bits.
const MAX_TEMPLATES: usize = 64;
/// The window over which [`Layers`] measures rates.
const WINDOW: Duration = Duration::from_secs(1);

/// A frame's decode target indication (table A.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dti {
    NotPresent,
    Discardable,
    Switch,
    Required,
}

/// A decode target's layers: its spatial id and highest temporal id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub spatial: u8,
    pub temporal: u8,
}

/// Two bits per decode target, target 0 lowest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Dtis(u64);

impl Dtis {
    fn get(self, target: usize) -> Dti {
        match self.0.checked_shr(2 * target as u32).unwrap_or(0) & 3 {
            0 => Dti::NotPresent,
            1 => Dti::Discardable,
            2 => Dti::Switch,
            _ => Dti::Required,
        }
    }

    fn read(bits: &mut Bits, count: usize) -> Option<Self> {
        let mut dtis = 0;
        for target in 0..count {
            dtis |= u64::from(bits.read(2)?) << (2 * target);
        }
        Some(Self(dtis))
    }
}

struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl Bits<'_> {
    /// `f(n)`, n ≤ 32.
    fn read(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self.data.get(self.position / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.read(1)? == 1)
    }

    /// `ns(n)`, a value below n.
    fn ns(&mut self, n: u32) -> Option<u32> {
        let width = u32::BITS - n.leading_zeros();
        let m = (1 << width) - n;
        let value = self.read(width - 1)?;
        if value < m {
            return Some(value);
        }
        Some((value << 1) - m + self.read(1)?)
    }
}

struct Template {
    layer: Target,
    dtis: Dtis,
}

/// A template dependency structure, sent with key frames.
struct Structure {
    offset: u32,
    templates: Vec<Template>,
    targets: Vec<Target>,
    /// The shorter side of each spatial layer, when the publisher sent it.
    heights: Vec<u32>,
    active: u32,
}

impl Structure {
    fn read(bits: &mut Bits) -> Option<Self> {
        let offset = bits.read(6)?;
        let count = bits.read(5)? as usize + 1;
        let mut templates = Vec::new();
        let mut layer = Target::default();
        loop {
            if templates.len() == MAX_TEMPLATES {
                return None;
            }
            templates.push(Template {
                layer,
                dtis: Dtis::default(),
            });
            match bits.read(2)? {
                0 => {}
                1 => layer.temporal += 1,
                2 => {
                    layer = Target {
                        spatial: layer.spatial + 1,
                        temporal: 0,
                    }
                }
                _ => break,
            }
        }
        for template in &mut templates {
            template.dtis = Dtis::read(bits, count)?;
        }
        // Frame diffs and chains matter to decoders; this reads past them.
        for _ in &templates {
            while bits.flag()? {
                bits.read(4)?;
            }
        }
        let chains = bits.ns(count as u32 + 1)?;
        if chains > 0 {
            for _ in 0..count {
                bits.ns(chains)?;
            }
            for _ in 0..templates.len() as u32 * chains {
                bits.read(4)?;
            }
        }
        let targets = (0..count)
            .map(|target| {
                let mut layers = templates
                    .iter()
                    .filter(|t| t.dtis.get(target) != Dti::NotPresent)
                    .map(|t| t.layer);
                let first = layers.next().unwrap_or_default();
                layers.fold(first, |a, b| Target {
                    spatial: a.spatial.max(b.spatial),
                    temporal: a.temporal.max(b.temporal),
                })
            })
            .collect();
        let mut heights = Vec::new();
        if bits.flag()? {
            for _ in 0..=layer.spatial {
                let width = bits.read(16)? + 1;
                heights.push(width.min(bits.read(16)? + 1));
            }
        }
        Some(Self {
            offset,
            templates,
            targets,
            heights,
            active: u32::MAX >> (32 - count),
        })
    }
}

/// One packet's descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub start: bool,
    pub end: bool,
    pub number: u16,
    pub layer: Target,
    dtis: Dtis,
}

impl Frame {
    pub fn dti(&self, target: usize) -> Dti {
        self.dtis.get(target)
    }
}

/// A decode target a receiver can be given, from [`Layers::choices`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The decode target index.
    pub target: usize,
    pub layer: Target,
    /// The measured rate of the target, its lower layers included.
    pub rate: u64,
}

/// One source's descriptor state and the measured rate and frame rate of each of
/// its decode targets over the last second.
#[derive(Default)]
pub struct Layers {
    structure: Option<Structure>,
    window: Duration,
    bytes: [u64; 32],
    frames: [u32; 32],
    rates: [u64; 32],
    fps: [u32; 32],
}

impl Layers {
    /// Reads one packet's descriptor and counts its `size` bytes toward the
    /// decode targets the frame belongs to. `None` for a malformed descriptor or
    /// one that needs a structure this source has not sent.
    pub fn packet(&mut self, descriptor: &[u8], size: usize, now: Duration) -> Option<Frame> {
        let frame = self.parse(descriptor)?;
        let elapsed = now.saturating_sub(self.window);
        if elapsed >= WINDOW {
            let millis = elapsed.as_millis() as u64;
            for target in 0..32 {
                self.rates[target] = self.bytes[target] * 8000 / millis;
                self.fps[target] = (u64::from(self.frames[target]) * 1000 / millis) as u32;
            }
            self.bytes = [0; 32];
            self.frames = [0; 32];
            self.window = now;
        }
        for target in 0..32 {
            if frame.dti(target) != Dti::NotPresent {
                self.bytes[target] += size as u64;
                self.frames[target] += u32::from(frame.start);
            }
        }
        Some(frame)
    }

    fn parse(&mut self, descriptor: &[u8]) -> Option<Frame> {
        let bits = &mut Bits {
            data: descriptor,
            position: 0,
        };
        let start = bits.flag()?;
        let end = bits.flag()?;
        let template_id = bits.read(6)?;
        let number = bits.read(16)? as u16;
        let (mut new, mut active, mut custom_dtis) = (None, None, false);
        if descriptor.len() > 3 {
            let has_structure = bits.flag()?;
            let has_active = bits.flag()?;
            custom_dtis = bits.flag()?;
            bits.read(2)?;
            if has_structure {
                new = Some(Structure::read(bits)?);
            }
            if has_active {
                let structure = new.as_ref().or(self.structure.as_ref())?;
                active = Some(bits.read(structure.targets.len() as u32)?);
            }
        }
        let structure = new.as_ref().or(self.structure.as_ref())?;
        let template = structure
            .templates
            .get(((template_id + 64 - structure.offset) % 64) as usize)?;
        let dtis = if custom_dtis {
            Dtis::read(bits, structure.targets.len())?
        } else {
            template.dtis
        };
        let frame = Frame {
            start,
            end,
            number,
            layer: template.layer,
            dtis,
        };
        if new.is_some() {
            self.structure = new;
        }
        if let (Some(structure), Some(active)) = (&mut self.structure, active) {
            structure.active = active;
        }
        Some(frame)
    }

    /// The active decode targets that fit tier `tier` of `kind`'s ladder, one per
    /// layer pair, lowest first; the lowest active target when none fits. A target
    /// fits when its spatial layer belongs to the tier or a lower one (when the
    /// publisher sent layer sizes) and its frame rate is at most a quarter above
    /// the tier's. These are the steps of [`crate::budget::Stream`] and
    /// [`crate::adapt::Stepper`].
    pub fn choices(&self, kind: Kind, tier: usize) -> Vec<Choice> {
        let Some(structure) = &self.structure else {
            return Vec::new();
        };
        let tier_fps = u32::from(kind.ladder()[tier.min(kind.top())].fps);
        let mut all: Vec<_> = structure
            .targets
            .iter()
            .enumerate()
            .filter(|(target, _)| structure.active >> target & 1 == 1)
            .map(|(target, &layer)| Choice {
                target,
                layer,
                rate: self.rates[target],
            })
            .collect();
        all.sort_by_key(|c| (c.layer.spatial, c.layer.temporal));
        all.dedup_by_key(|c| c.layer);
        let fits = |c: &Choice| {
            structure
                .heights
                .get(usize::from(c.layer.spatial))
                .is_none_or(|&h| kind.tier_of(h) <= tier)
                && self.fps[c.target] * 4 <= tier_fps * 5
        };
        if all.iter().any(fits) {
            all.retain(fits);
        } else {
            all.truncate(1);
        }
        all
    }
}

/// One receiver's view of one source: forwards the frames of its decode target.
#[derive(Clone, Copy, Debug, Default)]
pub struct Selector {
    current: Option<usize>,
    wanted: Option<usize>,
}

impl Selector {
    /// Sets the decode target to forward, `None` to forward nothing. The switch
    /// happens at the start of the next frame that is a switch point for it.
    pub fn want(&mut self, target: Option<usize>) {
        self.wanted = target;
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// Whether a switch waits for a switch point; the caller asks the publisher
    /// for a key frame.
    pub fn waiting(&self) -> bool {
        self.wanted.is_some() && self.current != self.wanted
    }

    /// Whether to forward a packet of `frame`.
    pub fn forward(&mut self, frame: &Frame) -> bool {
        if frame.start && self.current != self.wanted {
            match self.wanted {
                None => self.current = None,
                Some(target) if frame.dti(target) == Dti::Switch => self.current = Some(target),
                Some(_) => {}
            }
        }
        self.current
            .is_some_and(|target| frame.dti(target) != Dti::NotPresent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// Descriptors of an L3T3 key-frame stream captured from Chrome, from
    /// LiveKit's dependency descriptor tests (Apache-2.0).
    const CHROME_L3T3: [&str; 7] = [
        "c1017280081485214eafffaaaa863cf0430c10c302afc0aaa0063c00430010c002a000a80006000040001d954926e082b04a0941b820ac1282503157f974000ca864330e222222eca8655304224230eca877530077004200ef008601df010d",
        "86017340fc",
        "46017340fc",
        "c3017540fc",
        "88017640fc",
        "48017640fc",
        "c2017840fc",
    ];

    fn chrome() -> (Layers, Vec<Frame>) {
        let mut layers = Layers::default();
        let frames = CHROME_L3T3
            .iter()
            .map(|h| layers.packet(&hex(h), 1000, Duration::ZERO).expect("valid"))
            .collect();
        (layers, frames)
    }

    fn dtis(frame: &Frame) -> String {
        (0..9)
            .map(|target| match frame.dti(target) {
                Dti::NotPresent => '-',
                Dti::Discardable => 'D',
                Dti::Switch => 'S',
                Dti::Required => 'R',
            })
            .collect()
    }

    #[test]
    fn reads_a_chrome_structure_and_its_frames() {
        let (layers, frames) = chrome();
        let structure = layers.structure.as_ref().expect("structure");
        // libwebrtc's full L3T3: 15 templates, nine decode targets, spatial-major.
        assert_eq!(structure.templates.len(), 15);
        let targets: Vec<_> = structure
            .targets
            .iter()
            .map(|t| (t.spatial, t.temporal))
            .collect();
        assert_eq!(
            targets,
            [
                (0, 0),
                (0, 1),
                (0, 2),
                (1, 0),
                (1, 1),
                (1, 2),
                (2, 0),
                (2, 1),
                (2, 2)
            ]
        );
        assert_eq!(structure.heights, [67, 135, 270]);
        // The later packets turn the top spatial layer off.
        assert_eq!(structure.active, 0b111111);
        let summary: Vec<_> = frames
            .iter()
            .map(|f| {
                (
                    f.start,
                    f.end,
                    f.number,
                    f.layer.spatial,
                    f.layer.temporal,
                    dtis(f),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (true, true, 0x0172, 0, 0, "SSSSSSSSS".into()),
                (true, false, 0x0173, 1, 0, "---SSSSSS".into()),
                (false, true, 0x0173, 1, 0, "---SSSSSS".into()),
                (true, true, 0x0175, 0, 2, "--D--R--R".into()),
                (true, false, 0x0176, 1, 2, "-----D--R".into()),
                (false, true, 0x0176, 1, 2, "-----D--R".into()),
                (true, true, 0x0178, 0, 1, "-DS-RR-RR".into()),
            ]
        );
    }

    #[test]
    fn needs_a_structure_and_rejects_malformed_descriptors() {
        let mut layers = Layers::default();
        assert_eq!(layers.packet(&hex(CHROME_L3T3[1]), 1, Duration::ZERO), None);
        assert_eq!(layers.packet(&[0x80, 0x01], 1, Duration::ZERO), None);
        let full = hex(CHROME_L3T3[0]);
        assert_eq!(layers.packet(&full[..20], 1, Duration::ZERO), None);
        assert!(layers.structure.is_none());
        assert!(layers.packet(&full, 1, Duration::ZERO).is_some());
        // A template id outside the structure.
        assert_eq!(layers.packet(&[0xff, 0, 0], 1, Duration::ZERO), None);
    }

    /// A synthetic L1T3 stream with libwebrtc's decode target indications:
    /// templates T0, T1, T2 and decode targets for a quarter, half and all of the
    /// frame rate. T0 frames are switch points for all three, T1 frames for the
    /// full rate.
    fn l1t3(id: u32, number: u16, structure: bool) -> Vec<u8> {
        let mut bits = Vec::new();
        let mut put = |value: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push((value >> i) & 1 == 1);
            }
        };
        put(1, 1);
        put(1, 1);
        put(id, 6);
        put(u32::from(number), 16);
        if structure {
            put(0b10000, 5);
            put(0, 6); // offset
            put(2, 5); // three decode targets
            put(1, 2);
            put(1, 2);
            put(3, 2); // T0, T1, T2
            put(0b10_10_10, 6); // T0: S S S
            put(0b00_01_10, 6); // T1: - D S
            put(0b00_00_01, 6); // T2: - - D
            put(0, 3); // no frame diffs
            put(0, 2); // no chains: ns(4) of 0
            put(0, 1); // no sizes
        }
        while bits.len() % 8 != 0 {
            bits.push(false);
        }
        bits.chunks(8)
            .map(|byte| byte.iter().fold(0, |a, &b| (a << 1) | u8::from(b)))
            .collect()
    }

    #[test]
    fn reads_dtis_in_target_order() {
        let mut layers = Layers::default();
        let frame = layers
            .packet(&l1t3(0, 0, true), 1, Duration::ZERO)
            .expect("valid");
        assert_eq!([frame.dti(0), frame.dti(1), frame.dti(2)], [Dti::Switch; 3]);
        let t1 = layers
            .packet(&l1t3(1, 1, false), 1, Duration::ZERO)
            .expect("valid");
        assert_eq!(
            [t1.dti(0), t1.dti(1), t1.dti(2)],
            [Dti::NotPresent, Dti::Discardable, Dti::Switch]
        );
        let structure = layers.structure.as_ref().expect("structure");
        let targets: Vec<_> = structure.targets.iter().map(|t| t.temporal).collect();
        assert_eq!(targets, [0, 1, 2]);
    }

    #[test]
    fn a_receiver_gets_its_temporal_layers_and_switches_at_switch_points() {
        let mut layers = Layers::default();
        let mut low = Selector::default();
        let mut full = Selector::default();
        low.want(Some(0));
        full.want(Some(2));
        // T0 T2 T1 T2 repeated, 30 frames a second.
        let pattern = [0, 2, 1, 2];
        let mut forwarded = (0, 0);
        for number in 0..40u16 {
            let id = pattern[usize::from(number) % 4];
            let frame = layers
                .packet(&l1t3(id, number, number == 0), 1, Duration::ZERO)
                .expect("valid");
            forwarded.0 += u32::from(low.forward(&frame));
            forwarded.1 += u32::from(full.forward(&frame));
            if number == 21 {
                // Up at the next switch point, the T1 frame 22.
                low.want(Some(2));
                assert!(low.waiting());
            }
            if number == 22 {
                assert!(!low.waiting());
            }
        }
        // Frames 0, 4, …, 20 at T0, then all frames from 22 on.
        assert_eq!(forwarded, (6 + 18, 40));
        // Down to nothing at once.
        low.want(None);
        let frame = layers
            .packet(&l1t3(1, 41, false), 1, Duration::ZERO)
            .expect("valid");
        assert!(!low.forward(&frame));
        assert_eq!(low.current(), None);
        assert!(!low.waiting());
    }

    #[test]
    fn measures_rates_and_offers_the_targets_that_fit() {
        let mut layers = Layers::default();
        let pattern = [0, 2, 1, 2];
        // Two seconds at 30 frames a second, 1000 bytes a frame.
        for number in 0..60u16 {
            let at = Duration::from_millis(u64::from(number) * 1000 / 30);
            let id = pattern[usize::from(number) % 4];
            layers.packet(&l1t3(id, number, number == 0), 1000, at);
        }
        layers.packet(&l1t3(0, 60, false), 1000, Duration::from_secs(2));
        let choices = layers.choices(Kind::Camera, 3);
        let rates: Vec<_> = choices.iter().map(|c| c.rate).collect();
        // Frames 30 to 59 in the second window: 7, 15 and 30 of 8000 bits.
        assert_eq!(rates, [56_000, 120_000, 240_000]);
        // A 15 fps tier takes the 7.5 and 15 fps targets.
        let low: Vec<_> = layers
            .choices(Kind::Camera, 0)
            .iter()
            .map(|c| c.target)
            .collect();
        assert_eq!(low, [0, 1]);
    }

    #[test]
    fn active_targets_and_layer_sizes_limit_the_choice() {
        let (mut layers, _) = chrome();
        let targets: Vec<_> = layers
            .choices(Kind::Camera, 3)
            .iter()
            .map(|c| c.target)
            .collect();
        assert_eq!(targets, [0, 1, 2, 3, 4, 5]);
        let structure = layers.structure.as_mut().expect("structure");
        structure.active = u32::MAX;
        structure.heights = vec![180, 360, 720];
        let spatial = |tier| {
            let choices = layers.choices(Kind::Camera, tier);
            choices
                .iter()
                .map(|c| c.layer.spatial)
                .max()
                .expect("a choice")
        };
        assert_eq!(spatial(3), 2);
        assert_eq!(spatial(2), 2);
        assert_eq!(spatial(1), 1);
        assert_eq!(spatial(0), 0);
        // A screen tier is taller than every layer.
        layers.structure.as_mut().expect("structure").heights = vec![1440, 2160, 4320];
        assert_eq!(layers.choices(Kind::Screen, 0).len(), 1);
    }

    #[test]
    fn an_active_mask_limits_the_choice() {
        let mut layers = Layers::default();
        layers.packet(&l1t3(0, 0, true), 1, Duration::ZERO);
        // Extended fields with only an active mask, 0b011: targets 0 and 1.
        let mut packet = l1t3(1, 1, false);
        packet.extend([0x43, 0]);
        layers.packet(&packet, 1, Duration::ZERO).expect("valid");
        let targets: Vec<_> = layers
            .choices(Kind::Camera, 3)
            .iter()
            .map(|c| c.target)
            .collect();
        assert_eq!(targets, [0, 1]);
    }
}
