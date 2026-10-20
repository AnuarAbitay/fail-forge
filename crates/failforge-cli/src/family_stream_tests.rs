use super::*;
use std::io::Read;

fn rows(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(|row| {
            assert_eq!(row.last(), Some(&b'\n'));
            serde_json::from_slice(row).unwrap()
        })
        .collect()
}

#[test]
fn frames_are_flat_ordered_and_terminal_hashes_exact_utf8_prefix_with_delimiters() {
    let mut stream = Stream::new(Vec::new());
    stream
        .emit(json!({"kind":"stage_started","stage":"baseline"}))
        .unwrap();
    stream
        .emit(json!({"kind":"baseline","attempt":{"text":"Ж\n😀"}}))
        .unwrap();
    let prefix = stream.writer.clone();
    stream.finish("completed", None, 1, 1).unwrap();
    let events = rows(&stream.writer);
    assert_eq!(events.len(), 3);
    for (index, row) in events.iter().enumerate() {
        assert_eq!(row["version"], 1);
        assert_eq!(row["sequence"], index + 1);
    }
    assert_eq!(events[1]["attempt"]["text"], "Ж\n😀");
    assert_eq!(
        events[2],
        json!({"kind":"terminal","version":1,"sequence":3,"status":"completed",
            "error":null,"completedMutants":1,"expectedMutants":1,"eventCount":2,
            "eventSha256":format!("{:x}",Sha256::digest(&prefix))})
    );
    assert_eq!(stream.count, 2);
    assert_eq!(
        format!("{:x}", stream.hash.clone().finalize()),
        format!("{:x}", Sha256::digest(&prefix))
    );
    let sealed = stream.writer.clone();
    assert!(stream.emit(json!({"kind":"mutant"})).is_err());
    assert!(stream.finish("completed", None, 1, 1).is_err());
    assert_eq!(stream.writer, sealed);
}

#[test]
fn empty_terminal_and_all_event_kinds_have_one_based_sequences() {
    let mut empty = Stream::new(Vec::new());
    empty.finish("no_mutants", None, 0, 0).unwrap();
    let terminal = &rows(&empty.writer)[0];
    assert_eq!(terminal["eventCount"], 0);
    assert_eq!(terminal["sequence"], 1);
    assert_eq!(terminal["eventSha256"], format!("{:x}", Sha256::digest([])));
    for kind in [
        "stage_started",
        "stage_finished",
        "baseline",
        "mutant",
        "original_after",
    ] {
        let mut stream = Stream::new(Vec::new());
        stream.emit(json!({"kind":kind})).unwrap();
        assert_eq!(stream.count, 1);
    }
}

#[test]
fn invalid_shapes_reserved_fields_and_terminal_counts_fail_before_writing() {
    let mut stream = Stream::new(Vec::new());
    for event in [
        json!([]),
        json!(null),
        json!({}),
        json!({"kind":"terminal"}),
        json!({"kind":"other"}),
        json!({"kind":true}),
        json!({"kind":"mutant","sequence":1}),
        json!({"kind":"mutant","version":1}),
        json!({"kind":"mutant","eventCount":1}),
        json!({"kind":"mutant","eventSha256":"forged"}),
    ] {
        assert!(stream.emit(event).is_err());
        assert!(stream.writer.is_empty());
        assert_eq!(stream.count, 0);
    }
    assert!(stream.finish("unknown", None, 0, 0).is_err());
    assert!(stream.finish("completed", None, 2, 1).is_err());
    assert!(stream.writer.is_empty());
    stream.finish("failed", Some("no execution"), 0, 2).unwrap();
    assert_eq!(rows(&stream.writer)[0]["error"], "no execution");
}

#[test]
fn frame_bound_includes_newline_and_rejects_before_exposing_any_prefix() {
    let event = json!({"kind":"mutant","text":"Ж"});
    let raw = serde_json::to_vec(&event).unwrap();
    assert!(encode(&event, raw.len()).is_err());
    let exact = encode(&event, raw.len() + 1).unwrap();
    assert_eq!(&exact[..raw.len()], raw);
    assert_eq!(exact.last(), Some(&b'\n'));
    assert!(encode(&Value::Null, 0).is_err());
    let mut stream = Stream::new(Vec::new());
    stream.limit = 16;
    assert!(stream.emit(event).is_err());
    assert_eq!(stream.count, 0);
    assert!(stream.writer.is_empty());
    stream.limit = MAX_FRAME;
    stream.finish("cancelled", None, 0, 3).unwrap();
}

