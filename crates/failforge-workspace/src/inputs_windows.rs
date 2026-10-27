//! Local Windows file URI mapping used by Dart package configuration.
use super::*;

pub fn file_uri(path: &Path) -> Result<String, InputError> {
    let text = path
        .to_str()
        .ok_or_else(|| failure("dependency path must be UTF-8"))?;
    let text = text
        .strip_prefix(r"\\?\")
        .unwrap_or(text)
        .replace('\\', "/");
    if text.starts_with("UNC/") || !text.as_bytes().get(1).is_some_and(|byte| *byte == b':') {
        return Err(failure("dependency path must name a local Windows volume"));
    }
    let mut uri = String::from("file:///");
    for (index, byte) in text.bytes().enumerate() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) || index == 1 && byte == b':' {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(uri)
}

pub(super) fn local_uri(config: &Path, text: &str) -> Result<PathBuf, InputError> {
    if text.contains(['\0', '\\']) {
        return Err(failure("invalid prepared dependency URI"));
    }
    let path = text.split(['?', '#']).next().unwrap_or_default();
    let file_scheme = path
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"));
    let rest = if file_scheme { &path[5..] } else { path };
    let path = if let Some(rest) = rest.strip_prefix("//") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return Err(failure("dependencies must already exist on a local volume"));
        }
        format!("/{path}")
    } else {
        if !file_scheme && rest.split('/').next().unwrap_or_default().contains(':') {
            return Err(failure("dependencies must already exist locally"));
        }
        rest.to_owned()
    };
    if path.is_empty() {
        return Err(failure("prepared dependency URI has no directory path"));
    }
    let joined = if path.starts_with('/') {
        path
    } else {
        let base = file_uri(
            config
                .parent()
                .ok_or_else(|| failure("package configuration has no parent"))?,
        )?;
        format!("{}/{path}", base.strip_prefix("file://").expect("file URI"))
    };
    let mut segments = vec![];
    for segment in joined.split('/') {
        match segment {
            "" | "." => (),
            ".." => {
                if segments.len() > 1 {
                    segments.pop();
                }
            }
            part => segments.push(part),
        }
    }
    let normalized = segments.join("/");
    let mut decoded = vec![];
    let bytes = normalized.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16).map(|v| v as u8);
            if let (Some(a), Some(b)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(a * 16 + b);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    let decoded =
        String::from_utf8(decoded).map_err(|_| failure("dependency URI must be UTF-8"))?;
    if decoded.contains(['\0', '\\'])
        || !decoded.as_bytes().get(1).is_some_and(|b| *b == b':')
        || decoded.as_bytes().get(2) != Some(&b'/')
    {
        return Err(failure("invalid local Windows dependency URI"));
    }
    // Reject alternate streams and encoded separators in components. URI syntax
    // must not introduce a second interpretation at the filesystem boundary.
    if decoded[2..].contains(':') {
        return Err(failure("invalid dependency stream path"));
    }
    let path = PathBuf::from(decoded);
    crate::windows_fs::directory(&path)?;
    Ok(path.canonicalize()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drive_uri_encodes_unicode_and_round_trips_real_local_directories() {
        let directory = std::env::temp_dir().join(format!(
            "failforge-uri-{}-some space Ж",
            crate::windows_fs::random_token().unwrap()
        ));
        fs::create_dir(&directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        let uri = file_uri(&directory).unwrap();
        assert!(uri.starts_with("file:///"));
        assert!(uri.contains("%20"));
        assert_eq!(
            local_uri(&directory.join("config.json"), &uri).unwrap(),
            directory
        );
        fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn source_hash_wire_keys_use_forward_slashes_for_nested_files() {
        let directory = std::env::temp_dir().join(format!(
            "failforge-hash-keys-{}",
            crate::windows_fs::random_token().unwrap()
        ));
        fs::create_dir_all(directory.join("lib")).unwrap();
        fs::create_dir(directory.join("test")).unwrap();
        fs::write(directory.join("lib/a.dart"), "bool a() => true;\n").unwrap();
        fs::write(directory.join("test/foo_test.dart"), "void main() {}\n").unwrap();
        let hashes = super::source_hashes(&directory, &Cancellation::default()).unwrap();
        let wire = serde_json::to_value(hashes).unwrap();
        let files = wire.as_object().unwrap();
        assert!(files.contains_key("lib/a.dart"));
        assert!(files.contains_key("test/foo_test.dart"));
        assert!(files.keys().all(|key| !key.contains('\\')));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn remote_hosts_streams_and_malformed_roots_are_rejected() {
        for value in [
            "file://remote/C:/a",
            "https://a/b",
            "file:///C:/a:stream",
            "file:///C:/a%00b",
            "file:///tmp/a",
            "a\\b",
        ] {
            assert!(
                local_uri(Path::new(r"C:\config.json"), value).is_err(),
                "{value}"
            );
        }
    }
}
