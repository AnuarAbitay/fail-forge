//! Private controller channel; its handle is never passed to the Dart child.
use std::io;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::signal::windows::{CtrlBreak, CtrlC, ctrl_break, ctrl_c};

pub(super) struct Cancellation {
    interrupt: CtrlC,
    terminate: CtrlBreak,
    control: Option<NamedPipeClient>,
}
impl Cancellation {
    pub(super) async fn new() -> io::Result<Self> {
        // Install console listeners and bind the private pipe before any child.
        let interrupt = ctrl_c()?;
        let terminate = ctrl_break()?;
        let control = match std::env::var_os(super::CANCEL_ENV) {
            Some(name) => Some(connect(&name).await?),
            None => None,
        };
        Ok(Self {
            interrupt,
            terminate,
            control,
        })
    }
    pub(super) async fn request(&mut self) -> io::Result<&'static str> {
        tokio::select! {
            event = self.interrupt.recv() => event.map(|()| "SIGINT")
                .ok_or_else(|| io::Error::other("interrupt listener closed")),
            event = self.terminate.recv() => event.map(|()| "SIGTERM")
                .ok_or_else(|| io::Error::other("termination listener closed")),
            result = control_request(&mut self.control) => result,
        }
    }
}
async fn connect(name: &std::ffi::OsStr) -> io::Result<NamedPipeClient> {
    let text = name
        .to_str()
        .filter(|text| {
            text.strip_prefix(r"\\.\pipe\failforge-")
                .is_some_and(|suffix| {
                    !suffix.is_empty()
                        && suffix
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
                })
                && text.len() <= 240
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid campaign cancellation capability",
            )
        })?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ClientOptions::new().read(true).write(false).open(text) {
                Ok(client) => return Ok(client),
                Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "campaign cancellation connect deadline exceeded",
        )
    })?
}
async fn control_request(control: &mut Option<NamedPipeClient>) -> io::Result<&'static str> {
    let Some(control) = control else {
        return std::future::pending().await;
    };
    let mut byte = [0_u8; 1];
    match control.read(&mut byte).await {
        Ok(0) => Ok("controller_eof"),
        Ok(_) => match byte[0] {
            b'I' => Ok("SIGINT"),
            b'T' => Ok("SIGTERM"),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid campaign cancellation request",
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok("controller_eof"),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use failforge_workspace::windows_fs::{PrivateSecurity, random_token};
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ServerOptions;
    #[tokio::test]
    async fn private_pipe_buffers_bounded_requests_and_maps_controller_eof() {
        for (request, expected) in [
            (Some(b'I'), "SIGINT"),
            (Some(b'T'), "SIGTERM"),
            (None, "controller_eof"),
            (Some(b'X'), "invalid"),
        ] {
            let name = format!(
                r"\\.\pipe\failforge-control-test-{}{}",
                random_token().unwrap(),
                random_token().unwrap()
            );
            let mut security = PrivateSecurity::new().unwrap();
            let mut server = unsafe {
                ServerOptions::new()
                    .access_inbound(false)
                    .access_outbound(true)
                    .first_pipe_instance(true)
                    .reject_remote_clients(true)
                    .max_instances(1)
                    .create_with_security_attributes_raw(&name, security.as_ptr().cast())
                    .unwrap()
            };
            let client = connect(std::ffi::OsStr::new(&name)).await.unwrap();
            server.connect().await.unwrap();
            if let Some(byte) = request {
                server.write_all(&[byte]).await.unwrap();
            }
            drop(server);
            let result =
                tokio::time::timeout(Duration::from_secs(2), control_request(&mut Some(client)))
                    .await
                    .unwrap();
            if expected == "invalid" {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
            } else {
                assert_eq!(result.unwrap(), expected);
            }
        }
    }
    #[tokio::test]
    async fn foreign_or_malformed_endpoint_is_rejected_before_connecting() {
        for name in [
            "",
            r"\\server\pipe\failforge-test",
            r"\\.\pipe\foreign",
            r"\\.\pipe\failforge-x\more",
            "nul",
        ] {
            assert_eq!(
                connect(std::ffi::OsStr::new(name))
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
