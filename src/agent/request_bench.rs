//! Opt-in request-building benchmarks.

use super::*;

/// What building a request's messages costs (with the conversation
/// locked) for a 300- and a 3000-turn conversation, before and after a
/// cold moment cleared its old tool output. Run on its own:
/// `cargo test --release bench_request_messages -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_request_messages() {
    for turns in [300usize, 3000] {
        let mut c: Conversation = serde_json::from_value(crate::frame_bench::long_conversation("1", turns)).unwrap();
        for cold in [false, true] {
            if cold {
                let Conversation { rolling, messages, .. } = &mut c;
                assert!(rolling.clear_cold(messages));
            }
            let mut v: Vec<f64> = (0..9)
                .map(|_| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(request_messages(&c, &c.messages));
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            v.sort_by(f64::total_cmp);
            println!("{turns} turns{}: {:.2} ms (median of 9)", if cold { ", cold-cleared" } else { "" }, v[4]);
        }
    }
}
