use hegel::TestCase;
use hegel::generators::{integers, just, text, vecs};
use hegel::one_of;
use quire::event::{PushEvent, PushRef};
use quire::secret::SecretString;

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

#[hegel::composite]
fn push_ref(tc: TestCase) -> PushRef {
    PushRef {
        r#ref: tc.draw(text()),
        old_sha: tc.draw(text()),
        // Mix in zero-shas so updated_refs() actually exercises its filter.
        new_sha: tc.draw(one_of![text(), just(ZERO_SHA.to_string())]),
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

#[hegel::test]
fn updated_refs_excludes_only_zero_sha_deletions(tc: TestCase) {
    let event = tc.draw(push_event());
    let kept = event.updated_refs();

    let expected: Vec<&PushRef> = event
        .refs
        .iter()
        .filter(|r| r.new_sha != ZERO_SHA)
        .collect();
    assert_eq!(kept, expected);

    let deleted = event.refs.iter().filter(|r| r.new_sha == ZERO_SHA).count();
    assert_eq!(kept.len() + deleted, event.refs.len());
}

#[hegel::test]
fn secret_string_debug_never_leaks_plain_value(tc: TestCase) {
    let value = tc.draw(text());
    let secret = SecretString::from_plain(value.clone());
    let debug = format!("{secret:?}");
    assert_eq!(debug, "SecretString(\"<redacted>\")");
}

#[hegel::test]
fn secret_string_plain_json_round_trips(tc: TestCase) {
    let value = tc.draw(text());
    let json = serde_json::to_string(&value).expect("serialize string");
    let secret: SecretString = serde_json::from_str(&json).expect("deserialize SecretString");
    assert_eq!(secret.reveal().expect("plain reveal"), value);
}

#[hegel::test]
fn secret_string_from_file_strips_one_trailing_newline(tc: TestCase) {
    let content = tc.draw(text());
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("secret");
    fs_err::write(&path, &content).expect("write");

    let revealed = SecretString::from_file(&path)
        .reveal()
        .expect("reveal")
        .to_string();
    let expected = content.strip_suffix('\n').unwrap_or(&content).to_string();
    assert_eq!(revealed, expected);
}
