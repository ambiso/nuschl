// SPDX-License-Identifier: EUPL-1.2
use super::*;

/// 2026-11-01, 2026-11-18, 2026-11-19 and 2026-12-01, 00:00 UTC.
const NOVEMBER: i64 = 1_793_491_200;
const NOVEMBER_18: i64 = 1_794_960_000;
const NOVEMBER_19: i64 = 1_795_046_400;
const DECEMBER: i64 = 1_796_083_200;

fn traffic(directory: &nuschl_testkit::TempDir, budget: u64, now: i64) -> Traffic {
    Traffic::load(directory.path().join("traffic"), budget, Period::Month, now).expect("load")
}

#[test]
fn periods_are_utc_months_and_days() {
    assert_eq!(Period::Month.bounds(NOVEMBER_18 + 5), (NOVEMBER, DECEMBER));
    assert_eq!(Period::Month.bounds(NOVEMBER), (NOVEMBER, DECEMBER));
    assert_eq!(
        Period::Day.bounds(NOVEMBER_18 + 5),
        (NOVEMBER_18, NOVEMBER_19)
    );
}

#[test]
fn steps_follow_the_excess_over_the_paced_share() {
    let directory = nuschl_testkit::temp_dir();
    let mut traffic = traffic(&directory, 30_000, NOVEMBER);
    // A burst can use the share already accrued.
    let middle = NOVEMBER + (DECEMBER - NOVEMBER) / 2;
    traffic.add(15_000);
    assert_eq!(
        traffic.step(NOVEMBER + 1),
        50,
        "far above the share early on"
    );
    assert_eq!(traffic.step(middle), 0, "at the paced share");
    // Each step is 1 % of the budget, 300 bytes.
    traffic.add(1);
    assert_eq!(traffic.step(middle), 1);
    traffic.add(299);
    assert_eq!(traffic.step(middle), 1);
    traffic.add(1);
    assert_eq!(traffic.step(middle), 2);
    assert!(!traffic.exhausted());
    traffic.add(14_699);
    assert!(traffic.exhausted(), "at the budget");
    // A new month starts at zero.
    assert_eq!(traffic.step(DECEMBER), 0);
    assert_eq!(traffic.sent(), 0);
    assert!(!traffic.exhausted());
}

#[test]
fn accrued_monthly_share_covers_hours_of_above_average_traffic() {
    let directory = nuschl_testkit::temp_dir();
    let path = directory.path().join("traffic");
    std::fs::write(path, format!("{NOVEMBER} 9800000000000\n")).expect("save");
    let middle = NOVEMBER + (DECEMBER - NOVEMBER) / 2;
    let mut traffic = traffic(&directory, 20_000_000_000_000, middle);

    // Send 160 Mbit/s, above the month's average of about 62 Mbit/s.
    for minute in 1..=360 {
        traffic.add(1_200_000_000);
        let step = traffic.step(middle + minute * 60);
        if minute <= 240 {
            assert_eq!(step, 0, "minute {minute}");
        }
    }
    assert_eq!(traffic.step(middle + 6 * 3_600), 1);
    assert_eq!(traffic.step(middle + 9 * 3_600), 0);
}

#[test]
fn the_count_survives_a_restart_within_its_period() {
    let directory = nuschl_testkit::temp_dir();
    let mut before = traffic(&directory, 1_000, NOVEMBER_18);
    before.add(700);
    before.save().expect("save");
    assert_eq!(traffic(&directory, 1_000, NOVEMBER_19).sent(), 700);
    assert_eq!(
        traffic(&directory, 1_000, DECEMBER).sent(),
        0,
        "a new period"
    );

    std::fs::write(directory.path().join("traffic"), "garbage\n").expect("write");
    let path = directory.path().join("traffic");
    assert!(Traffic::load(path, 1_000, Period::Month, NOVEMBER_18).is_err());
}
