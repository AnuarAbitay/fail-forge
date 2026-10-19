//! Fresh campaign copies, verified before rebasing prepared package metadata.
//!
//! Traversal and cleanup are relative to retained directory descriptors. This
//! prevents payload aliases from redirecting copy or cleanup outside its root;
//! it is not a sandbox against another process running as the same user.
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File, FileTimes};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::inputs::{dependency_roots_from_value, file_uri};
use crate::{
    Cancellation, InputError, PreparedProject, failure, inspect_project, parse_json, source_hashes,
    verify_source,
};

const DIRECTORY_FLAGS: i32 =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const FILE_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory { mode: u32 },
    File { hash: String, mode: u32 },
}
type Inventory = BTreeMap<PathBuf, Entry>;

#[derive(Clone, Copy)]
enum Stage {
    Inspected,
    Inventoried,
    Copied,
    Verified,
}

/// Create one fresh package copy. The destination must not exist and its parent
/// must be a private directory owned by the current user.
pub fn copy_project(
    source: &Path,
    destination: &Path,
    cancel: &Cancellation,
) -> Result<PreparedProject, InputError> {
    copy_with(source, destination, cancel, |_, _, _| {})
}

/// Freeze a campaign source using the same byte and inventory checks as each
/// attempt. Only verified copied package metadata is subsequently rewritten.
pub fn copy_campaign_snapshot(
    source: &Path,
    destination: &Path,
    cancel: &Cancellation,
) -> Result<PreparedProject, InputError> {
    copy_project(source, destination, cancel)
}

