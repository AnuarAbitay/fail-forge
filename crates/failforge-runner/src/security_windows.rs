//! Owner-only Windows security supplied atomically when capture files are created.
use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn check(value: i32) -> io::Result<()> {
    if value == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn current_sid() -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: query only the current process token into valid HANDLE storage.
    unsafe {
        check(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?;
    }
    // SAFETY: successful OpenProcessToken created a unique real handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    use std::os::windows::io::AsRawHandle;
    let mut length = 0;
    // SAFETY: null storage queries the native TokenUser allocation size.
    unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length);
    }
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut storage = vec![0_usize; (length as usize).div_ceil(size_of::<usize>())];
    // SAFETY: aligned storage is at least the native size requested above.
    unsafe {
        check(GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            length,
            &mut length,
        ))?;
    }
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    // SAFETY: user SID points into retained native token information; returned
    // text is a terminated LocalAlloc allocation, owned until conversion ends.
    unsafe {
        check(ConvertSidToStringSidW(user.User.Sid, &mut text))?;
    }
    let _text = LocalAllocation(text.cast());
    let mut count = 0;
    while unsafe { *text.add(count) } != 0 {
        count += 1;
    }
    String::from_utf16(unsafe { std::slice::from_raw_parts(text, count) })
        .map_err(|_| io::ErrorKind::InvalidData.into())
}

pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    let sid = current_sid()?;
    let descriptor_text: Vec<_> = format!("O:{sid}D:P(A;;FA;;;{sid})")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor = null_mut();
    // SAFETY: terminated SDDL and valid output storage; the returned descriptor
    // is retained through CreateFileW and freed even if creation fails.
    unsafe {
        check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_text.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        ))?;
    }
    let _descriptor = LocalAllocation(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name: Vec<_> = OsStr::new(path).encode_wide().chain([0]).collect();
    // SAFETY: buffers and descriptor remain live; CREATE_NEW never replaces a
    // prior file or follows an existing final-component reparse point.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateFileW created this unique owned handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}
