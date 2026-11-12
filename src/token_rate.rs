//! Per-response output speed, from the provider's own output token count
//! only: never estimated from streamed bytes. A reply whose provider sent
//! no output token count has no speed.
use std::time::{Duration, Instant};
use crate::oak::Event;

#[derive(Default)]
pub struct TokenRate {
    /// When the first and the latest output arrived.
    first: Option<Instant>,
    last: Option<Instant>,
    /// The output token count the provider reported.
    reported: Option<u64>,
}

impl TokenRate {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn observe(&mut self, event: &Event, now: Instant) {
        match event {
            // Retries must not inherit the previous attempt's output or time.
            Event::Start { .. } | Event::Retry { .. } | Event::ThreadRestart(_) => self.reset(),
            // Signatures are not model output.
            Event::Text { text, .. } | Event::Reasoning { text, .. } | Event::Input { json: text, .. } => {
                if !text.is_empty() {
                    self.first.get_or_insert(now);
                    self.last = Some(now);
                }
            }
            // The count is a running total: the last is the largest.
            Event::Usage(u) if u.output > 0 => self.reported = Some(self.reported.unwrap_or(0).max(u.output)),
            _ => {}
        }
    }

    /// Output tokens per second: the provider's count over the time from
    /// the first output to the last (the wait for the first is left out).
    /// None without a reported count, or for output too quick to time.
    pub fn reading(&self) -> Option<f64> {
        let tokens = self.reported?;
        let elapsed = self.last?.saturating_duration_since(self.first?);
        (elapsed >= Duration::from_millis(250)).then(|| tokens as f64 / elapsed.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oak::Usage;

    #[test]
    fn only_the_reported_count_gives_a_speed() {
        let mut rate = TokenRate::default();
        let first = Instant::now();
        rate.observe(&Event::Reasoning { index: 0, text: "x".repeat(700) }, first);
        rate.observe(&Event::Text { index: 1, text: "x".repeat(700) }, first + Duration::from_secs(2));
        rate.observe(&Event::Sig { index: 0, sig: "s".into() }, first + Duration::from_secs(9));
        assert_eq!(rate.reading(), None, "no estimate from bytes");
        rate.observe(&Event::Usage(Usage::default()), first);
        assert_eq!(rate.reading(), None, "a zero count is no count");
        rate.observe(&Event::Usage(Usage { output: 50, ..Default::default() }), first + Duration::from_secs(30));
        assert_eq!(rate.reading(), Some(25.0));
    }
}
