use super::*;
use failforge_workspace::windows_fs as win;
use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-capture-{}", win::random_token().unwrap()));
        let parent = win::directory(path.parent().unwrap()).unwrap();
        drop(win::create_directory_at(&parent, path.file_name().unwrap()).unwrap());
        Self(path)
    }
    fn capture(&self, bytes: &[u8]) -> CaptureSummary {
        let root = win::directory(&self.0).unwrap();
        let mut file = win::regular_at(&root, OsStr::new("stdout.bin"), true, true).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        CaptureSummary {
            file_path: Some(self.0.join("stdout.bin")),
            bytes_read: bytes.len() as u64,
            file_bytes_written: bytes.len() as u64,
            sha256: Some(Sha256::digest(bytes).into()),
            ..Default::default()
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn authenticates_binary_capture_with_and_without_retention() {
    let fixture = Fixture::new();
    let bytes = b"\0\xff\xfe\r\ncomplete";
    let summary = fixture.capture(bytes);
    let retained = read_capture_limited(&summary, true, 1024).unwrap();
    assert_eq!(retained.bytes, bytes);
    assert_eq!(retained.length, bytes.len() as u64);
    let hashed = read_capture_limited(&summary, false, 1024).unwrap();
    assert!(hashed.bytes.is_empty());
    assert_eq!(hashed.hash, retained.hash);
}

#[test]
fn rejects_equal_length_capture_tampering_and_inconsistent_counters() {
    let fixture = Fixture::new();
    let mut summary = fixture.capture(b"original");
    std::fs::write(summary.file_path.as_ref().unwrap(), b"tampered").unwrap();
    assert!(read_capture_limited(&summary, true, 1024).is_err());
    summary.sha256 = Some(Sha256::digest(b"tampered").into());
    summary.file_bytes_written -= 1;
    assert!(read_capture_limited(&summary, true, 1024).is_err());
}

#[test]
fn enforces_size_limit_and_refuses_directories() {
    let fixture = Fixture::new();
    let mut summary = fixture.capture(b"1234");
    assert!(read_capture_limited(&summary, true, 3).is_err());
    summary.file_path = Some(fixture.0.clone());
    assert!(read_capture_limited(&summary, true, 1024).is_err());
}