fn copy_with(
    source: &Path,
    destination: &Path,
    cancel: &Cancellation,
    mut hook: impl FnMut(Stage, &Path, &Path),
) -> Result<PreparedProject, InputError> {
    cancel.check()?;
    let project = inspect_project(source, cancel)?;
    hook(Stage::Inspected, &project.root, destination);
    let original = open_directory_at(libc::AT_FDCWD, project.root.as_os_str())?;
    let destination_name = destination
        .file_name()
        .ok_or_else(|| failure("snapshot destination needs a file name"))?;
    let parent_path = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    let destination = parent_path.join(destination_name);
    if destination.starts_with(&project.root) || project.root.starts_with(&destination) {
        return Err(failure("snapshot source and destination overlap"));
    }
    let parent = open_directory_at(libc::AT_FDCWD, parent_path.as_os_str())?;
    let parent_info = parent.metadata()?;
    // SAFETY: geteuid takes no pointers and has no side effects.
    if parent_info.uid() != unsafe { libc::geteuid() } || parent_info.mode() & 0o7777 != 0o700 {
        return Err(failure(
            "snapshot destination parent must be private and owned",
        ));
    }
    match entry_info(&parent, destination_name) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "snapshot destination already exists",
            )
            .into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let before = source_hashes(&project.root, cancel)?;
    let frozen = inventory(&original, cancel, false)?;
    hook(Stage::Inventoried, &project.root, &destination);
    cancel.check()?;
    mkdir_at(&parent, destination_name)?;
    let copied = open_directory_at(parent.as_raw_fd(), destination_name)?;
    let result = (|| {
        copy_tree(&original, &copied, Path::new(""), cancel, 0)?;
        hook(Stage::Copied, &project.root, &destination);
        if inventory(&copied, cancel, true)? != frozen {
            return Err(failure(
                "copied snapshot inputs do not match frozen project",
            ));
        }
        // Both inventories are checked before metadata rebase: otherwise a
        // transient config edit could be hidden by intentional serialization.
        if source_hashes(&destination, cancel)? != before {
            return Err(failure("copied snapshot source inventory changed"));
        }
        verify_source(&project.root, &before, cancel)?;
        if inventory(&original, cancel, false)? != frozen {
            return Err(failure("prepared project inputs changed during copying"));
        }
        hook(Stage::Verified, &project.root, &destination);
        same_entry(&parent, destination_name, &copied)?;
        let tool = open_directory_at(copied.as_raw_fd(), OsStr::new(".dart_tool"))?;
        let mut raw = Vec::new();
        open_regular_at(&tool, OsStr::new("package_config.json"), libc::O_RDONLY)?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut raw)?;
        let config_relative = Path::new(".dart_tool/package_config.json");
        let config_mode = match frozen.get(config_relative) {
            Some(Entry::File { hash, mode }) if *hash == format!("{:x}", Sha256::digest(&raw)) => {
                *mode
            }
            _ => return Err(failure("copied package metadata changed before rebase")),
        };
        let mut value = parse_json(&raw)?;
        if dependency_roots_from_value(&project.root.join(config_relative), &value)?
            != project.packages
        {
            return Err(failure(
                "copied dependency metadata differs from inspected project",
            ));
        }
        let graph_relative = Path::new(".dart_tool/package_graph.json");
        let mut graph_bytes = Vec::new();
        open_regular_at(&tool, OsStr::new("package_graph.json"), libc::O_RDONLY)?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut graph_bytes)?;
        match frozen.get(graph_relative) {
            Some(Entry::File { hash, .. })
                if *hash == format!("{:x}", Sha256::digest(&graph_bytes)) => {}
            _ => return Err(failure("copied package graph changed before rebase")),
        }
        if parse_json(&graph_bytes)? != project.graph {
            return Err(failure(
                "copied package graph differs from inspected project",
            ));
        }
        for row in value["packages"]
            .as_array_mut()
            .ok_or_else(|| failure("invalid snapshot package list"))?
        {
            let name = row["name"]
                .as_str()
                .ok_or_else(|| failure("invalid snapshot package name"))?;
            row["rootUri"] = if name == project.name {
                json!("../")
            } else {
                json!(file_uri(
                    project
                        .packages
                        .get(name)
                        .ok_or_else(|| failure("snapshot dependency changed"))?
                )?)
            };
        }
        let rebased = serde_json::to_vec(&value)
            .map_err(|_| failure("cannot serialize prepared metadata"))?;
        let config_file =
            open_regular_at(&tool, OsStr::new("package_config.json"), libc::O_RDONLY)?;
        require_private_file(&config_file)?;
        config_file.set_permissions(fs::Permissions::from_mode(config_mode | 0o200))?;
        let mut output = open_regular_at(&tool, OsStr::new("package_config.json"), libc::O_WRONLY)?;
        require_private_file(&output)?;
        let input_metadata = config_file.metadata()?;
        let output_metadata = output.metadata()?;
        if input_metadata.dev() != output_metadata.dev()
            || input_metadata.ino() != output_metadata.ino()
        {
            return Err(failure("copied package metadata changed before rebase"));
        }
        output.set_len(0)?;
        output.write_all(&rebased)?;
        output.flush()?;
        output.set_permissions(fs::Permissions::from_mode(config_mode))?;
        same_entry(&parent, destination_name, &copied)?;
        let prepared = inspect_project(&destination, cancel)?;
        // A source edit while the copied metadata was being rebased must still
        // fail the snapshot. The copied metadata never re-reads original bytes.
        verify_source(&project.root, &before, cancel)?;
        if inventory(&original, cancel, false)? != frozen {
            return Err(failure("prepared project inputs changed during copying"));
        }
        let mut expected = frozen;
        expected.insert(
            config_relative.into(),
            Entry::File {
                hash: format!("{:x}", Sha256::digest(&rebased)),
                mode: config_mode,
            },
        );
        if inventory(&copied, cancel, true)? != expected {
            return Err(failure("copied snapshot changed during metadata rebase"));
        }
        same_entry(&parent, destination_name, &copied)?;
        Ok(prepared)
    })();
    if result.is_err() {
        // Cleanup is independent of cancellation. A replaced destination is
        // never traversed or unlinked under the authority of the old inode.
        if same_entry(&parent, destination_name, &copied).is_ok() {
            let _ = remove_contents(&copied, 0).and_then(|()| {
                same_entry(&parent, destination_name, &copied)?;
                unlink_at(&parent, destination_name, true).map_err(InputError::from)
            });
        }
    }
    result
}

fn skipped(relative: &Path) -> bool {
    let parts: Vec<_> = relative.components().map(|part| part.as_os_str()).collect();
    parts.iter().any(|part| {
        [".git", "__pycache__", ".failforge"]
            .iter()
            .any(|name| part == name)
    }) || parts.first().is_some_and(|part| *part == "build")
        || parts.len() >= 2 && parts[0] == ".dart_tool" && parts[1] == "test"
        || (parts.len() == 1
            || parts[..parts.len().saturating_sub(1)].contains(&OsStr::new(".dart_tool")))
            && relative.file_name().is_some_and(|name| {
                name.as_bytes().starts_with(b"incremental_kernel")
                    && !name.as_bytes().ends_with(b".dart")
            })
}

