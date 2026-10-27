//! Windows file capabilities. Every descendant open is relative to a retained
//! directory handle, with reparse processing disabled. Paths are display data.
use std::ffi::{OsStr, OsString, c_void};
use std::fs::{File, Metadata};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::*;

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: HANDLE,
    name: *mut UNICODE_STRING,
    attributes: u32,
    security: *mut c_void,
    quality: *mut c_void,
}
#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut HANDLE,
        access: u32,
        attributes: *mut ObjectAttributes,
        status: *mut IoStatus,
        size: *const i64,
        file_attributes: u32,
        sharing: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn NtSetInformationFile(
        handle: HANDLE,
        status: *mut IoStatus,
        information: *const c_void,
        length: u32,
        class: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn checked(value: i32) -> io::Result<()> {
    if value == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

/// A security descriptor granting access only to the current account. The
/// protected DACL is supplied at creation, never installed after exposure.
pub struct PrivateSecurity {
    descriptor: *mut c_void,
    attributes: SECURITY_ATTRIBUTES,
}
impl PrivateSecurity {
    pub fn new() -> io::Result<Self> {
        let sid = current_sid()?;
        let text = wide(OsStr::new(&format!("O:{sid}D:P(A;;FA;;;{sid})")));
        let mut descriptor = null_mut();
        // SAFETY: terminated input and writable returned allocation pointer.
        checked(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        })?;
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }
    pub fn as_ptr(&mut self) -> *mut SECURITY_ATTRIBUTES {
        &mut self.attributes
    }
}
impl Drop for PrivateSecurity {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

pub fn current_sid() -> io::Result<String> {
    token_sid(false)
}

fn registry_name(sid: &str, purpose: char) -> String {
    use sha2::{Digest, Sha256};

    // Reserve path space for runtimes that spawn SDK-relative executables before
    // normalizing them. This routing index never replaces full SID authority.
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = Sha256::digest(sid.as_bytes());
    let mut name = String::with_capacity(21);
    name.push(purpose);
    // Encode exactly 96 hash bits, padding the last base32 group with zeroes.
    for group in 0..20 {
        let mut value = 0_u8;
        for bit in 0..5 {
            let position = group * 5 + bit;
            value <<= 1;
            if position < 96 {
                value |= (digest[position / 8] >> (7 - position % 8)) & 1;
            }
        }
        name.push(char::from(ALPHABET[usize::from(value)]));
    }
    name
}

/// Compact routing name; full SID markers and private ACLs prove ownership.
pub fn campaign_registry_name() -> io::Result<String> {
    Ok(registry_name(&current_sid()?, 'c'))
}

/// Profile scope has a distinct routing prefix and the same full SID authority.
pub fn profile_registry_name() -> io::Result<String> {
    Ok(registry_name(&current_sid()?, 'p'))
}

/// The current token's exact default object owner, which can differ from its
/// user SID for an elevated account. This never broadens private control ACLs.
pub fn current_owner_sid() -> io::Result<String> {
    token_sid(true)
}
fn token_sid(owner: bool) -> io::Result<String> {
    let mut token = null_mut();
    checked(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })?;
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let token = Token(token);
    let class = if owner { TokenOwner } else { TokenUser };
    let mut length = 0;
    unsafe {
        GetTokenInformation(token.0, class, null_mut(), 0, &mut length);
    }
    let mut storage = vec![0_usize; (length as usize).div_ceil(size_of::<usize>())];
    checked(unsafe {
        GetTokenInformation(
            token.0,
            class,
            storage.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    })?;
    let sid = if owner {
        unsafe { (*storage.as_ptr().cast::<TOKEN_OWNER>()).Owner }
    } else {
        unsafe { (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    };
    sid_string(sid)
}
fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = null_mut();
    checked(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    let mut length = 0;
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })
        .map_err(|_| invalid("invalid owner SID"));
    unsafe {
        LocalFree(text.cast());
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub volume: u32,
    pub index: u64,
    pub volume_serial: u64,
    pub file_id: [u8; 16],
}
pub fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    checked(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) })?;
    Ok(info)
}
pub fn identity(file: &File) -> io::Result<Identity> {
    let info = information(file)?;
    let mut full = FILE_ID_INFO::default();
    checked(unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut full as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    })?;
    Ok(Identity {
        volume: info.dwVolumeSerialNumber,
        index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        volume_serial: full.VolumeSerialNumber,
        file_id: full.FileId.Identifier,
    })
}
pub fn is_reparse(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
fn require_kind(file: &File, directory: Option<bool>) -> io::Result<()> {
    let info = information(file)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid("reparse points are outside supported workspaces"));
    }
    if directory.is_some_and(|dir| dir != (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0)) {
        return Err(invalid("unexpected workspace entry type"));
    }
    Ok(())
}
/// Validate current-account ownership; private entries additionally require a
/// protected DACL containing only current-account full-access allow ACEs.
pub fn require_owned(file: &File, private: bool) -> io::Result<()> {
    let mut owner = null_mut();
    let mut acl = null_mut();
    let mut descriptor = null_mut();
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    struct Descriptor(*mut c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    let _descriptor = Descriptor(descriptor);
    let expected = current_sid()?;
    let actual = sid_string(owner)?;
    if actual != expected && (private || actual != current_owner_sid()?) {
        return Err(invalid("wrong workspace owner"));
    }
    if !private {
        return Ok(());
    }
    let mut control = 0;
    let mut revision = 0;
    checked(unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) })?;
    if control & SE_DACL_PROTECTED == 0 || acl.is_null() || unsafe { (*acl).AceCount } == 0 {
        return Err(invalid("workspace DACL is not private"));
    }
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let mut ace = null_mut();
        checked(unsafe { GetAce(acl, index, &mut ace) })?;
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        if allowed.Header.AceType != 0
            || allowed.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
            || sid_string(std::ptr::addr_of!(allowed.SidStart).cast_mut().cast())? != expected
        {
            return Err(invalid("workspace DACL grants foreign access"));
        }
    }
    Ok(())
}
pub fn require_private(file: &File, directory: bool) -> io::Result<()> {
    require_kind(file, Some(directory))?;
    require_owned(file, true)?;
    if !directory && information(file)?.nNumberOfLinks != 1 {
        return Err(invalid("workspace file has unexpected hard links"));
    }
    Ok(())
}
fn valid_name(name: &OsStr) -> io::Result<Vec<u16>> {
    let text: Vec<u16> = name.encode_wide().collect();
    if text.is_empty()
        || text.len() > 32760
        || text.iter().any(|c| matches!(*c, 0 | 47 | 92 | 58))
        || name == "."
        || name == ".."
    {
        return Err(invalid(
            "workspace operation requires one ordinary entry name",
        ));
    }
    Ok(text)
}
fn open_at(
    parent: &File,
    name: &OsStr,
    directory: Option<bool>,
    writable: bool,
    create: bool,
    delete: bool,
) -> io::Result<File> {
    let mut text = valid_name(name)?;
    let mut unicode = UNICODE_STRING {
        Length: (text.len() * 2) as u16,
        MaximumLength: (text.len() * 2) as u16,
        Buffer: text.as_mut_ptr(),
    };
    let security = if create {
        Some(PrivateSecurity::new()?)
    } else {
        None
    };
    let mut attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: parent.as_raw_handle(),
        name: &mut unicode,
        attributes: 0x40 | 0x1000,
        security: security.as_ref().map_or(null_mut(), |v| v.descriptor),
        quality: null_mut(),
    };
    let mut handle = null_mut();
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    let access =
        0x0012_0081 | if writable { 0x116 } else { 0 } | if delete { 0x0001_0000 } else { 0 };
    // FILE_OPEN_REPARSE_POINT plus OBJ_DONT_REPARSE rejects aliases at the
    // kernel boundary. RootDirectory binds this name to the retained parent.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &mut attributes,
            &mut status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            if create { 2 } else { 1 },
            0x0020_0020
                | match directory {
                    Some(true) => 1,
                    Some(false) => 0x40,
                    None => 0,
                },
            null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    let file = unsafe { File::from_raw_handle(handle) };
    require_kind(&file, directory)?;
    Ok(file)
}
pub fn directory_at(parent: &File, name: &OsStr) -> io::Result<File> {
    open_at(parent, name, Some(true), false, false, false)
}
pub fn create_directory_at(parent: &File, name: &OsStr) -> io::Result<File> {
    open_at(parent, name, Some(true), true, true, true)
}
pub fn regular_at(parent: &File, name: &OsStr, writable: bool, create: bool) -> io::Result<File> {
    open_at(parent, name, Some(false), writable, create, writable)
}
fn root_handle(path: &Path) -> io::Result<File> {
    let text = wide(path.as_os_str());
    let handle = unsafe {
        CreateFileW(
            text.as_ptr(),
            FILE_GENERIC_READ | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    require_kind(&file, Some(true))?;
    Ok(file)
}
pub fn directory(path: &Path) -> io::Result<File> {
    let absolute = std::path::absolute(path)?;
    let mut components = absolute.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(invalid("workspace needs a Windows volume"));
    };
    if components.next() != Some(Component::RootDir) {
        return Err(invalid("workspace root must be absolute"));
    }
    let mut volume = prefix.as_os_str().to_os_string();
    volume.push("\\");
    let mut file = root_handle(Path::new(&volume))?;
    for part in components {
        match part {
            Component::Normal(name) => file = directory_at(&file, name)?,
            _ => return Err(invalid("workspace root contains traversal")),
        }
    }
    Ok(file)
}
pub fn regular(path: &Path, writable: bool) -> io::Result<File> {
    let parent = directory(path.parent().ok_or_else(|| invalid("file has no parent"))?)?;
    regular_at(
        &parent,
        path.file_name()
            .ok_or_else(|| invalid("file has no name"))?,
        writable,
        false,
    )
}
pub fn same_entry(parent: &File, name: &OsStr, file: &File) -> io::Result<()> {
    if identity(&open_at(parent, name, None, false, false, false)?)? != identity(file)? {
        return Err(invalid("workspace directory entry changed"));
    }
    Ok(())
}
pub fn names(parent: &File) -> io::Result<Vec<OsString>> {
    // An empty NT name reopens the exact retained object with an independent
    // enumeration cursor; native names do not normalize the Win32 "." alias.
    let file = reopen(
        parent,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE,
        Some(true),
    )?;
    let mut storage = vec![0_u64; 8192];
    let mut result = vec![];
    loop {
        let success = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdBothDirectoryInfo,
                storage.as_mut_ptr().cast(),
                (storage.len() * 8) as u32,
            )
        };
        if success == 0 {
            if io::Error::last_os_error().raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(io::Error::last_os_error());
        }
        let mut offset = 0;
        loop {
            if offset + size_of::<FILE_ID_BOTH_DIR_INFO>() > storage.len() * 8 {
                return Err(invalid("invalid directory enumeration"));
            }
            let info = unsafe {
                &*storage
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_BOTH_DIR_INFO>()
            };
            let length = info.FileNameLength as usize;
            let name_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if length % 2 != 0 || offset + name_offset + length > storage.len() * 8 {
                return Err(invalid("invalid directory name"));
            }
            let name = OsString::from_wide(unsafe {
                std::slice::from_raw_parts(info.FileName.as_ptr(), length / 2)
            });
            if name != "." && name != ".." {
                valid_name(&name)?;
                result.push(name);
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            if (info.NextEntryOffset as usize) < name_offset + length {
                return Err(invalid("invalid directory entry offset"));
            }
            offset += info.NextEntryOffset as usize;
        }
    }
    result.sort();
    Ok(result)
}
/// Rename this exact retained object relative to another retained directory.
pub fn rename_handle(file: &File, parent: &File, to: &OsStr, replace: bool) -> io::Result<()> {
    let file = deletion_handle(file)?;
    let name = valid_name(to)?;
    // NT requires at least the complete fixed record plus the UTF-16 name,
    // including the ABI padding after FileName[1] for short entry names.
    let length = size_of::<FILE_RENAME_INFO>() + name.len() * 2;
    let mut storage = vec![0_usize; length.div_ceil(size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.Flags = if replace { 0x1 | 0x2 | 0x40 } else { 0 };
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
    }
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    let result = unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            info.cast(),
            length as u32,
            65,
        )
    };
    if result < 0 {
        Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ))
    } else {
        Ok(())
    }
}
fn rename_impl(parent: &File, from: &OsStr, to: &OsStr, replace: bool) -> io::Result<()> {
    let file = open_at(parent, from, None, false, false, true)?;
    rename_handle(&file, parent, to, replace)
}
pub fn rename_at(parent: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    rename_impl(parent, from, to, false)
}
pub fn replace_at(parent: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    rename_impl(parent, from, to, true)
}
fn deletion_handle(file: &File) -> io::Result<File> {
    reopen(file, DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, None)
}
fn reopen(file: &File, access: u32, directory: Option<bool>) -> io::Result<File> {
    let mut name = UNICODE_STRING {
        Length: 0,
        MaximumLength: 0,
        Buffer: null_mut(),
    };
    let mut attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: file.as_raw_handle(),
        name: &mut name,
        attributes: 0x40 | 0x1000,
        security: null_mut(),
        quality: null_mut(),
    };
    let mut handle = null_mut();
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &mut attributes,
            &mut status,
            null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            1,
            0x0020_0020
                | match directory {
                    Some(true) => 1,
                    Some(false) => 0x40,
                    None => 0,
                },
            null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    let reopened = unsafe { File::from_raw_handle(handle) };
    require_kind(&reopened, directory)?;
    if identity(&reopened)? != identity(file)? {
        return Err(invalid("reopened workspace object changed"));
    }
    Ok(reopened)
}
/// Mark only this exact retained entry for removal, without a pathname lookup.
pub fn remove_handle(file: &File) -> io::Result<()> {
    let file = deletion_handle(file)?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    checked(unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&info as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    })
}
pub fn remove_at(parent: &File, name: &OsStr, directory: bool) -> io::Result<()> {
    let file = open_at(parent, name, Some(directory), false, false, true)?;
    remove_handle(&file)
}
pub fn try_lock(file: &File) -> io::Result<bool> {
    try_lock_at(file, false, 0)
}
pub fn try_lock_at(file: &File, shared: bool, offset: u64) -> io::Result<bool> {
    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous.Offset = offset as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    if unsafe {
        LockFileEx(
            file.as_raw_handle(),
            if shared {
                LOCKFILE_FAIL_IMMEDIATELY
            } else {
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY
            },
            0,
            1,
            0,
            &mut overlapped,
        )
    } != 0
    {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(error)
    }
}
pub fn random_token() -> io::Result<String> {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut bytes = [0_u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// True only when the retained object has already lost its directory entry.
pub fn unlinked(file: &File) -> io::Result<bool> {
    let mut info = FILE_STANDARD_INFO::default();
    checked(unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&mut info as *mut FILE_STANDARD_INFO).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    })?;
    Ok(info.NumberOfLinks == 0 || info.DeletePending)
}

