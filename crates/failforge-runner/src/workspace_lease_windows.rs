//! Windows campaign leases use independent shared locks beyond the marker.
//! An inherited handle alone does not inherit its creator's byte-range lock.
use crate::CommandSpec;
use std::fs::File;
use std::io;
use std::mem::zeroed;
use std::os::windows::fs::FileExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle, RawHandle};
use windows_sys::Win32::Foundation::{
    GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::OVERLAPPED;

pub const WORKSPACE_LEASE_ENV: &str = "FAILFORGE_WORKSPACE_LEASE_FD";
pub const WORKSPACE_LEASE_MAGIC: &[u8] = b"FAILFORGE_WORKSPACE_LEASE_V1\n";
/// Owners hold a shared lock on this byte, cleanup must acquire it exclusively.
pub const WORKSPACE_LEASE_LOCK_OFFSET: u64 = WORKSPACE_LEASE_MAGIC.len() as u64;

#[derive(Debug)]
pub struct WorkspaceLease {
    _descriptor: OwnedHandle,
}
impl WorkspaceLease {
    pub(crate) fn file(&self) -> io::Result<File> {
        // Export an independent unlocked description. Closing temporary
        // duplicates must never affect the caller's existing shared lock.
        let handle = unsafe {
            ReOpenFile(
                self._descriptor.as_raw_handle(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful ReOpenFile returned a uniquely owned file handle.
        Ok(unsafe { File::from_raw_handle(handle) })
    }
    pub fn from_borrowed(handle: BorrowedHandle<'_>) -> io::Result<Self> {
        Self::from_handle(handle.as_raw_handle().cast())
    }
    pub fn protect_child(&self, spec: &mut CommandSpec) {
        spec.env.set.retain(|key, _| {
            !key.as_encoded_bytes()
                .eq_ignore_ascii_case(WORKSPACE_LEASE_ENV.as_bytes())
        });
        spec.env.remove.insert(WORKSPACE_LEASE_ENV.into());
    }
    pub fn from_env() -> io::Result<Option<Self>> {
        let Some(value) = std::env::var_os(WORKSPACE_LEASE_ENV) else {
            return Ok(None);
        };
        let text = value.to_str().ok_or_else(invalid_capability)?;
        if text.is_empty() || !text.bytes().all(|value| value.is_ascii_digit()) {
            return Err(invalid_capability());
        }
        let handle: usize = text.parse().map_err(|_| invalid_capability())?;
        if handle == 0 || handle == usize::MAX {
            return Err(invalid_capability());
        }
        Self::from_handle(handle as HANDLE).map(Some)
    }
    fn from_handle(source: HANDLE) -> io::Result<Self> {
        // SAFETY: ReOpenFile validates the externally supplied handle, opens the
        // same file by identity and returns a new independent owned description.
        let handle = unsafe {
            ReOpenFile(
                source,
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful ReOpenFile returned a unique real owned file handle.
        let file = unsafe { File::from_raw_handle(handle as RawHandle) };
        validate(&file)?;
        let mut marker = [0; WORKSPACE_LEASE_MAGIC.len()];
        if file.seek_read(&mut marker, 0)? != marker.len() || marker != WORKSPACE_LEASE_MAGIC {
            return Err(invalid_capability());
        }
        // Lock a byte past EOF so marker validation remains possible while all
        // owners hold shared locks. This process acquires its own lock: parent
        // termination cannot release this owner's lease.
        let mut overlapped: OVERLAPPED = unsafe { zeroed() };
        overlapped.Anonymous.Anonymous.Offset = WORKSPACE_LEASE_LOCK_OFFSET as u32;
        // SAFETY: the owned file and initialized OVERLAPPED outlive synchronous
        // fail-immediately acquisition; shared lock flags omit EXCLUSIVE_LOCK.
        if unsafe { LockFileEx(handle, LOCKFILE_FAIL_IMMEDIATELY, 0, 1, 0, &mut overlapped) } == 0 {
            return Err(io::Error::last_os_error());
        }
        validate(&file)?;
        // SAFETY: change inheritance only on the verified source capability.
        if unsafe { SetHandleInformation(source, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _descriptor: file.into(),
        })
    }
}
fn validate(file: &File) -> io::Result<()> {
    let handle = file.as_raw_handle().cast();
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    // SAFETY: the file is owned, information is valid writable ABI storage.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK
        || unsafe { GetFileInformationByHandle(handle, &mut information) } == 0
    {
        return Err(invalid_capability());
    }
    if information.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || information.nNumberOfLinks != 1
        || information.nFileSizeHigh != 0
        || information.nFileSizeLow as usize != WORKSPACE_LEASE_MAGIC.len()
    {
        return Err(invalid_capability());
    }
    require_private_owner(file)?;
    Ok(())
}

fn require_private_owner(file: &File) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = std::ptr::null_mut();
    // SAFETY: pseudo current process is valid and token points to writable output.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let token = Token(token);
    let mut length = 0;
    // SAFETY: null output requests the exact TokenUser storage size.
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut length);
    }
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut storage = vec![0_usize; (length as usize).div_ceil(size_of::<usize>())];
    // SAFETY: storage is aligned and large enough for the queried native record.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let expected = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
    let (mut owner, mut acl, mut descriptor) = (
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    // SAFETY: file is live, out pointers are valid; returned SID/ACL point inside
    // the security descriptor allocation, retained until after all comparisons.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    struct Descriptor(*mut core::ffi::c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    let _descriptor = Descriptor(descriptor);
    if owner.is_null() || unsafe { EqualSid(owner, expected) } == 0 {
        return Err(invalid_capability());
    }
    let (mut control, mut revision) = (0, 0);
    // SAFETY: descriptor came from successful GetSecurityInfo and remains owned.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 || acl.is_null() || unsafe { (*acl).AceCount } == 0 {
        return Err(invalid_capability());
    }
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let mut ace = std::ptr::null_mut();
        // SAFETY: index is inside validated ACL's native ACE count.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        if allowed.Header.AceType != 0
            || allowed.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
            || unsafe {
                EqualSid(
                    std::ptr::addr_of!(allowed.SidStart).cast_mut().cast(),
                    expected,
                )
            } == 0
        {
            return Err(invalid_capability());
        }
    }
    Ok(())
}

fn invalid_capability() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid inherited workspace lease capability",
    )
}
