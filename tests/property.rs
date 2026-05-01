use hegel::TestCase;
use hegel::generators::{integers, text, vecs};
use quire::event::{PushEvent, PushRef};

#[hegel::composite]
fn push_ref(tc: TestCase) -> PushRef {
    PushRef {
        r#ref: tc.draw(text()),
        old_sha: tc.draw(text()),
        new_sha: tc.draw(text()),
    }
}

#[hegel::composite]
fn push_event(tc: TestCase) -> PushEvent {
    // jiff::Timestamp range: -377705023201..=253402207200 seconds.
    let secs = tc.draw(
        integers::<i64>()
            .min_value(-377_705_023_201)
            .max_value(253_402_207_200),
    );
    PushEvent {
        r#type: tc.draw(text()),
        repo: tc.draw(text()),
        pushed_at: jiff::Timestamp::from_second(secs).expect("timestamp in range"),
        refs: tc.draw(vecs(push_ref()).max_size(8)),
    }
}

#[hegel::test]
fn push_event_round_trips_json(tc: TestCase) {
    let event = tc.draw(push_event());
    let json = serde_json::to_string(&event).expect("serialize");
    let parsed: PushEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(event, parsed);
}
