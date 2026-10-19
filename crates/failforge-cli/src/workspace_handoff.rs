//! Transfer the same leased open-file description, never reopen its pathname.
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use failforge_workspace::InputError;
use failforge_workspace::lifecycle::LifecycleCaps;

pub(crate) const HANDOFF_ENV: &str = "FAILFORGE_WORKSPACE_HANDOFF_FD";
pub(crate) const CAPS_ENV: &str = "FAILFORGE_WORKSPACE_CAPS_FDS";
const PACKET: &[u8] = b"FFWS1";

fn invalid() -> InputError {
    InputError::Invalid("invalid native workspace capabilities".into())
}

fn descriptors(value: &OsStr, count: usize) -> Result<Vec<RawFd>, InputError> {
    let value = value.to_str().ok_or_else(invalid)?;
    let result = value
        .split(',')
        .map(|text| {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid());
            }
            let descriptor: RawFd = text.parse().map_err(|_| invalid())?;
            if descriptor < 3 {
                return Err(invalid());
            }
            Ok(descriptor)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if result.len() != count || result.iter().copied().collect::<BTreeSet<_>>().len() != count {
        return Err(invalid());
    }
    Ok(result)
}

// This private command takes ownership of explicitly inherited descriptors.
// The parser rejects duplicates, stdio and conflicts before invoking this.
fn adopt(descriptor: RawFd) -> Result<File, InputError> {
    // SAFETY: inspect validity before adopting the inherited descriptor once.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: the unique inherited descriptor now has exactly one Rust owner.
    let file = unsafe { File::from_raw_fd(descriptor) };
    // SAFETY: this changes only flags of the live owned descriptor.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(file)
}

pub(crate) struct Handoff(File);

impl Handoff {
    pub(crate) fn from_env() -> Result<Self, InputError> {
        if std::env::var_os(CAPS_ENV).is_some() {
            return Err(invalid());
        }
        Self::from_value(&std::env::var_os(HANDOFF_ENV).ok_or_else(invalid)?)
    }

    fn from_value(value: &OsStr) -> Result<Self, InputError> {
        let file = adopt(descriptors(value, 1)?[0])?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no pointers or side effects.
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(invalid());
        }
        let mut kind = 0 as libc::c_int;
        let mut length = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt writes into initialized, correctly sized storage.
        if unsafe {
            libc::getsockopt(
                file.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut libc::c_int).cast(),
                &mut length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if kind != libc::SOCK_DGRAM {
            return Err(invalid());
        }
        for peer in [false, true] {
            // SAFETY: zero is a valid initial representation for sockaddr_storage.
            let mut address: libc::sockaddr_storage = unsafe { zeroed() };
            let mut length = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            // SAFETY: the syscall writes at most the supplied address capacity.
            let result = unsafe {
                if peer {
                    libc::getpeername(
                        file.as_raw_fd(),
                        (&mut address as *mut libc::sockaddr_storage).cast(),
                        &mut length,
                    )
                } else {
                    libc::getsockname(
                        file.as_raw_fd(),
                        (&mut address as *mut libc::sockaddr_storage).cast(),
                        &mut length,
                    )
                }
            };
            if result < 0 {
                return Err(io::Error::last_os_error().into());
            }
            if address.ss_family as libc::c_int != libc::AF_UNIX {
                return Err(invalid());
            }
        }
        Ok(Self(file))
    }

    pub(crate) fn send(&self, caps: &LifecycleCaps) -> Result<(), InputError> {
        let lease = caps.lease.as_ref().ok_or_else(invalid)?;
        let descriptors = [
            caps.root.as_raw_fd(),
            caps.campaign.as_raw_fd(),
            caps.work.as_raw_fd(),
            lease.as_raw_fd(),
        ];
        let size = size_of::<[RawFd; 4]>();
        // SAFETY: CMSG_SPACE calculates capacity from a small fixed byte count.
        let capacity = unsafe { libc::CMSG_SPACE(size as _) } as usize;
        // usize storage provides cmsghdr alignment; the capacity includes padding.
        let mut ancillary = vec![0_usize; capacity.div_ceil(size_of::<usize>())];
        let mut vector = libc::iovec {
            iov_base: PACKET.as_ptr().cast_mut().cast(),
            iov_len: PACKET.len(),
        };
        // SAFETY: all-zero msghdr is valid before setting its buffer fields.
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = capacity as _;
        // SAFETY: ancillary is aligned and large enough for one descriptor array.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            if header.is_null() {
                return Err(invalid());
            }
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size as _) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                size,
            );
        }
        loop {
            // SAFETY: all buffers/descriptors remain live for this bounded send.
            let sent = unsafe {
                libc::sendmsg(
                    self.0.as_raw_fd(),
                    &message,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if sent == PACKET.len() as isize {
                return Ok(());
            }
            if sent >= 0 {
                return Err(invalid());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }
}

pub(crate) fn capabilities(count: usize) -> Result<LifecycleCaps, InputError> {
    if std::env::var_os(HANDOFF_ENV).is_some() {
        return Err(invalid());
    }
    let raw = descriptors(&std::env::var_os(CAPS_ENV).ok_or_else(invalid)?, count)?;
    let mut files = raw
        .into_iter()
        .map(adopt)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter();
    Ok(LifecycleCaps {
        root: files.next().ok_or_else(invalid)?,
        campaign: files.next().ok_or_else(invalid)?,
        work: files.next().ok_or_else(invalid)?,
        lease: files.next(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::{UnixDatagram, UnixStream};

    #[test]
    fn descriptors_require_exact_distinct_non_stdio_capabilities() {
        for text in [
            "",
            "0",
            "2",
            "-3",
            "+3",
            " 3",
            "3 ",
            "3,3,5,6",
            "3,4,5",
            "3,4,5,6,7",
            "2147483648,4,5,6",
        ] {
            assert!(descriptors(OsStr::new(text), 4).is_err(), "{text}");
        }
        assert_eq!(
            descriptors(OsStr::new("3,4,5,6"), 4).unwrap(),
            vec![3, 4, 5, 6]
        );
    }

    #[test]
    fn handoff_requires_a_connected_unix_datagram_and_sets_cloexec() {
        let (socket, _peer) = UnixDatagram::pair().unwrap();
        let descriptor = socket.into_raw_fd();
        let handoff = Handoff::from_value(OsStr::new(&descriptor.to_string())).unwrap();
        // SAFETY: the descriptor is owned by the live handoff.
        assert_ne!(
            unsafe { libc::fcntl(handoff.0.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let (socket, _peer) = UnixStream::pair().unwrap();
        let descriptor = socket.into_raw_fd();
        assert!(Handoff::from_value(OsStr::new(&descriptor.to_string())).is_err());
        let socket = UnixDatagram::unbound().unwrap();
        let descriptor = socket.into_raw_fd();
        assert!(Handoff::from_value(OsStr::new(&descriptor.to_string())).is_err());
    }
}
