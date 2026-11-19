// SPDX-License-Identifier: EUPL-1.2
//! Per-receiver video quality (docs/video-plan.md §5) on `nuschl-stream-quality`.
//!
//! The AV1 Dependency Descriptor travels through str0m as a user header
//! extension on both legs, as the publisher wrote it.
//!
//! A receiver gets the frames of one decode target per layered source from
//! Q1's `Selector`, which switches at switch points, with sequence numbers of
//! its own ([`Rewrite`]) so intentional layer skips leave no gaps. The queue
//! budget sheds pictures when a receiver falls behind.
//!
//! A receiver asks for a tier per source (`QualityRequest`) and states the
//! tiers it decodes (`DecodeCapacity`), each at most once a second. The tier it
//! gets is the least of both; until it asks, it asks for the top tier. Once a
//! second the publisher hears the resulting demand for each source.

use nuschl_stream_quality::{
    DecodeCapacity, Kind, Throttle,
    adapt::Stepper,
    dd::{Choice, Layers, Selector},
    receive,
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues, StreamTxQueueInfo};

const QUEUE_AGE: Duration = Duration::from_millis(300);
const MIN_QUEUE_BYTES: u64 = 32 * 1024;
const MAX_QUEUE_BYTES: u64 = 2 * 1024 * 1024;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(10);

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
    /// Paces moves toward the receiver's share of the budgets.
    pub stepper: Option<Stepper>,
    /// The decode targets that fit the receiver's tier, lowest first.
    pub choices: Vec<Choice>,
    pub rewrite: Rewrite,
    pub backlog: Backlog,
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

    pub fn shed(&mut self, timestamp: u64) {
        self.backlog.shed(timestamp);
        // Re-enter the decode target at a switch point after dropping a picture.
        let target = self.selector.current();
        self.selector = Selector::default();
        self.selector.want(target);
    }
}

/// The video admitted to one receiver's send queue and its temporary quality limit.
#[derive(Default)]
pub struct Backlog {
    dropped_through: Option<u64>,
    sampled_at: Option<Instant>,
    unsampled_bytes: usize,
    overloaded: bool,
    steps: usize,
    recover_at: Option<Instant>,
}

impl Backlog {
    pub fn dropped(&self, timestamp: u64) -> bool {
        self.dropped_through.is_some_and(|last| timestamp <= last)
    }

    fn shed(&mut self, timestamp: u64) {
        self.dropped_through = Some(timestamp.max(self.dropped_through.unwrap_or(0)));
        self.overloaded();
    }

    pub fn overloaded(&mut self) {
        self.overloaded = true;
    }

    pub fn full(
        &mut self,
        queue: Option<&dyn StreamTxQueueInfo>,
        bitrate: u64,
        payload: usize,
        now: Instant,
    ) -> bool {
        let sampled_at = queue.map(StreamTxQueueInfo::created_at);
        if self.sampled_at != sampled_at {
            self.sampled_at = sampled_at;
            self.unsampled_bytes = 0;
        }
        let limit = (bitrate / 8 * 3 / 10).clamp(MIN_QUEUE_BYTES, MAX_QUEUE_BYTES);
        let bytes = queue.map_or(0, StreamTxQueueInfo::byte_size);
        let oldest = queue.and_then(StreamTxQueueInfo::first_unsent);
        bytes
            .saturating_add(self.unsampled_bytes)
            .saturating_add(payload) as u64
            > limit
            || oldest.is_some_and(|at| now.saturating_duration_since(at) >= QUEUE_AGE)
    }

    pub fn enqueued(&mut self, payload: usize) {
        self.unsampled_bytes += payload;
    }

    pub fn tick(&mut self, now: Instant) {
        if std::mem::take(&mut self.overloaded) {
            self.steps = (self.steps + 1).min(Kind::Camera.top().max(Kind::Screen.top()));
            self.recover_at = Some(now + RECOVERY_INTERVAL);
        } else if self.recover_at.is_some_and(|at| now >= at) {
            self.steps = self.steps.saturating_sub(1);
            self.recover_at = (self.steps > 0).then_some(now + RECOVERY_INTERVAL);
        }
    }

    pub fn tier(&self, tier: usize) -> usize {
        tier.saturating_sub(self.steps)
    }

    pub fn limited(&self) -> bool {
        self.steps > 0
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

/// The tier a receiver gets of a source of `kind`, with the caps lowered by
/// `step` tiers (`traffic`).
pub fn tier(kind: Kind, view: &View, decode: DecodeCapacity, step: u64) -> usize {
    let top = kind.top();
    let policy = top.saturating_sub(usize::try_from(step).unwrap_or(usize::MAX));
    receive(view.request.unwrap_or(top), decode.get(kind), top, policy)
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

    struct Queue {
        oldest: Instant,
        bytes: usize,
    }

    impl StreamTxQueueInfo for Queue {
        fn created_at(&self) -> Instant {
            self.oldest
        }

        fn byte_size(&self) -> usize {
            self.bytes
        }

        fn packet_count(&self) -> usize {
            1
        }

        fn first_unsent(&self) -> Option<Instant> {
            Some(self.oldest)
        }
    }

    #[test]
    fn video_queue_bounds_bytes_and_age() {
        let now = Instant::now();
        let mut backlog = Backlog::default();
        let sample = Queue {
            oldest: now,
            bytes: 74_000,
        };
        assert!(!backlog.full(Some(&sample), 2_000_000, 1000, now));
        backlog.enqueued(1000);
        assert!(backlog.full(Some(&sample), 2_000_000, 1, now));
        assert!(backlog.full(Some(&sample), 15_000_000, 1000, now + QUEUE_AGE));
        assert!(!backlog.full(None, 2_000_000, 1000, now));
    }

    #[test]
    fn video_shedding_covers_the_whole_remainder_and_recovers_slowly() {
        let now = Instant::now();
        let mut backlog = Backlog::default();
        backlog.shed(12);
        assert!(backlog.dropped(11));
        assert!(backlog.dropped(12));
        assert!(!backlog.dropped(13));
        backlog.tick(now);
        assert_eq!(backlog.tier(3), 2);
        backlog.shed(14);
        let latest = now + Duration::from_secs(1);
        backlog.tick(latest);
        assert_eq!(backlog.tier(3), 1);
        backlog.tick(now + RECOVERY_INTERVAL);
        assert_eq!(backlog.tier(3), 1);
        backlog.tick(latest + RECOVERY_INTERVAL);
        assert_eq!(backlog.tier(3), 2);
        backlog.tick(latest + RECOVERY_INTERVAL * 2);
        assert_eq!(backlog.tier(3), 3);
    }

    #[test]
    fn traffic_steps_lower_the_tier() {
        let view = View::default();
        let full = full_capacity();
        let tiers: Vec<usize> = (0..5)
            .map(|step| tier(Kind::Camera, &view, full, step))
            .collect();
        assert_eq!(tiers, [3, 2, 1, 0, 0]);
        let asked = View {
            request: Some(1),
            ..View::default()
        };
        assert_eq!(tier(Kind::Screen, &asked, full, 1), 1, "the least of both");
    }

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
