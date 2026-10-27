//! Whole-file native Windows publication through pinned directory handles.
//! File contents are flushed before atomic rename. Namespace checks detect
//! observed concurrent edits; they do not lock unrelated namespace writers.
use crate::ProfileError;
use failforge_workspace::windows_fs::{self as win, Identity};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
};

type Result<T> = std::result::Result<T, ProfileError>;
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    identity: Identity,
    size: u64,
    attributes: u32,
    modified: i64,
    changed: i64,
    digest: [u8; 32],
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputBinding {
    project: PathBuf,
    output: PathBuf,
    ancestors: Vec<Identity>,
    parents: Vec<Identity>,
    pubspec: Option<Entry>,
    destination: Option<Entry>,
}
fn config(message: &str) -> ProfileError {
    ProfileError::Config(message.into())
}
fn paths(project: &Path, output: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let project = std::path::absolute(project)?;
    let output = std::path::absolute(output)?;
    let relative = output
        .strip_prefix(&project)
        .map_err(|_| config("profile output must be inside the source package"))?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(config(
            "profile output must name a file inside the source package",
        ));
    }
    Ok((project.clone(), output.clone(), relative.into()))
}
fn identity_chain(project: &Path) -> Result<Vec<Identity>> {
    project
        .ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|path| Ok(win::identity(&win::directory(path)?)?))
        .collect()
}
fn file_state(file: &File) -> Result<(Identity, u64, u32, i64, i64)> {
    let info = win::information(file)?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: a live owned file and ABI-matching writable output record.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok((
        win::identity(file)?,
        (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        info.dwFileAttributes,
        basic.LastWriteTime,
        basic.ChangeTime,
    ))
}
fn entry(parent: &File, name: &OsStr) -> Result<Option<Entry>> {
    let mut file = match win::regular_at(parent, name, false, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let before = file_state(&file)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    if file_state(&file)? != before {
        return Err(config(
            "profile destination changed while binding its contents",
        ));
    }
    win::same_entry(parent, name, &file)?;
    Ok(Some(Entry {
        identity: before.0,
        size: before.1,
        attributes: before.2,
        modified: before.3,
        changed: before.4,
        digest: digest.finalize().into(),
    }))
}
fn no_nested_package(parent: &File) -> Result<()> {
    if entry(parent, OsStr::new("pubspec.yaml"))?.is_some() {
        return Err(config(
            "profile output must remain in the selected source package",
        ));
    }
    Ok(())
}
pub fn capture_output_binding(project: &Path, output: &Path) -> Result<OutputBinding> {
    let (project, output, relative) = paths(project, output)?;
    let ancestors = identity_chain(&project)?;
    let mut parent = win::directory(&project)?;
    let pubspec = entry(&parent, OsStr::new("pubspec.yaml"))?;
    let mut parents = Vec::new();
    let mut complete = true;
    for part in relative.parent().unwrap().components() {
        match win::directory_at(&parent, part.as_os_str()) {
            Ok(directory) => {
                no_nested_package(&directory)?;
                parents.push(win::identity(&directory)?);
                parent = directory;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                complete = false;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let destination = if complete {
        entry(&parent, relative.file_name().unwrap())?
    } else {
        None
    };
    Ok(OutputBinding {
        project,
        output,
        ancestors,
        parents,
        pubspec,
        destination,
    })
}
pub fn check_destination(path: &Path, force: bool) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || win::is_reparse(&metadata) {
        return Err(config(
            "profile output must be a regular file, not a symlink or special file",
        ));
    }
    if !force {
        return Err(config(
            "profile output already exists; use --force to replace it",
        ));
    }
    Ok(())
}
struct Parent {
    root: File,
    directory: File,
    parents: Vec<Identity>,
    created: Vec<(File, OsString, File)>,
    committed: bool,
}
impl Drop for Parent {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for (parent, name, file) in self.created.iter().rev() {
            if win::same_entry(parent, name, file).is_ok()
                && win::names(file).is_ok_and(|names| names.is_empty())
            {
                let _ = win::remove_handle(file);
            }
        }
    }
}
fn parent(binding: &OutputBinding, relative: &Path) -> Result<Parent> {
    if identity_chain(&binding.project)? != binding.ancestors {
        return Err(config(
            "profile package directory changed before publication",
        ));
    }
    let root = win::directory(&binding.project)?;
    if entry(&root, OsStr::new("pubspec.yaml"))? != binding.pubspec {
        return Err(config("package pubspec.yaml changed before publication"));
    }
    let mut result = Parent {
        directory: root.try_clone()?,
        root,
        parents: Vec::new(),
        created: Vec::new(),
        committed: false,
    };
    for (index, part) in relative.parent().unwrap().components().enumerate() {
        let name = part.as_os_str();
        let child = match win::directory_at(&result.directory, name) {
            Ok(child) => child,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && index >= binding.parents.len() =>
            {
                let child = win::create_directory_at(&result.directory, name)?;
                result.created.push((
                    result.directory.try_clone()?,
                    name.into(),
                    child.try_clone()?,
                ));
                child
            }
            Err(error) => return Err(error.into()),
        };
        no_nested_package(&child)?;
        let identity = win::identity(&child)?;
        if binding
            .parents
            .get(index)
            .is_some_and(|expected| *expected != identity)
        {
            return Err(config(
                "profile parent directory changed before publication",
            ));
        }
        result.parents.push(identity);
        result.directory = child;
    }
    Ok(result)
}
fn verify_namespace(binding: &OutputBinding, parent: &Parent, relative: &Path) -> Result<()> {
    if identity_chain(&binding.project)? != binding.ancestors
        || entry(&parent.root, OsStr::new("pubspec.yaml"))? != binding.pubspec
    {
        return Err(config("profile package changed during publication"));
    }
    let mut current = win::directory(&binding.project)?;
    for (index, part) in relative.parent().unwrap().components().enumerate() {
        current = win::directory_at(&current, part.as_os_str())?;
        no_nested_package(&current)?;
        if Some(&win::identity(&current)?) != parent.parents.get(index) {
            return Err(config(
                "profile parent directory changed during publication",
            ));
        }
    }
    Ok(())
}
fn same_contents(left: &Entry, right: &Entry) -> bool {
    // Creating/removing our backup link changes ctime, but cannot change bytes,
    // the actual file identity, permissions or modification time.
    left.identity == right.identity
        && left.size == right.size
        && left.attributes == right.attributes
        && left.modified == right.modified
        && left.digest == right.digest
}
fn verify_destination(
    parent: &File,
    target: &OsStr,
    expected: Option<&Entry>,
    force: bool,
) -> Result<()> {
    let current = entry(parent, target)?;
    if current.is_some() && !force {
        return Err(config(
            "profile output already exists; use --force to replace it",
        ));
    }
    if match (expected, current.as_ref()) {
        (None, None) => false,
        (Some(left), Some(right)) => left != right,
        _ => true,
    } {
        return Err(config("profile destination changed before publication"));
    }
    Ok(())
}
fn verify_document(parent: &File, target: &OsStr, identity: Identity, bytes: &[u8]) -> Result<()> {
    let document = entry(parent, target)?.ok_or_else(|| config("published profile disappeared"))?;
    if document.identity != identity
        || document.size != bytes.len() as u64
        || document.digest != Sha256::digest(bytes).as_slice()
    {
        return Err(config("profile document changed during publication"));
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    BeforeCommit,
    AfterCommit,
}
pub fn write_profile(
    project: &Path,
    output: &Path,
    text: &str,
    force: bool,
    expected_binding: Option<&OutputBinding>,
) -> Result<()> {
    write_with_hook(project, output, text, force, expected_binding, &mut |_| {
        Ok(())
    })
}
fn write_with_hook(
    project: &Path,
    output: &Path,
    text: &str,
    force: bool,
    expected_binding: Option<&OutputBinding>,
    hook: &mut dyn FnMut(Stage) -> Result<()>,
) -> Result<()> {
    let (project, output, relative) = paths(project, output)?;
    let owned;
    let binding = match expected_binding {
        Some(binding) => binding,
        None => {
            owned = capture_output_binding(&project, &output)?;
            &owned
        }
    };
    if binding.project != project || binding.output != output {
        return Err(config(
            "profile publication binding does not match its destination",
        ));
    }
    let mut parent = parent(binding, &relative)?;
    let target = relative.file_name().unwrap();
    verify_destination(
        &parent.directory,
        target,
        binding.destination.as_ref(),
        force,
    )?;
    let temporary = OsString::from(format!(".failforge-profile-stage-{}", win::random_token()?));
    let mut staged = win::regular_at(&parent.directory, &temporary, true, true)?;
    let staged_identity = win::identity(&staged)?;
    let mut backup: Option<(OsString, File)> = None;
    let mut committed = false;
    let mut expected_destination = binding.destination.clone();
    let result = (|| {
        staged.write_all(text.as_bytes())?;
        staged.flush()?;
        staged.sync_all()?;
        verify_namespace(binding, &parent, &relative)?;
        verify_destination(
            &parent.directory,
            target,
            binding.destination.as_ref(),
            force,
        )?;
        verify_document(
            &parent.directory,
            &temporary,
            staged_identity,
            text.as_bytes(),
        )?;
        if let Some(previous) = &binding.destination {
            let old = win::regular_at(&parent.directory, target, false, false)?;
            let backup_name = OsString::from(format!(
                ".failforge-profile-backup-{}",
                win::random_token()?
            ));
            win::link_handle(&old, &parent.directory, &backup_name)?;
            backup = Some((backup_name.clone(), old.try_clone()?));
            let retained = entry(&parent.directory, &backup_name)?
                .ok_or_else(|| config("profile backup disappeared"))?;
            if !same_contents(previous, &retained) {
                return Err(config("previous profile changed before publication"));
            }
            let backup_file = win::regular_at(&parent.directory, &backup_name, false, false)?;
            backup = Some((backup_name, backup_file));
            expected_destination = entry(&parent.directory, target)?;
            if !expected_destination
                .as_ref()
                .is_some_and(|current| same_contents(previous, current))
            {
                return Err(config("previous profile changed while retaining backup"));
            }
        }
        hook(Stage::BeforeCommit)?;
        verify_namespace(binding, &parent, &relative)?;
        verify_destination(
            &parent.directory,
            target,
            expected_destination.as_ref(),
            force,
        )?;
        win::require_private(&staged, false)?;
        win::rename_handle(&staged, &parent.directory, target, force)?;
        committed = true;
        hook(Stage::AfterCommit)?;
        verify_namespace(binding, &parent, &relative)?;
        verify_document(&parent.directory, target, staged_identity, text.as_bytes())?;
        win::require_private(&staged, false)?;
        if let Some((name, old)) = &backup {
            win::same_entry(&parent.directory, name, old)?;
            win::remove_at(&parent.directory, name, false)?;
        }
        Ok(())
    })();
    if result.is_err() {
        if committed
            && verify_document(&parent.directory, target, staged_identity, text.as_bytes()).is_ok()
        {
            if let Some((name, old)) = &backup {
                if win::same_entry(&parent.directory, name, old).is_ok() {
                    let _ = win::rename_handle(old, &parent.directory, target, true);
                }
            } else {
                let _ = win::remove_handle(&staged);
            }
        }
        if !committed {
            let _ = win::remove_handle(&staged);
        }
        if !committed {
            if let Some((name, old)) = &backup {
                if win::same_entry(&parent.directory, name, old).is_ok() {
                    let _ = win::remove_at(&parent.directory, name, false);
                }
            }
        }
    } else {
        parent.committed = true;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "failforge-profile-output-{}",
                win::random_token().unwrap()
            ));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("pubspec.yaml"), "name: example\n").unwrap();
            Self(root)
        }
        fn output(&self) -> PathBuf {
            self.0.join("profiles/selected.yml")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn native_atomic_publish_creates_whole_document_and_requires_force() {
        let fixture = Fixture::new();
        write_profile(&fixture.0, &fixture.output(), "first\n", false, None).unwrap();
        assert_eq!(std::fs::read(fixture.output()).unwrap(), b"first\n");
        assert!(write_profile(&fixture.0, &fixture.output(), "second\n", false, None).is_err());
        write_profile(&fixture.0, &fixture.output(), "second\n", true, None).unwrap();
        assert_eq!(std::fs::read(fixture.output()).unwrap(), b"second\n");
        assert_eq!(
            std::fs::read_dir(fixture.output().parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }
    #[test]
    fn changed_destination_and_nested_package_invalidate_binding() {
        let fixture = Fixture::new();
        let binding = capture_output_binding(&fixture.0, &fixture.output()).unwrap();
        std::fs::create_dir(fixture.output().parent().unwrap()).unwrap();
        std::fs::write(fixture.output(), "concurrent writer\n").unwrap();
        assert!(
            write_profile(
                &fixture.0,
                &fixture.output(),
                "ours\n",
                true,
                Some(&binding)
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(fixture.output()).unwrap(),
            b"concurrent writer\n"
        );
        std::fs::remove_file(fixture.output()).unwrap();
        let binding = capture_output_binding(&fixture.0, &fixture.output()).unwrap();
        std::fs::write(
            fixture.output().parent().unwrap().join("pubspec.yaml"),
            "name: nested\n",
        )
        .unwrap();
        assert!(
            write_profile(
                &fixture.0,
                &fixture.output(),
                "ours\n",
                false,
                Some(&binding)
            )
            .is_err()
        );
    }
    #[test]
    fn failed_final_check_restores_previous_whole_document() {
        let fixture = Fixture::new();
        write_profile(&fixture.0, &fixture.output(), "previous\n", false, None).unwrap();
        assert!(
            write_with_hook(
                &fixture.0,
                &fixture.output(),
                "replacement\n",
                true,
                None,
                &mut |stage| if stage == Stage::AfterCommit {
                    Err(config("injected final check failure"))
                } else {
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(std::fs::read(fixture.output()).unwrap(), b"previous\n");
    }
}
