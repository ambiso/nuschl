// SPDX-License-Identifier: EUPL-1.2
//! The egress budget per period (docs/video-plan.md §5.2, #443). The SFU counts
//! the bytes it sends, and the count survives restarts in a file in its state
//! directory. The paced share is the budget times the elapsed fraction of the
//! period. At or below it the normal caps apply, so long bursts run at full
//! quality; every 1/[`STEP`] of the budget above it lowers every receiver's
//! tier by one, and past the lowest tier receivers get base layers only. At the
//! budget the SFU refuses new video.

use jiff::{Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use serde::Deserialize;
use std::path::PathBuf;

/// A step is 1/STEP of the budget above the paced share.
pub const STEP: u64 = 100;

/// A billing period, in UTC.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Month,
}

impl Period {
    /// The period around `now`, in Unix seconds.
    fn bounds(self, now: i64) -> (i64, i64) {
        let date = Timestamp::from_second(now)
            .unwrap_or(Timestamp::UNIX_EPOCH)
            .to_zoned(TimeZone::UTC)
            .date();
        let start = match self {
            Self::Day => date,
            Self::Month => date.first_of_month(),
        };
        let end = match self {
            Self::Day => start.checked_add(1.day()),
            Self::Month => start.checked_add(1.month()),
        };
        let second = |date: Date| {
            date.to_zoned(TimeZone::UTC)
                .map(|z| z.timestamp().as_second())
        };
        match (second(start), end.map(second)) {
            (Ok(start), Ok(Ok(end))) => (start, end),
            _ => (now, now + 86_400),
        }
    }
}

pub struct Traffic {
    budget: u64,
    period: Period,
    path: PathBuf,
    start: i64,
    end: i64,
    sent: u64,
}

impl Traffic {
    /// Reads the count of the current period from `path` (`start sent`), or
    /// starts at zero when the file is missing or from an earlier period.
    pub fn load(path: PathBuf, budget: u64, period: Period, now: i64) -> Result<Self, String> {
        let (start, end) = period.bounds(now);
        let sent = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut fields = text.split_whitespace().map(str::parse::<i64>);
                match (fields.next(), fields.next(), fields.next()) {
                    (Some(Ok(saved)), Some(Ok(sent)), None) if sent >= 0 => {
                        if saved == start {
                            sent as u64
                        } else {
                            0
                        }
                    }
                    _ => return Err(format!("{}: unreadable traffic count", path.display())),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        Ok(Self {
            budget,
            period,
            path,
            start,
            end,
            sent,
        })
    }

    pub fn add(&mut self, bytes: usize) {
        self.sent = self.sent.saturating_add(bytes as u64);
    }

    pub fn sent(&self) -> u64 {
        self.sent
    }

    pub fn exhausted(&self) -> bool {
        self.sent >= self.budget
    }

    /// Starts a new period once this one ended, and returns how many steps the
    /// count is above the paced share.
    pub fn step(&mut self, now: i64) -> u64 {
        if now >= self.end {
            (self.start, self.end) = self.period.bounds(now);
            self.sent = 0;
        }
        let elapsed = (now - self.start).clamp(0, self.end - self.start) as u128;
        let paced = u128::from(self.budget) * elapsed / (self.end - self.start) as u128;
        let over = u128::from(self.sent).saturating_sub(paced);
        over.div_ceil(u128::from(self.budget / STEP).max(1)) as u64
    }

    /// Writes the count atomically.
    pub fn save(&self) -> std::io::Result<()> {
        let partial = self.path.with_extension("partial");
        std::fs::write(&partial, format!("{} {}\n", self.start, self.sent))?;
        std::fs::rename(&partial, &self.path)
    }
}

#[cfg(test)]
mod tests;