#[derive(Default)]
struct Sink {
    bytes: Vec<u8>,
    short: usize,
    fail_after: Option<usize>,
    fail_flush: bool,
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self
            .fail_after
            .map_or(usize::MAX, |limit| limit.saturating_sub(self.bytes.len()));
        if remaining == 0 {
            return Err(io::Error::other("injected write failure"));
        }
        let short = if self.short == 0 {
            usize::MAX
        } else {
            self.short
        };
        let count = bytes.len().min(remaining).min(short);
        self.bytes.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush {
            Err(io::Error::other("injected flush failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn short_writes_complete_and_partial_failures_permanently_poison_the_stream() {
    let mut short = Stream::new(Sink {
        short: 2,
        ..Sink::default()
    });
    short.emit(json!({"kind":"mutant","index":7})).unwrap();
    short.finish("completed", None, 1, 1).unwrap();
    assert_eq!(rows(&short.writer.bytes).len(), 2);

    let mut broken = Stream::new(Sink {
        fail_after: Some(5),
        ..Sink::default()
    });
    assert!(broken.emit(json!({"kind":"mutant","index":7})).is_err());
    assert_eq!(broken.writer.bytes.len(), 5);
    assert_eq!(broken.count, 0);
    assert!(broken.finish("failed", Some("write failed"), 0, 1).is_err());
    assert!(broken.emit(json!({"kind":"mutant"})).is_err());
    assert_eq!(broken.writer.bytes.len(), 5);
}

#[test]
fn flush_failure_preserves_emitted_count_but_cannot_authorize_a_terminal() {
    let mut stream = Stream::new(Sink {
        fail_flush: true,
        ..Sink::default()
    });
    assert!(stream.emit(json!({"kind":"baseline"})).is_err());
    assert_eq!(rows(&stream.writer.bytes).len(), 1);
    assert_eq!(stream.count, 1);
    assert_eq!(
        format!("{:x}", stream.hash.clone().finalize()),
        format!("{:x}", Sha256::digest(&stream.writer.bytes))
    );
    assert!(stream.finish("failed", Some("flush failed"), 0, 1).is_err());
    let mut terminal = Stream::new(Sink {
        fail_after: Some(5),
        ..Sink::default()
    });
    assert!(terminal.finish("completed", None, 0, 0).is_err());
    assert!(terminal.finish("failed", None, 0, 0).is_err());
    assert_eq!(terminal.writer.bytes.len(), 5);
}

fn pipe() -> (File, File) {
    let mut descriptors = [-1; 2];
    // SAFETY: pipe initializes exactly two entries in the local array.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: a successful pipe call created both unique descriptors.
    unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            File::from_raw_fd(descriptors[1]),
        )
    }
}

#[test]
fn production_writer_requires_a_write_pipe_and_prevents_capability_inheritance() {
    let (read, write) = pipe();
    assert!(PipeWriter::from_fd(read.as_raw_fd(), WRITE_TIMEOUT).is_err());
    let regular = File::open(std::env::current_exe().unwrap()).unwrap();
    assert!(PipeWriter::from_fd(regular.as_raw_fd(), WRITE_TIMEOUT).is_err());
    assert!(PipeWriter::from_fd(-1, WRITE_TIMEOUT).is_err());
    let writer = PipeWriter::from_fd(write.as_raw_fd(), WRITE_TIMEOUT).unwrap();
    for fd in [write.as_raw_fd(), writer.file.as_raw_fd()] {
        // SAFETY: F_GETFD/F_GETFL only inspect these fixture descriptors.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
    }
}

#[test]
fn vanished_reader_returns_broken_pipe_and_frozen_reader_has_bounded_deadline() {
    let (read, write) = pipe();
    let writer = PipeWriter::from_fd(write.as_raw_fd(), Duration::from_millis(100)).unwrap();
    let mut stream = Stream::new(writer);
    drop(read);
    assert_eq!(
        stream.emit(json!({"kind":"baseline"})).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );

    let (_read, write) = pipe();
    let mut writer = PipeWriter::from_fd(write.as_raw_fd(), Duration::from_millis(100)).unwrap();
    let started = Instant::now();
    assert_eq!(
        writer
            .write_frame(&vec![0; 8 * 1024 * 1024])
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn real_pipe_retries_after_backpressure_and_preserves_every_frame_byte() {
    let (mut read, write) = pipe();
    let writer = PipeWriter::from_fd(write.as_raw_fd(), Duration::from_secs(10)).unwrap();
    drop(write);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        read.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let mut stream = Stream::new(writer);
    let value = json!({"kind":"mutant","record":{"text":"Ж".repeat(512*1024)}});
    stream.emit(value.clone()).unwrap();
    stream.finish("completed", None, 1, 1).unwrap();
    drop(stream);
    let bytes = reader.join().unwrap();
    let events = rows(&bytes);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["record"], value["record"]);
    let split = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    assert_eq!(
        events[1]["eventSha256"],
        format!("{:x}", Sha256::digest(&bytes[..split]))
    );
}
