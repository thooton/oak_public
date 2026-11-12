//! Dates and times as Oak shows them (`oak_sdk::time`, which plugins use
//! too); the tests are here.

pub use oak_sdk::time::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_times_in_words_and_in_full() {
        let (min, hour, day) = (60_000, 3_600_000, 86_400_000);
        // 2026-10-31 12:00 UTC, read in UTC.
        let now = 1_793_448_000_000;
        assert_eq!(time_ago(now - 5_000, now, 0), "just now");
        assert_eq!(time_ago(now + 5_000, now, 0), "just now");
        assert_eq!(time_ago(now - min, now, 0), "1 minute ago");
        assert_eq!(time_ago(now - 59 * min, now, 0), "59 minutes ago");
        assert_eq!(time_ago(now - 3 * hour, now, 0), "3 hours ago");
        assert_eq!(time_ago(now - 25 * hour, now, 0), "yesterday");
        assert_eq!(time_ago(now - 3 * day, now, 0), "3 days ago");
        assert_eq!(time_ago(now - 6 * day, now, 0), "6 days ago");
        assert_eq!(time_ago(now - 7 * day, now, 0), "Oct 24");
        assert_eq!(time_ago(now - 400 * day, now, 0), "Sep 26, 2025");
        // Days go by the local calendar: 36 hours back, 23:00 two days
        // ago in UTC, is yesterday 11 hours ahead.
        assert_eq!(time_ago(now - 36 * hour, now, 0), "yesterday");
        assert_eq!(time_ago(now - 37 * hour, now, 0), "2 days ago");
        assert_eq!(time_ago(now - 37 * hour, now, 11 * hour), "yesterday");
        assert_eq!(full_time(now, 0), "Oct 31, 2026, 12:00 PM");
        assert_eq!(full_time(now - 3 * day - 3 * hour - 26 * min, -hour), "Oct 28, 2026, 7:34 AM");
        assert_eq!(full_time(now - 12 * hour, 0), "Oct 31, 2026, 12:00 AM");
        assert_eq!(full_time(951_825_600_000, 0), "Feb 29, 2000, 12:00 PM");
        assert_eq!(day_clock(now - 3 * hour - 26 * min, now, 0), "Today, 8:34 AM");
        assert_eq!(day_clock(now - 13 * hour, now, 0), "Yesterday, 11:00 PM");
        assert_eq!(day_clock(now - 7 * day, now, 0), "Oct 24, 12:00 PM");
        assert_eq!(day_clock(now - 400 * day, now, 0), "Sep 26, 2025, 12:00 PM");
    }
}
