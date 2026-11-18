//! Bounded original-source excerpts authenticated by the candidate's SHA-256.
use std::fs;
use std::io;
use std::path::{Component, Path};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 2 * 1024;
const MAX_CONTEXT_BYTES: usize = 16 * 1024;
const BOM: &[u8] = b"\xef\xbb\xbf";

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn verify(bytes: &[u8], expected_hash: &str) -> io::Result<()> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(invalid("source exceeds the 4 MiB context limit"));
    }
    if expected_hash.len() != 64
        || !expected_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || format!("{:x}", Sha256::digest(bytes)) != expected_hash
    {
        return Err(invalid("source does not match its recorded SHA-256"));
    }
    std::str::from_utf8(bytes).map_err(|_| invalid("source context requires UTF-8"))?;
    Ok(())
}

pub(crate) fn read_verified(root: &Path, file: &str, expected_hash: &str) -> io::Result<Vec<u8>> {
    let relative = Path::new(file);
    if file.is_empty()
        || file.contains(['\\', ':'])
        || file.chars().any(char::is_control)
        || file
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.ends_with(['.', ' ']))
        || relative
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("dart")
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(invalid("source context requires a safe relative Dart path"));
    }
    let root = root.canonicalize()?;
    if !fs::metadata(&root)?.is_dir() {
        return Err(invalid("source context root must be a directory"));
    }
    let mut path = root.clone();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        path.push(part.as_os_str());
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink()
            || if parts.peek().is_some() {
                !metadata.is_dir()
            } else {
                !metadata.is_file()
            }
        {
            return Err(invalid(
                "source context does not follow links or special files",
            ));
        }
    }
    let resolved = path.canonicalize()?;
    if !resolved.starts_with(&root) {
        return Err(invalid("source context resolves outside its project"));
    }
    let bytes = failforge_workspace::read_regular(&resolved, MAX_SOURCE_BYTES)
        .map_err(|error| invalid(&error.to_string()))?;
    verify(&bytes, expected_hash)?;
    Ok(bytes)
}

pub(crate) fn capture(
    root: &Path,
    file: &str,
    expected_hash: &str,
    edit: &Value,
) -> io::Result<Value> {
    let bytes = read_verified(root, file, expected_hash)?;
    from_bytes(&bytes, expected_hash, edit)
}

