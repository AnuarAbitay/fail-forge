//! Read retained campaign facts and export a single offline HTML document.

pub(crate) mod data;
pub(crate) mod source;

use std::fs::OpenOptions;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::args::ReportArgs;

const CSS: &str = include_str!("report.css");
const SCRIPT: &str = include_str!("report.js");
const BODY: &str = include_str!("body.html");
const MAX_HTML_BYTES: usize = 128 * 1024 * 1024;

pub(crate) fn execute(arguments: ReportArgs, invocation_cwd: &Path) -> ExitCode {
    let input = invocation_cwd.join(arguments.run);
    let output = invocation_cwd.join(arguments.html);
    let source_root = arguments.source_root.map(|path| invocation_cwd.join(path));
    match export(&input, &output, source_root.as_deref()) {
        Ok(()) => {
            let _ = writeln!(std::io::stdout(), "HTML report: {}", output.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            let code = match error.kind() {
                io::ErrorKind::InvalidInput
                | io::ErrorKind::InvalidData
                | io::ErrorKind::NotFound
                | io::ErrorKind::AlreadyExists
                | io::ErrorKind::NotADirectory
                | io::ErrorKind::IsADirectory => 2,
                _ => 125,
            };
            let _ = writeln!(std::io::stderr(), "failforge report: {error}");
            ExitCode::from(code)
        }
    }
}

fn export(input: &Path, output: &Path, source_root: Option<&Path>) -> io::Result<()> {
    let data = match source_root {
        Some(source_root) => crate::report_html::data::load_with_source(input, Some(source_root))?,
        None => crate::report_html::data::load(input)?,
    };
    let document = render(&data)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(output).map_err(|error| {
        let action = if error.kind() == io::ErrorKind::AlreadyExists {
            "output already exists; choose a new HTML path"
        } else {
            "cannot create HTML output"
        };
        io::Error::new(
            error.kind(),
            format!("{action}: {}: {error}", output.display()),
        )
    })?;
    // Generation finishes in memory before creating the output. Exclusive
    // creation protects existing files, including the input and symlink aliases.
    // If publication fails, leave the partial file for the user to inspect.
    file.write_all(document.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot finish HTML output (partial file retained): {}: {error}",
                    output.display()
                ),
            )
        })
}

fn render(data: &Value) -> io::Result<String> {
    let payload = script_json(data)?;
    // HTML parsers normalize line endings before CSP hashing. A Windows Git
    // checkout may use CRLF even though the source asset was authored with LF.
    let script = SCRIPT.replace("\r\n", "\n").replace('\r', "\n");
    let script_hash = base64(&Sha256::digest(script.as_bytes()));
    let mut html =
        String::with_capacity(payload.len() + CSS.len() + script.len() + BODY.len() + 1024);
    html.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    html.push_str(&format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'sha256-{script_hash}'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'\">\n"
    ));
    html.push_str("<meta name=\"color-scheme\" content=\"light dark\">\n<title>FailForge mutation report</title>\n<style>");
    html.push_str(CSS);
    html.push_str("</style>\n</head>\n<body>\n");
    html.push_str(BODY);
    html.push_str("\n<noscript>This report needs JavaScript for navigation and filtering. The original results remain available in summary.json.</noscript>\n<script type=\"application/json\" id=\"failforge-report-data\">");
    html.push_str(&payload);
    html.push_str("</script>\n<script>");
    html.push_str(&script);
    html.push_str("</script>\n</body>\n</html>\n");
    if html.len() > MAX_HTML_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HTML report exceeds the 128 MiB export limit",
        ));
    }
    Ok(html)
}

/// JSON remains JSON, but cannot close its data element or become HTML markup.
fn script_json(value: &Value) -> io::Result<String> {
    let raw = serde_json::to_string(value).map_err(io::Error::other)?;
    let mut safe = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            '<' => safe.push_str("\\u003c"),
            '>' => safe.push_str("\\u003e"),
            '&' => safe.push_str("\\u0026"),
            '\u{2028}' => safe.push_str("\\u2028"),
            '\u{2029}' => safe.push_str("\\u2029"),
            other => safe.push(other),
        }
        if safe.len() > MAX_HTML_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "embedded report data exceeds the 128 MiB export limit",
            ));
        }
    }
    Ok(safe)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[usize::from(first >> 2)] as char);
        encoded.push(ALPHABET[usize::from(((first & 3) << 4) | (second >> 4))] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[usize::from(((second & 15) << 2) | (third >> 6))] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[usize::from(third & 63)] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embedded_untrusted_data_stays_json_without_html_delimiters() {
        let value = json!({"file":"</script><script>alert(1)</script>",
            "log":"<img src=x onerror=alert(1)>&\u{2028}\u{2029}"});
        let encoded = script_json(&value).unwrap();
        assert!(!encoded.contains(['<', '>', '&', '\u{2028}', '\u{2029}']));
        assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), value);
    }

    #[test]
    fn base64_matches_standard_padding_and_sha256() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(
            base64(&Sha256::digest(b"abc")),
            "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
    }

    #[test]
    fn document_constrains_scripts_and_keeps_hostile_text_in_data() {
        let html = render(&json!({"run":{"id":"</script><script>alert(1)</script>"}})).unwrap();
        assert_eq!(html.matches("</script>").count(), 2);
        assert!(html.contains("script-src 'sha256-"));
        assert!(html.contains("connect-src 'none'"));
        assert!(!html.contains("</script><script>alert(1)"));
    }
}
