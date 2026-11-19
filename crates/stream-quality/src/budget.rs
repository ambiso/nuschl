// SPDX-License-Identifier: EUPL-1.2
//! Sharing forwarded bandwidth (§5.2): the host's egress, each room's and each
//! receiver's. A full budget is shared max-min by dropping layers, and a join
//! fails only when the base layers do not fit.

use crate::adapt::step_up_cost;
use std::{cmp::Reverse, collections::BinaryHeap};

/// Egress limits in bits per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub host: u64,
    pub room: u64,
    pub receiver: u64,
}

impl Limits {
    /// The project's limits on a host with `upload` bits per second: 70 % of it,
    /// 150 Mbit/s per room and 25 Mbit/s per receiver.
    pub fn project(upload: u64) -> Self {
        Self {
            host: upload / 10 * 7,
            room: 150_000_000,
            receiver: 25_000_000,
        }
    }
}

/// One source forwarded to one receiver.
#[derive(Clone, Copy, Debug)]
pub struct Stream<'a> {
    /// An index of the caller's rooms.
    pub room: usize,
    /// An index into the receivers' capacities.
    pub receiver: usize,
    /// The rate of each layer choice, lowest first, each including the layers
    /// below it ([`crate::dd::Layers::choices`]).
    pub rates: &'a [u64],
    /// Choices forwarded now: the base and these cost their rate, higher
    /// choices [`step_up_cost`].
    pub current: usize,
}

impl Stream<'_> {
    fn cost(&self, choices: usize) -> u64 {
        match choices {
            0 => 0,
            n if n <= self.current.max(1) => self.rates[n - 1],
            n => step_up_cost(self.rates[n - 1]),
        }
    }
}

/// The number of choices each stream gets, 0 for none. Rounds give the stream
/// with the lowest rate its next choice while that fits the host's, its room's
/// and its receiver's budget; a receiver's budget is the least of
/// `limits.receiver` and `receivers[stream.receiver]`, its network capacity.
pub fn allocate(limits: &Limits, receivers: &[u64], streams: &[Stream]) -> Vec<usize> {
    let rooms = streams.iter().map(|s| s.room + 1).max().unwrap_or(0);
    let mut room_used = vec![0; rooms];
    let mut receiver_used = vec![0; receivers.len()];
    let mut host_used = 0;
    let mut choices = vec![0; streams.len()];
    let mut queue: BinaryHeap<_> = (0..streams.len())
        .filter(|&i| !streams[i].rates.is_empty())
        .map(|i| Reverse((0, i)))
        .collect();
    while let Some(Reverse((_, i))) = queue.pop() {
        let stream = &streams[i];
        let extra = stream
            .cost(choices[i] + 1)
            .saturating_sub(stream.cost(choices[i]));
        let fits = host_used + extra <= limits.host
            && room_used[stream.room] + extra <= limits.room
            && receiver_used[stream.receiver] + extra
                <= limits.receiver.min(receivers[stream.receiver]);
        if !fits {
            continue;
        }
        host_used += extra;
        room_used[stream.room] += extra;
        receiver_used[stream.receiver] += extra;
        choices[i] += 1;
        if choices[i] < stream.rates.len() {
            queue.push(Reverse((stream.rates[choices[i] - 1], i)));
        }
    }
    choices
}