pub(crate) fn from_bytes(bytes: &[u8], source_hash: &str, edit: &Value) -> io::Result<Value> {
    verify(bytes, source_hash)?;
    let text = std::str::from_utf8(bytes).expect("source UTF-8 checked");
    let start = edit["offsetBytes"]
        .as_u64()
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or_else(|| invalid("invalid source context edit offset"))?;
    let length = edit["lengthBytes"]
        .as_u64()
        .and_then(|length| usize::try_from(length).ok())
        .ok_or_else(|| invalid("invalid source context edit length"))?;
    let expected = edit["expected"]
        .as_str()
        .ok_or_else(|| invalid("source context edit has no expected text"))?;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid("source context edit is outside the source"))?;
    if !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
        || bytes.get(start..end) != Some(expected.as_bytes())
    {
        return Err(invalid(
            "source context edit does not match original UTF-8 bytes",
        ));
    }

    // Newline bytes are not part of the rendered code. CRLF counts as one line;
    // bare CR and LF remain valid separators. Preserve an empty EOF line.
    let mut spans = Vec::new();
    let mut line_start = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        if matches!(bytes[cursor], b'\r' | b'\n') {
            spans.push((line_start, cursor));
            cursor += if bytes[cursor] == b'\r' && bytes.get(cursor + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
            line_start = cursor;
        } else {
            cursor += 1;
        }
    }
    spans.push((line_start, bytes.len()));
    let line_index = spans
        .partition_point(|(line_start, _)| *line_start <= start)
        .saturating_sub(1);
    let last_byte = if length == 0 { start } else { end - 1 };
    let end_line = spans
        .partition_point(|(line_start, _)| *line_start <= last_byte)
        .max(1);
    let (start_byte, end_byte) = spans[line_index];
    let visible_start = if line_index == 0 && bytes.starts_with(BOM) {
        BOM.len()
    } else {
        start_byte
    };
    let prefix_end = start.max(visible_start).min(end_byte);
    let column = text[visible_start..prefix_end].encode_utf16().count() + 1;
    let first = line_index.saturating_sub(3);
    let last = (line_index + 4).min(spans.len());
    let mut truncated = end_line > last;
    let mut used = 0;
    let lines = spans[first..last]
        .iter()
        .enumerate()
        .map(|(index, (start, end))| {
            let start = if first + index == 0 && bytes.starts_with(BOM) {
                BOM.len()
            } else {
                *start
            };
            let line = &text[start..*end];
            let limit = MAX_LINE_BYTES.min(MAX_CONTEXT_BYTES - used);
            let value = if line.len() > limit {
                truncated = true;
                let mut end = limit.saturating_sub('…'.len_utf8());
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &line[..end])
            } else {
                line.to_owned()
            };
            used += value.len();
            value
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"version":1,"sourceHash":source_hash,"line":line_index+1,"column":column,
        "endLine":end_line,"startLine":first+1,"lines":lines,"truncated":truncated}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn context(text: &str, offset: usize, expected: &str) -> Value {
        from_bytes(
            text.as_bytes(),
            &format!("{:x}", Sha256::digest(text.as_bytes())),
            &json!({"offsetBytes":offset,"lengthBytes":expected.len(),"expected":expected}),
        )
        .unwrap()
    }

    #[test]
    fn positions_count_utf16_and_ignore_a_leading_bom() {
        let source = "\u{feff}α👭true\r\nnext\rlast\n";
        let offset = source.find("true").unwrap();
        let value = context(source, offset, "true");
        assert_eq!(value["line"], 1);
        assert_eq!(value["column"], 4);
        assert_eq!(value["lines"], json!(["α👭true", "next", "last", ""]));
        let value = context(source, source.find("last").unwrap(), "last");
        assert_eq!(value["line"], 3);
        assert_eq!(value["column"], 1);
        let value = context(source, source.len(), "");
        assert_eq!(value["line"], 4);
        assert_eq!(value["endLine"], 4);
        assert_eq!(value["column"], 1);
    }

    #[test]
    fn multiline_edits_and_eof_insertions_use_inclusive_source_lines() {
        let source = "one\r\ntwo\nthree\rfour";
        let value = context(source, 2, "e\r\ntwo\nth");
        assert_eq!(value["line"], 1);
        assert_eq!(value["endLine"], 3);
        let value = context(source, source.len(), "");
        assert_eq!(value["line"], 4);
        assert_eq!(value["column"], 5);
        assert_eq!(context("", 0, "")["lines"], json!([""]));
    }

    #[test]
    fn excerpt_keeps_the_mutated_line_and_bounds_utf8_text() {
        let source = (0..12)
            .map(|i| format!("{i}: {}\n", "👭".repeat(1200)))
            .collect::<String>();
        let offset = source.find("7:").unwrap();
        let value = context(&source, offset, "7:");
        assert_eq!(value["startLine"], 5);
        assert_eq!(value["line"], 8);
        let lines = value["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 7);
        assert!(lines[3].as_str().unwrap().starts_with("7:"));
        assert!(
            lines
                .iter()
                .all(|line| line.as_str().unwrap().len() <= MAX_LINE_BYTES)
        );
        assert!(
            lines
                .iter()
                .map(|line| line.as_str().unwrap().len())
                .sum::<usize>()
                <= MAX_CONTEXT_BYTES
        );
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn invalid_ranges_mismatched_bytes_and_hashes_are_rejected() {
        let bytes = "αtrue".as_bytes();
        let digest = format!("{:x}", Sha256::digest(bytes));
        for edit in [
            json!({"offsetBytes":1,"lengthBytes":1,"expected":"x"}),
            json!({"offsetBytes":2,"lengthBytes":4,"expected":"else"}),
            json!({"offsetBytes":u64::MAX,"lengthBytes":4,"expected":"true"}),
            json!({"offsetBytes":2,"lengthBytes":5,"expected":"true"}),
            json!({"offsetBytes":2,"lengthBytes":4,"expected":"true!"}),
        ] {
            assert!(from_bytes(bytes, &digest, &edit).is_err());
        }
        assert!(from_bytes(bytes, &"0".repeat(64), &json!({})).is_err());
        let invalid_utf8 = b"\xff";
        let hash = format!("{:x}", Sha256::digest(invalid_utf8));
        assert!(from_bytes(invalid_utf8, &hash, &json!({})).is_err());
        let oversized = vec![b'x'; MAX_SOURCE_BYTES + 1];
        let hash = format!("{:x}", Sha256::digest(&oversized));
        assert!(from_bytes(&oversized, &hash, &json!({})).is_err());
    }

    #[test]
    fn file_reads_verify_content_and_reject_traversal_links_and_changed_sources() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ff-source-context-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("lib")).unwrap();
        fs::write(root.join("lib/value.dart"), "true").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"true"));
        assert_eq!(
            read_verified(&root, "lib/value.dart", &hash).unwrap(),
            b"true"
        );
        for file in [
            "../value.dart",
            "/lib/value.dart",
            "lib/./value.dart",
            "lib//value.dart",
            "lib/value.txt",
            "lib\\value.dart",
            "lib/value.dart:stream",
        ] {
            assert!(read_verified(&root, file, &hash).is_err(), "{file}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("lib/value.dart"), root.join("alias.dart"))
                .unwrap();
            assert!(read_verified(&root, "alias.dart", &hash).is_err());
        }
        fs::write(root.join("lib/value.dart"), "false").unwrap();
        assert!(read_verified(&root, "lib/value.dart", &hash).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
