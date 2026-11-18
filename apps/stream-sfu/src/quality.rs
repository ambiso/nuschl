// SPDX-License-Identifier: EUPL-1.2
//! Per-receiver video quality (docs/video-plan.md §5) on `nuschl-stream-quality`.
//!
//! The AV1 Dependency Descriptor travels through str0m as a user header
//! extension on both legs, as the publisher wrote it.
//!
//! A receiver gets the frames of one decode target per layered source from
//! Q1's `Selector`, which switches at switch points, with sequence numbers of
//! its own ([`Rewrite`]) so frames it skips leave no gaps. A source without a
//! descriptor reaches every receiver whole.
//!
//! A receiver asks for a tier per source (`QualityRequest`) and states the
//! tiers it decodes (`DecodeCapacity`), each at most once a second. The tier it
//! gets is the least of both; until it asks, it asks for the top tier. Once a
//! second each publisher hears the largest tier its receivers get per source.

use nuschl_stream_quality::{
    DecodeCapacity, Kind, Throttle,
    dd::{Choice, Layers, Selector},
    receive,
};
use std::collections::VecDeque;
use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues};

/// A video source: its kind and the demand last sent to its publisher.
pub struct Video {
    pub kind: Kind,
    pub demand: Option<usize>,
    pub throttle: Throttle,
    pub layers: Layers,
}

impl Video {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            demand: None,
            throttle: Throttle::default(),
            layers: Layers::default(),
        }
    }
}

/// One receiver's view of one video source.
#[derive(Default)]
pub struct View {
    /// The tier asked for.
    pub request: Option<usize>,
    pub throttle: Throttle,
    pub selector: Selector,
    /// The decode targets that fit the receiver's tier, lowest first.
    pub choices: Vec<Choice>,
    pub rewrite: Rewrite,
}

impl View {
    /// The spatial layer of the decode target forwarded now.
    pub fn spatial(&self) -> Option<u8> {
        let current = self.selector.current()?;
        self.choices
            .iter()
            .find(|c| c.target == current)
            .map(|c| c.layer.spatial)
    }
}

/// How far back [`Rewrite`] remembers skipped packets.
const REWRITE_WINDOW: u64 = 2048;

/// One receiver's sequence numbers for one source: the publisher's, from zero,
/// less the packets skipped before them. A packet skipped after a later one was
/// forwarded keeps its number, and the receiver sees it as lost.
#[derive(Default)]
pub struct Rewrite {
    base: Option<u64>,
    highest: Option<u64>,
    skipped: VecDeque<u64>,
    pruned: u64,
}

impl Rewrite {
    pub fn skip(&mut self, seq: u64) {
        self.base.get_or_insert(seq);
        if self.highest.is_some_and(|highest| seq <= highest) {
            return;
        }
        self.highest = Some(seq);
        self.skipped.push_back(seq);
        while self
            .skipped
            .front()
            .is_some_and(|&old| old + REWRITE_WINDOW < seq)
        {
            self.skipped.pop_front();
            self.pruned += 1;
        }
    }

    /// The receiver's number for a forwarded packet; `None` for one from before
    /// the first packet or older than the window.
    pub fn forward(&mut self, seq: u64) -> Option<u64> {
        let base = *self.base.get_or_insert(seq);
        let highest = self.highest.unwrap_or(seq);
        if seq < base || seq + REWRITE_WINDOW < highest {
            return None;
        }
        self.highest = Some(highest.max(seq));
        let skipped = self.pruned + self.skipped.partition_point(|&s| s < seq) as u64;
        Some(seq - base - skipped)
    }
}

/// Every tier of both kinds.
pub fn full_capacity() -> DecodeCapacity {
    DecodeCapacity {
        camera: Kind::Camera.top() as u8,
        screen: Kind::Screen.top() as u8,
    }
}

/// The tier a receiver gets of a source of `kind`.
pub fn tier(kind: Kind, view: &View, decode: DecodeCapacity) -> usize {
    let top = kind.top();
    receive(view.request.unwrap_or(top), decode.get(kind), top, top)
}

/// The descriptor's extension id in the SFU's offers; an offer from a peer
/// keeps its own id.
pub const DESCRIPTOR_ID: u8 = 12;

/// One packet's Dependency Descriptor, as it arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor(pub Vec<u8>);

#[derive(Debug)]
struct DescriptorSerializer;

impl ExtensionSerializer for DescriptorSerializer {
    fn write_to(&self, buf: &mut [u8], ev: &ExtensionValues) -> usize {
        match ev.user_values.get::<Descriptor>() {
            Some(Descriptor(bytes)) if bytes.len() <= buf.len() => {
                buf[..bytes.len()].copy_from_slice(bytes);
                bytes.len()
            }
            _ => 0,
        }
    }

    fn parse_value(&self, buf: &[u8], ev: &mut ExtensionValues) -> bool {
        ev.user_values.set(Descriptor(buf.to_vec()));
        true
    }

    fn is_video(&self) -> bool {
        true
    }

    fn is_audio(&self) -> bool {
        false
    }

    /// A descriptor with its template structure is longer than 16 bytes.
    fn requires_two_byte_form(&self, ev: &ExtensionValues) -> bool {
        ev.user_values
            .get::<Descriptor>()
            .is_some_and(|d| d.0.len() > 16)
    }
}

pub fn descriptor_extension() -> Extension {
    Extension::with_serializer(nuschl_stream_quality::dd::URI, DescriptorSerializer)
}

/// Header extension values for a forwarded packet: the descriptor, if any.
pub fn forwarded(received: &ExtensionValues) -> ExtensionValues {
    let mut values = ExtensionValues::default();
    if let Some(descriptor) = received.user_values.get::<Descriptor>() {
        values.user_values.set(descriptor.clone());
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_packets_leave_no_gap() {
        let mut rewrite = Rewrite::default();
        assert_eq!(rewrite.forward(100), Some(0));
        rewrite.skip(101);
        rewrite.skip(102);
        assert_eq!(rewrite.forward(103), Some(1));
        // A packet lost on the way in keeps its gap; when it arrives late it
        // fills it.
        assert_eq!(rewrite.forward(105), Some(3));
        assert_eq!(rewrite.forward(104), Some(2));
        // A late packet the receiver skips leaves its number unused.
        rewrite.skip(104);
        assert_eq!(rewrite.forward(106), Some(4));
        assert_eq!(rewrite.forward(99), None, "before the first packet");
    }

    #[test]
    fn the_first_packets_may_be_skipped() {
        let mut rewrite = Rewrite::default();
        rewrite.skip(7);
        rewrite.skip(8);
        assert_eq!(rewrite.forward(9), Some(0));
        assert_eq!(rewrite.forward(10), Some(1));
    }

    #[test]
    fn old_skips_are_counted_once_pruned() {
        let mut rewrite = Rewrite::default();
        assert_eq!(rewrite.forward(0), Some(0));
        for seq in (1..10_000).step_by(2) {
            rewrite.skip(seq);
            assert_eq!(rewrite.forward(seq + 1), Some(seq.div_ceil(2)));
        }
        assert!(rewrite.skipped.len() as u64 <= REWRITE_WINDOW);
        assert_eq!(rewrite.forward(1), None, "older than the window");
    }
}
