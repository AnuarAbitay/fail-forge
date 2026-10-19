//! Bounded private JSON responses shared by the native workspace commands.
use std::io::{self, Write};

use failforge_workspace::{Cancellation, InputError};
use serde_json::{Value, json};

const LIMIT: usize = 64 * 1024 * 1024;

struct Response(Vec<u8>);

impl Write for Response {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > LIMIT - self.0.len() - 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace response limit exceeded",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn encode(result: Value) -> Result<Vec<u8>, InputError> {
    let mut response = Response(Vec::new());
    serde_json::to_writer(&mut response, &json!({"version":1,"result":result}))
        .map_err(|_| InputError::Invalid("native workspace response exceeds limits".into()))?;
    response.0.push(b'\n');
    Ok(response.0)
}

pub(crate) fn publish(response: &[u8], cancellation: &Cancellation) -> Result<(), InputError> {
    for chunk in response.chunks(64 * 1024) {
        cancellation.check()?;
        std::io::stdout().lock().write_all(chunk)?;
    }
    cancellation.check()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_is_framed_and_reserves_the_delimiter_within_the_limit() {
        let bytes = encode(json!({"ok":true})).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"version":1,"result":{"ok":true}})
        );
        let mut buffer = Response(vec![0; LIMIT - 2]);
        assert_eq!(buffer.write(b"x").unwrap(), 1);
        assert!(buffer.write(b"y").is_err());
        assert_eq!(buffer.0.len(), LIMIT - 1);
    }
}