fn inventory(
    root: &File,
    cancel: &Cancellation,
    owned_copy: bool,
) -> Result<Inventory, InputError> {
    fn visit(
        root: &File,
        prefix: &Path,
        result: &mut Inventory,
        cancel: &Cancellation,
        depth: usize,
        owned_copy: bool,
    ) -> Result<(), InputError> {
        check_depth(depth)?;
        cancel.check()?;
        for name in names(root)? {
            let relative = prefix.join(&name);
            if skipped(&relative) {
                continue;
            }
            let info = entry_info(root, &name)?;
            match info.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    let child = open_directory_at(root.as_raw_fd(), &name)?;
                    same_entry(root, &name, &child)?;
                    result.insert(
                        relative.clone(),
                        Entry::Directory {
                            mode: child.metadata()?.mode() & 0o777,
                        },
                    );
                    visit(&child, &relative, result, cancel, depth + 1, owned_copy)?;
                    same_entry(root, &name, &child)?;
                }
                libc::S_IFREG => {
                    let mut file = open_regular_at(root, &name, libc::O_RDONLY)?;
                    if owned_copy {
                        require_private_file(&file)?;
                    }
                    let mut hash = Sha256::new();
                    let mut buffer = [0; 64 * 1024];
                    loop {
                        cancel.check()?;
                        let read = file.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        hash.update(&buffer[..read]);
                    }
                    same_entry(root, &name, &file)?;
                    result.insert(
                        relative,
                        Entry::File {
                            hash: format!("{:x}", hash.finalize()),
                            mode: file.metadata()?.mode() & 0o777,
                        },
                    );
                }
                _ => {
                    return Err(failure(
                        "snapshot inputs must be regular files or directories",
                    ));
                }
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    result.insert(
        PathBuf::new(),
        Entry::Directory {
            mode: root.metadata()?.mode() & 0o777,
        },
    );
    visit(root, Path::new(""), &mut result, cancel, 0, owned_copy)?;
    Ok(result)
}

fn copy_tree(
    source: &File,
    destination: &File,
    prefix: &Path,
    cancel: &Cancellation,
    depth: usize,
) -> Result<(), InputError> {
    check_depth(depth)?;
    cancel.check()?;
    let directory_metadata = source.metadata()?;
    for name in names(source)? {
        let relative = prefix.join(&name);
        if skipped(&relative) {
            continue;
        }
        let info = entry_info(source, &name)?;
        match info.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let input = open_directory_at(source.as_raw_fd(), &name)?;
                same_entry(source, &name, &input)?;
                mkdir_at(destination, &name)?;
                let output = open_directory_at(destination.as_raw_fd(), &name)?;
                copy_tree(&input, &output, &relative, cancel, depth + 1)?;
                same_entry(source, &name, &input)?;
            }
            libc::S_IFREG => {
                let mut input = open_regular_at(source, &name, libc::O_RDONLY)?;
                let metadata = input.metadata()?;
                let mut output = open_regular_at(
                    destination,
                    &name,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                )?;
                let mut buffer = [0; 64 * 1024];
                loop {
                    cancel.check()?;
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    output.write_all(&buffer[..read])?;
                }
                output.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o777))?;
                output.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
                same_entry(source, &name, &input)?;
            }
            _ => {
                return Err(failure(
                    "snapshot inputs must be regular files or directories",
                ));
            }
        }
    }
    // Creating children changes their parent's timestamp. Restore it only once
    // the directory is complete, including empty directories and the copy root.
    destination.set_permissions(fs::Permissions::from_mode(
        directory_metadata.mode() & 0o777,
    ))?;
    destination.set_times(FileTimes::new().set_modified(directory_metadata.modified()?))?;
    Ok(())
}

fn check_depth(depth: usize) -> Result<(), InputError> {
    if depth > 128 {
        Err(failure("snapshot tree exceeds supported depth"))
    } else {
        Ok(())
    }
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid snapshot path"))
}

