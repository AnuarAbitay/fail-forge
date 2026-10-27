//! Native path spellings for portable request and filesystem fixtures.
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(crate) fn absolute(value: &str) -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from(value)
    }
    #[cfg(windows)]
    {
        let path = Path::new(value);
        if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir().unwrap().join(path)
        }
    }
}
pub(crate) fn paths(value: Value) -> Value {
    #[cfg(unix)]
    {
        value
    }
    #[cfg(windows)]
    {
        match value {
            Value::String(text) if text.starts_with('/') && !text.starts_with("//") => {
                serde_json::to_value(absolute(&text)).unwrap()
            }
            Value::Object(rows) => Value::Object(
                rows.into_iter()
                    .map(|(key, value)| (key, paths(value)))
                    .collect(),
            ),
            Value::Array(rows) => Value::Array(rows.into_iter().map(paths).collect()),
            value => value,
        }
    }
}
pub(crate) fn symlink(source: impl AsRef<Path>, target: impl AsRef<Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
    }
    #[cfg(windows)]
    {
        let source_path = source.as_ref();
        let resolved = if source_path.is_absolute() {
            source_path.to_owned()
        } else {
            target.as_ref().parent().unwrap().join(source_path)
        };
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(source, target)
        } else {
            std::os::windows::fs::symlink_file(source, target)
        }
    }
}

/// Invalid UTF-8 on Unix and unpaired UTF-16 on Windows exercise native argv.
pub(crate) fn raw(bytes: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(bytes.to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(
            &bytes
                .iter()
                .map(|byte| {
                    if byte.is_ascii() {
                        u16::from(*byte)
                    } else {
                        0xd800 + u16::from(*byte)
                    }
                })
                .collect::<Vec<_>>(),
        )
    }
}
