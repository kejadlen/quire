use std::io::{BufRead, Write};
use std::os::unix::net::UnixListener;

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;

fn cmd() -> Command {
    Command::from(cargo_bin_cmd!("quire"))
}

#[test]
fn shows_help() {
    cmd().arg("--help").assert().success();
}

#[test]
fn shows_version() {
    cmd().arg("--version").assert().success();
}

/// Test that a push event round-trips through a Unix socket.
#[test]
fn push_event_round_trips_through_socket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("server.sock");

    let listener = UnixListener::bind(&socket_path).expect("bind");

    // Simulate what the hook would send.
    let event = quire::event::PushEvent {
        r#type: "push".to_string(),
        repo: "test.git".to_string(),
        pushed_at: "12345".to_string(),
        refs: vec![quire::event::PushRef {
            old_sha: "0000000000000000000000000000000000000000".to_string(),
            new_sha: "abc123".to_string(),
            r#ref: "refs/heads/main".to_string(),
        }],
    };

    let mut line = serde_json::to_string(&event).expect("serialize");
    line.push('\n');

    // Write from a client socket in a separate thread.
    let path_clone = socket_path.clone();
    let line_clone = line.clone();
    let writer_handle = std::thread::spawn(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(&path_clone).expect("connect");
        stream.write_all(line_clone.as_bytes()).expect("write");
    });

    let (client, _) = listener.accept().expect("accept");

    // Read on the server side.
    let mut buf = String::new();
    let mut reader = std::io::BufReader::new(client);
    reader.read_line(&mut buf).expect("read line");

    writer_handle.join().expect("writer thread");

    let parsed: quire::event::PushEvent = serde_json::from_str(&buf).expect("deserialize");

    assert_eq!(parsed.r#type, "push");
    assert_eq!(parsed.repo, "test.git");
    assert_eq!(parsed.refs.len(), 1);
    assert_eq!(parsed.refs[0].r#ref, "refs/heads/main");
    assert_eq!(parsed.refs[0].new_sha, "abc123");
}

/// Test that multiple ref updates round-trip correctly.
#[test]
fn push_event_multiple_refs_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("server.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind");

    let event = quire::event::PushEvent {
        r#type: "push".to_string(),
        repo: "work/project.git".to_string(),
        pushed_at: "99999".to_string(),
        refs: vec![
            quire::event::PushRef {
                old_sha: "aaa".to_string(),
                new_sha: "bbb".to_string(),
                r#ref: "refs/heads/main".to_string(),
            },
            quire::event::PushRef {
                old_sha: "ccc".to_string(),
                new_sha: "0000000000000000000000000000000000000000".to_string(),
                r#ref: "refs/heads/feature".to_string(),
            },
        ],
    };

    let mut line = serde_json::to_string(&event).expect("serialize");
    line.push('\n');

    let path_clone = socket_path.clone();
    let line_clone = line.clone();
    let writer_handle = std::thread::spawn(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(&path_clone).expect("connect");
        stream.write_all(line_clone.as_bytes()).expect("write");
    });

    let (client, _) = listener.accept().expect("accept");
    let mut buf = String::new();
    let mut reader = std::io::BufReader::new(client);
    reader.read_line(&mut buf).expect("read line");
    writer_handle.join().expect("writer thread");

    let parsed: quire::event::PushEvent = serde_json::from_str(&buf).expect("deserialize");

    assert_eq!(parsed.refs.len(), 2);
    assert_eq!(parsed.refs[0].r#ref, "refs/heads/main");
    assert_eq!(parsed.refs[1].r#ref, "refs/heads/feature");
    // Deletion ref included in event (server decides how to handle).
    assert_eq!(
        parsed.refs[1].new_sha,
        "0000000000000000000000000000000000000000"
    );
}
