//! One private outbound named-pipe instance supplies the adapter's stdin.
//! Only the current account can open it; no endpoint is passed to the child.
use std::fs::{File, OpenOptions};
use std::io;

use failforge_workspace::windows_fs::{PrivateSecurity, random_token};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

pub(super) fn input_pair() -> io::Result<(NamedPipeServer, File)> {
    let name = format!(
        r"\\.\pipe\failforge-dart-{}{}",
        random_token()?,
        random_token()?
    );
    let mut security = PrivateSecurity::new()?;
    // SAFETY: PrivateSecurity owns the descriptor and attributes for the entire
    // creation call; the server handle is not inheritable.
    let server = unsafe {
        ServerOptions::new()
            .access_inbound(false)
            .access_outbound(true)
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .out_buffer_size(32 * 1024)
            .create_with_security_attributes_raw(&name, security.as_ptr().cast())?
    };
    // A synchronous read handle is required by the adapter's standard input.
    // The runner duplicates only this handle into the child's explicit list.
    let input = OpenOptions::new().read(true).open(name)?;
    Ok((server, input))
}
