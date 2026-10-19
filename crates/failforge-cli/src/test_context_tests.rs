use super::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const VALID: &[u8] =
    b"{\"expected\":null,\"baseline\":null,\"mutant\":null,\"alias\":\"subject\"}\n";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ff-context-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str, raw: &[u8], anonymous: bool) -> File {
        let path = self.0.join(name);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(raw).unwrap();
        file.rewind().unwrap();
        if anonymous {
            std::fs::remove_file(path).unwrap();
        }
        file
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read(file: &File) -> io::Result<Option<SelectionContext>> {
    from_value(Some(OsStr::new(&file.as_raw_fd().to_string())), true)
}

#[test]
fn context_requires_an_evidence_owner_and_a_private_descriptor() {
    assert!(from_value(None, false).unwrap().is_none());
    for value in [
        "",
        "0",
        "1",
        "2",
        "-3",
        "+3",
        " 3",
        "3 ",
        "999999999999999999999999",
    ] {
        assert!(from_value(Some(OsStr::new(value)), true).is_err());
    }
    let fixture = Fixture::new();
    let file = fixture.file("valid", VALID, true);
    assert!(from_value(Some(OsStr::new(&file.as_raw_fd().to_string())), false).is_err());
    let context = read(&file).unwrap().unwrap();
    assert_eq!(context.alias, "subject");
    // SAFETY: descriptor is kept alive by file; only its flags are queried.
    assert_ne!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
}

#[test]
fn malformed_uncommitted_and_oversized_contexts_fail_before_execution() {
    let fixture = Fixture::new();
    for (index, raw) in [
        b"".as_slice(),
        b"{}",
        b"{\n",
        b"{\"alias\":\"a\",\"alias\":\"b\"}\n",
        b"{\"alias\":\"subject\",\"unexpected\":true}\n",
    ]
    .into_iter()
    .enumerate()
    {
        assert!(read(&fixture.file(&index.to_string(), raw, true)).is_err());
    }
    let large = fixture.file("large", VALID, true);
    large.set_len(MAX_CONTEXT + 1).unwrap();
    assert!(read(&large).is_err());
}

#[test]
fn named_seeked_shared_and_append_files_are_rejected() {
    let fixture = Fixture::new();
    assert!(read(&fixture.file("named", VALID, false)).is_err());
    let mut seeked = fixture.file("seeked", VALID, true);
    seeked.seek(std::io::SeekFrom::Start(1)).unwrap();
    assert!(read(&seeked).is_err());
    let shared = fixture.file("shared", VALID, true);
    shared
        .set_permissions(std::fs::Permissions::from_mode(0o640))
        .unwrap();
    assert!(read(&shared).is_err());
    let append = fixture.file("append", VALID, true);
    // SAFETY: fcntl changes flags on a descriptor retained by this test.
    assert_eq!(
        unsafe { libc::fcntl(append.as_raw_fd(), libc::F_SETFL, libc::O_APPEND) },
        0
    );
    assert!(read(&append).is_err());
    assert!(read(&File::open(&fixture.0).unwrap()).is_err());
}
