//! Event sinks consume the structured stream emitted during a run.
//!
//! [`NullSink`] drops events on the floor and is the default for
//! standalone `quire-ci run`. [`JsonlSink`] writes one JSON object per
//! line to any [`Write`], flushing per event so a crash mid-run leaves
//! a usable artifact.

use std::io::{self, Write};

use crate::event::Event;

/// A consumer of pipeline events.
pub trait EventSink {
    fn emit(&mut self, event: Event) -> io::Result<()>;
}

/// Drops every event. Default for `quire-ci run` standalone use.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event) -> io::Result<()> {
        Ok(())
    }
}

/// Writes events as JSON Lines (one object per line) to a writer,
/// flushing after each event. Generic over the writer so production
/// code uses [`io::Stdout`] and tests use a `Vec<u8>`.
pub struct JsonlSink<W: Write> {
    writer: W,
}

impl<W: Write> JsonlSink<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Recover the underlying writer (used by tests to inspect output).
    #[cfg(test)]
    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: Write> EventSink for JsonlSink<W> {
    fn emit(&mut self, event: Event) -> io::Result<()> {
        serde_json::to_writer(&mut self.writer, &event)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::EventKind;

    fn sample_started(job_id: &str) -> Event {
        Event {
            at_ms: 1,
            kind: EventKind::JobStarted {
                job_id: job_id.into(),
            },
        }
    }

    #[test]
    fn null_sink_accepts_events() {
        let mut sink = NullSink;
        sink.emit(sample_started("a")).unwrap();
    }

    #[test]
    fn jsonl_sink_writes_one_line_per_event() {
        let mut sink = JsonlSink::new(Vec::<u8>::new());
        sink.emit(Event {
            at_ms: 10,
            kind: EventKind::ShStarted {
                job_id: "a".into(),
                cmd: "echo".into(),
            },
        })
        .unwrap();
        sink.emit(sample_started("a")).unwrap();
        let output = sink.into_inner();
        let s = std::str::from_utf8(&output).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""type":"sh_started""#));
        assert!(lines[1].contains(r#""type":"job_started""#));
    }

    #[test]
    fn jsonl_sink_flushes_per_event() {
        struct CountingWriter {
            buf: Vec<u8>,
            flushes: usize,
        }
        impl Write for CountingWriter {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.buf.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }

        let mut sink = JsonlSink::new(CountingWriter {
            buf: Vec::new(),
            flushes: 0,
        });
        sink.emit(sample_started("a")).unwrap();
        sink.emit(sample_started("b")).unwrap();
        let inner = sink.into_inner();
        assert_eq!(inner.flushes, 2);
    }
}