fn open_at(parent: i32, name: &OsStr, flags: i32) -> io::Result<File> {
    let name = c_name(name)?;
    // SAFETY: name is a live C string; successful openat returns an owned fd.
    let descriptor = unsafe { libc::openat(parent, name.as_ptr(), flags, 0o600) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful descriptor has exactly this Rust owner.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn open_directory_at(parent: i32, name: &OsStr) -> Result<File, InputError> {
    Ok(open_at(parent, name, DIRECTORY_FLAGS)?)
}

fn open_regular_at(parent: &File, name: &OsStr, flags: i32) -> Result<File, InputError> {
    let file = open_at(parent.as_raw_fd(), name, flags | FILE_FLAGS)?;
    if !file.metadata()?.is_file() {
        return Err(failure("snapshot input must be a regular file"));
    }
    Ok(file)
}

fn require_private_file(file: &File) -> Result<(), InputError> {
    let info = file.metadata()?;
    // SAFETY: geteuid has no pointers or side effects.
    if info.uid() != unsafe { libc::geteuid() } || info.nlink() != 1 {
        return Err(failure(
            "copied snapshot file has unexpected ownership or links",
        ));
    }
    Ok(())
}

fn entry_info(parent: &File, name: &OsStr) -> io::Result<libc::stat> {
    let name = c_name(name)?;
    let mut info = std::mem::MaybeUninit::uninit();
    // SAFETY: parent is live, name terminated and info writable.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized info.
    Ok(unsafe { info.assume_init() })
}

fn same_entry(parent: &File, name: &OsStr, file: &File) -> Result<(), InputError> {
    let current = entry_info(parent, name)?;
    let opened = file.metadata()?;
    if current.st_dev as u64 != opened.dev() || current.st_ino != opened.ino() {
        return Err(failure("snapshot directory entry changed"));
    }
    Ok(())
}

fn mkdir_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: parent is retained and name is a live C string.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unlink_at(parent: &File, name: &OsStr, directory: bool) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: unlinkat does not follow the final entry; parent remains open.
    if unsafe {
        libc::unlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            if directory { libc::AT_REMOVEDIR } else { 0 },
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn names(directory: &File) -> io::Result<Vec<OsString>> {
    let copy = open_at(directory.as_raw_fd(), OsStr::new("."), DIRECTORY_FLAGS)?;
    let descriptor = copy.into_raw_fd();
    // SAFETY: fdopendir takes ownership of the descriptor on success.
    let pointer = unsafe { libc::fdopendir(descriptor) };
    if pointer.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: failed fdopendir leaves the caller owning this descriptor.
        unsafe {
            libc::close(descriptor);
        }
        return Err(error);
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: this object uniquely owns the successful fdopendir stream.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let guard = Directory(pointer);
    let mut result = Vec::new();
    loop {
        // SAFETY: errno is thread-local integer storage.
        unsafe {
            *errno() = 0;
        }
        // SAFETY: readdir borrows a valid directory stream.
        let entry = unsafe { libc::readdir(guard.0) };
        if entry.is_null() {
            // SAFETY: errno is thread-local integer storage.
            if unsafe { *errno() } != 0 {
                return Err(io::Error::last_os_error());
            }
            break;
        }
        // SAFETY: POSIX guarantees a terminated d_name valid until next readdir.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            result.push(OsString::from_vec(name.to_vec()));
        }
    }
    result.sort();
    Ok(result)
}

#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: __error returns thread-local errno storage.
    unsafe { libc::__error() }
}
#[cfg(not(target_os = "macos"))]
unsafe fn errno() -> *mut libc::c_int {
    // SAFETY: __errno_location returns thread-local errno storage on Linux.
    unsafe { libc::__errno_location() }
}

fn remove_contents(directory: &File, depth: usize) -> Result<(), InputError> {
    check_depth(depth)?;
    // SAFETY: geteuid takes no pointers and has no side effects.
    let uid = unsafe { libc::geteuid() };
    let metadata = directory.metadata()?;
    if metadata.uid() != uid {
        return Err(failure("snapshot cleanup found foreign ownership"));
    }
    if metadata.mode() & 0o700 != 0o700 {
        // This retained descriptor identifies an owned copied directory. Never
        // follow a payload alias or change an external path to enable removal.
        directory.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o777 | 0o700))?;
    }
    for name in names(directory)? {
        let info = entry_info(directory, &name)?;
        if info.st_uid != uid {
            return Err(failure("snapshot cleanup found foreign ownership"));
        }
        if info.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = open_directory_at(directory.as_raw_fd(), &name)?;
            same_entry(directory, &name, &child)?;
            remove_contents(&child, depth + 1)?;
            same_entry(directory, &name, &child)?;
            unlink_at(directory, &name, true)?;
        } else {
            // Removing an alias only removes its own directory entry.
            unlink_at(directory, &name, false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;