/// Whether all base layers fit the configured egress budgets.
pub fn admits(limits: &Limits, receivers: usize, streams: &[Stream]) -> bool {
    let rooms = streams.iter().map(|s| s.room + 1).max().unwrap_or(0);
    let mut room_left = vec![limits.room; rooms];
    let mut receiver_left = vec![limits.receiver; receivers];
    let mut host_left = limits.host;
    streams.iter().all(|stream| {
        let rate = stream.rates.first().copied().unwrap_or(0);
        let room = &mut room_left[stream.room];
        let receiver = &mut receiver_left[stream.receiver];
        if rate > host_left.min(*room).min(*receiver) {
            return false;
        }
        host_left -= rate;
        *room -= rate;
        *receiver -= rate;
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYERS: [u64; 3] = [100, 300, 600];

    fn stream(room: usize, receiver: usize, current: usize) -> Stream<'static> {
        Stream {
            room,
            receiver,
            rates: &LAYERS,
            current,
        }
    }

    fn wide(host: u64, room: u64) -> Limits {
        Limits {
            host,
            room,
            receiver: u64::MAX,
        }
    }

    #[test]
    fn project_limits() {
        assert_eq!(
            Limits::project(1_000_000_000),
            Limits {
                host: 700_000_000,
                room: 150_000_000,
                receiver: 25_000_000
            }
        );
    }

    #[test]
    fn a_receiver_budget_is_shared_max_min() {
        // Three streams to one receiver with 1000: everyone gets the middle
        // layer before anyone gets the top.
        let streams = [stream(0, 0, 3), stream(0, 0, 3), stream(0, 0, 3)];
        assert_eq!(
            allocate(&wide(u64::MAX, u64::MAX), &[1000], &streams),
            [2, 2, 2]
        );
        // With 1400 one of them gets the top.
        assert_eq!(
            allocate(&wide(u64::MAX, u64::MAX), &[1400], &streams),
            [3, 2, 2]
        );
        // The receiver limit applies with a larger network capacity.
        let limits = Limits {
            receiver: 300,
            ..wide(u64::MAX, u64::MAX)
        };
        assert_eq!(allocate(&limits, &[u64::MAX], &streams[..1]), [2]);
    }

    #[test]
    fn rooms_and_the_host_are_shared_across_receivers() {
        // Two receivers in one room capped at 700: 300 + 300, not 600 + 100.
        let streams = [stream(0, 0, 3), stream(0, 1, 3), stream(1, 2, 3)];
        let receivers = [u64::MAX; 3];
        assert_eq!(
            allocate(&wide(u64::MAX, 700), &receivers, &streams),
            [2, 2, 3]
        );
        // The host caps both rooms together.
        assert_eq!(allocate(&wide(900, 700), &receivers, &streams), [2, 2, 2]);
        // A smaller later layer still fits after a larger one does not.
        let small = [100, 120];
        let streams = [
            stream(0, 0, 3),
            Stream {
                rates: &small,
                ..stream(0, 1, 2)
            },
        ];
        assert_eq!(
            allocate(&wide(330, u64::MAX), &[u64::MAX; 2], &streams),
            [1, 2]
        );
    }

    #[test]
    fn layers_above_the_current_need_headroom() {
        // Holding 300 needs 300; reaching it from the base needs 390.
        assert_eq!(
            allocate(&wide(300, 300), &[u64::MAX], &[stream(0, 0, 2)]),
            [2]
        );
        assert_eq!(
            allocate(&wide(389, 389), &[u64::MAX], &[stream(0, 0, 1)]),
            [1]
        );
        assert_eq!(
            allocate(&wide(390, 390), &[u64::MAX], &[stream(0, 0, 1)]),
            [2]
        );
        // The base layer costs its rate also for a new stream.
        assert_eq!(
            allocate(&wide(100, 100), &[u64::MAX], &[stream(0, 0, 0)]),
            [1]
        );
    }

    #[test]
    fn a_join_fails_only_when_base_layers_do_not_fit() {
        let limits = wide(u64::MAX, 250);
        let empty = Stream {
            rates: &[],
            ..stream(0, 2, 0)
        };
        // Two bases fit, upper layers get dropped.
        let two = [stream(0, 0, 3), stream(0, 1, 0), empty];
        assert!(admits(&limits, 3, &two));
        assert_eq!(allocate(&limits, &[u64::MAX; 3], &two), [1, 1, 0]);
        // A third base does not.
        let three = [stream(0, 0, 3), stream(0, 1, 0), stream(0, 2, 0)];
        assert!(!admits(&limits, 3, &three));
        // Host and per-receiver caps also apply during admission.
        assert!(!admits(&wide(150, u64::MAX), 3, &two));
        assert!(!admits(
            &Limits {
                receiver: 50,
                ..limits
            },
            3,
            &two
        ));
        assert_eq!(allocate(&limits, &[50], &three[..1]), [0]);
        assert!(admits(&limits, 1, &three[..1]));
    }
}