/// Create a new hard-link entry to this exact retained object; never replace.
pub fn link_handle(file: &File, parent: &File, to: &OsStr) -> io::Result<()> {
    let file = deletion_handle(file)?;
    let name = valid_name(to)?;
    // NT requires at least the complete fixed record plus the UTF-16 name,
    // including the ABI padding after FileName[1] for short entry names.
    let length = size_of::<FILE_RENAME_INFO>() + name.len() * 2;
    let mut storage = vec![0_usize; length.div_ceil(size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
    }
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    let result = unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            info.cast(),
            length as u32,
            11,
        )
    };
    if result < 0 {
        Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ))
    } else {
        Ok(())
    }
}

/// Change only the read-only attribute of this exact retained object. An
/// attribute-only reopen also works when the file currently denies data writes.
pub fn set_readonly(file: &File, readonly: bool) -> io::Result<()> {
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    let attributes = information(&file)?.dwFileAttributes;
    let attributes = if readonly {
        attributes | FILE_ATTRIBUTE_READONLY
    } else {
        attributes & !FILE_ATTRIBUTE_READONLY
    };
    let basic = FILE_BASIC_INFO {
        FileAttributes: if attributes == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attributes
        },
        ..FILE_BASIC_INFO::default()
    };
    checked(unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileBasicInfo,
            (&basic as *const FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_routing_names_are_fixed_lowercase_and_account_scoped() {
        let sid = "S-1-5-21-3776380004-1005456608-3524014338-500";
        assert_eq!(registry_name(sid, 'c'), "ctjzawtlaiojefj35d7ua");
        assert_eq!(registry_name(sid, 'p'), "ptjzawtlaiojefj35d7ua");
        assert_ne!(registry_name(sid, 'c'), registry_name(sid, 'p'));
        let other = registry_name("S-1-5-21-3776380004-1005456608-3524014338-501", 'c');
        assert_eq!(other, "cji2x7e2iwbaqa537yeyq");
        assert_ne!(registry_name(sid, 'c'), other);
        for name in [
            campaign_registry_name().unwrap(),
            profile_registry_name().unwrap(),
        ] {
            assert_eq!(name.len(), 21);
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
            );
        }
        let current = current_sid().unwrap();
        assert_eq!(
            campaign_registry_name().unwrap(),
            registry_name(&current, 'c')
        );
        assert_eq!(
            profile_registry_name().unwrap(),
            registry_name(&current, 'p')
        );
    }

    #[test]
    fn retained_child_blocks_parent_rename_until_its_handle_closes() {
        let root = std::env::temp_dir().join(format!("ff-dir-reopen-{}", random_token().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let parent = directory(&root).unwrap();
        let child = create_directory_at(&parent, OsStr::new("from")).unwrap();
        let payload = regular_at(&child, OsStr::new("payload"), true, true).unwrap();
        assert_eq!(names(&child).unwrap(), vec![OsString::from("payload")]);
        assert_eq!(names(&child).unwrap(), vec![OsString::from("payload")]);
        let error = rename_handle(&child, &parent, OsStr::new("to"), false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        same_entry(&parent, OsStr::new("from"), &child).unwrap();
        same_entry(&child, OsStr::new("payload"), &payload).unwrap();
        assert!(!root.join("to").exists());
        drop(payload);
        rename_handle(&child, &parent, OsStr::new("to"), false).unwrap();
        same_entry(&parent, OsStr::new("to"), &child).unwrap();
        assert!(!root.join("from").exists());
        assert!(root.join("to").join("payload").is_file());
        drop(child);
        drop(parent);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_character_rename_and_hard_link_names_preserve_native_identity() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("ff-short-name-{}", random_token().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let parent = directory(&root).unwrap();
        let mut file = regular_at(&parent, OsStr::new("a"), true, true).unwrap();
        file.write_all(b"native bytes").unwrap();
        file.sync_all().unwrap();
        rename_handle(&file, &parent, OsStr::new("b"), false).unwrap();
        same_entry(&parent, OsStr::new("b"), &file).unwrap();
        assert!(!root.join("a").exists());
        link_handle(&file, &parent, OsStr::new("c")).unwrap();
        same_entry(&parent, OsStr::new("c"), &file).unwrap();
        assert_eq!(information(&file).unwrap().nNumberOfLinks, 2);
        assert_eq!(std::fs::read(root.join("b")).unwrap(), b"native bytes");
        assert_eq!(std::fs::read(root.join("c")).unwrap(), b"native bytes");
        assert_eq!(
            names(&parent).unwrap(),
            vec![OsString::from("b"), OsString::from("c")]
        );
        drop(file);
        drop(parent);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn payload_default_token_owner_is_accepted_but_control_dacl_stays_private() {
        let root = std::env::temp_dir().join(format!("ff-owner-{}", random_token().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let parent = directory(&root).unwrap();
        require_owned(&parent, false).unwrap();
        let source = root.join("payload");
        std::fs::write(&source, "ordinary payload").unwrap();
        let payload = regular(&source, false).unwrap();
        require_owned(&payload, false).unwrap();
        // An ordinary inherited DACL does not become a private control just
        // because its account's default owner is authorized for payloads.
        assert!(require_private(&payload, false).is_err());
        let control = regular_at(&parent, OsStr::new("control"), true, true).unwrap();
        require_private(&control, false).unwrap();
        drop(control);
        drop(payload);
        drop(parent);
        std::fs::remove_dir_all(root).unwrap();
    }
}
