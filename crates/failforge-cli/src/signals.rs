//! Console cancellation and an optional private controller channel.
#[cfg(unix)]
pub(crate) use tokio::signal::unix::{SignalKind, signal};

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::io;
    use std::sync::{Mutex, OnceLock};
    use tokio::io::AsyncReadExt;
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::signal::windows::{CtrlBreak, CtrlC, CtrlClose, CtrlShutdown};
    use tokio::sync::watch;

    const CONTROL_ENV: &str = "FAILFORGE_CLI_CANCEL_PIPE";

    #[derive(Clone, Copy)]
    pub(crate) enum SignalKind {
        Interrupt,
        Terminate,
    }
    impl SignalKind {
        pub(crate) fn interrupt() -> Self {
            Self::Interrupt
        }
        pub(crate) fn terminate() -> Self {
            Self::Terminate
        }
        fn number(self) -> i32 {
            match self {
                Self::Interrupt => 2,
                Self::Terminate => 15,
            }
        }
    }

    enum Native {
        Interrupt(CtrlC),
        Terminate {
            interrupt: CtrlBreak,
            close: CtrlClose,
            shutdown: CtrlShutdown,
        },
    }
    impl Native {
        async fn recv(&mut self) -> Option<()> {
            match self {
                Self::Interrupt(stream) => stream.recv().await,
                Self::Terminate {
                    interrupt,
                    close,
                    shutdown,
                } => tokio::select! {
                    event = interrupt.recv() => event,
                    event = close.recv() => event,
                    event = shutdown.recv() => event,
                },
            }
        }
    }

    struct Controller {
        name: OsString,
        latest: watch::Sender<i32>,
    }
    static CONTROLLER: OnceLock<Mutex<Option<Controller>>> = OnceLock::new();

    fn control() -> io::Result<Option<watch::Receiver<i32>>> {
        let Some(name) = std::env::var_os(CONTROL_ENV) else {
            return Ok(None);
        };
        let text = name
            .to_str()
            .ok_or_else(|| io::Error::other("invalid cancellation capability"))?;
        let token = text
            .strip_prefix(r"\\.\pipe\failforge-cli-cancel-")
            .filter(|token| {
                matches!(token.len(), 32 | 64) && token.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| io::Error::other("invalid cancellation capability"))?;
        let _ = token;
        let mut guard = CONTROLLER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| io::Error::other("cancellation controller unavailable"))?;
        if let Some(controller) = guard.as_ref() {
            if controller.name != name {
                return Err(io::Error::other("cancellation capability changed"));
            }
            return Ok(Some(controller.latest.subscribe()));
        }
        // Tokio's default SECURITY_IDENTIFICATION prevents server impersonation.
        let mut client = ClientOptions::new().read(true).write(false).open(&name)?;
        let (latest, receiver) = watch::channel(0);
        let signal = latest.clone();
        tokio::spawn(async move {
            let mut byte = [0];
            let number = match client.read_exact(&mut byte).await {
                Ok(_) if byte[0] == b'T' => 15,
                // EOF or malformed controller input requests a conservative stop.
                _ => 2,
            };
            signal.send_replace(number);
        });
        *guard = Some(Controller { name, latest });
        Ok(Some(receiver))
    }

    pub(crate) struct Signal {
        native: Native,
        controller: Option<watch::Receiver<i32>>,
        kind: SignalKind,
        control_seen: bool,
    }
    impl Signal {
        pub(crate) async fn recv(&mut self) -> Option<()> {
            if self.control_seen {
                return self.native.recv().await;
            }
            let Some(controller) = self.controller.as_mut() else {
                return self.native.recv().await;
            };
            let number = self.kind.number();
            tokio::select! {
                event = self.native.recv() => event,
                _ = async {
                    loop {
                        if *controller.borrow_and_update() == number { break; }
                        if controller.changed().await.is_err() { std::future::pending::<()>().await; }
                    }
                } => { self.control_seen = true; Some(()) },
            }
        }
    }
    pub(crate) fn signal(kind: SignalKind) -> io::Result<Signal> {
        let native = match kind {
            SignalKind::Interrupt => Native::Interrupt(tokio::signal::windows::ctrl_c()?),
            SignalKind::Terminate => Native::Terminate {
                interrupt: tokio::signal::windows::ctrl_break()?,
                close: tokio::signal::windows::ctrl_close()?,
                shutdown: tokio::signal::windows::ctrl_shutdown()?,
            },
        };
        Ok(Signal {
            native,
            controller: control()?,
            kind,
            control_seen: false,
        })
    }
}

#[cfg(windows)]
pub(crate) use windows::{SignalKind, signal};
