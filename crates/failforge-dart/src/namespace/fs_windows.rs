//! Handle-relative Windows reads and conditional replacement in a fresh copy.
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use failforge_workspace::{Cancellation, windows_fs as fs};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
};

use super::{NamespaceError, invalid, require};
pub(super) type Inventory = BTreeMap<String, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Version {
    identity: fs::Identity,
    size: u64,
    links: u32,
    created: i64,
    modified: i64,
    changed: i64,
    attributes: u32,
}
fn version(file: &File) -> io::Result<Version> {
    let info = fs::information(file)?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: file owns a live handle; basic is native writable output storage.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Version {
        identity: fs::identity(file)?,
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        links: info.nNumberOfLinks,
        created: basic.CreationTime,
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
        attributes: basic.FileAttributes,
    })
}
fn unchanged(file: &File, before: Version) -> Result<(), NamespaceError> {
    require(
        version(file)? == before,
        "namespace input changed while reading",
    )
}
fn hash(mut file: &File, cancel: &Cancellation) -> Result<String, NamespaceError> {
    let before = version(file)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        cancel.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    unchanged(file, before)?;
    Ok(format!("{:x}", digest.finalize()))
}
/// Flush file contents before replacement. Windows does not provide POSIX
/// directory fsync; atomic handle-relative rename supplies visibility ordering.
pub(super) fn sync(file: &File) -> io::Result<()> {
    if file.metadata()?.is_dir() {
        Ok(())
    } else {
        file.sync_all()
    }
}

pub(crate) struct Root {
    pub path: PathBuf,
    file: File,
}
impl Root {
    pub fn open(path: &Path) -> Result<Self, NamespaceError> {
        require(path.is_absolute(), "namespace roots must be absolute")?;
        let file = fs::directory(path)?;
        let path = path.canonicalize()?;
        require(
            fs::identity(&fs::directory(&path)?)? == fs::identity(&file)?,
            "namespace root changed",
        )?;
        Ok(Self { path, file })
    }
    pub fn verify(&self) -> Result<(), NamespaceError> {
        require(
            fs::identity(&fs::directory(&self.path)?)? == fs::identity(&self.file)?,
            "namespace root changed",
        )
    }
    fn parent(&self, relative: &str) -> Result<(File, OsString), NamespaceError> {
        crate::validate_relative_target(relative)
            .map_err(|_| invalid("unsafe namespace relative path"))?;
        self.verify()?;
        let mut parent = self.file.try_clone()?;
        let parts: Vec<_> = relative.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            parent = fs::directory_at(&parent, OsStr::new(part))?;
        }
        Ok((parent, OsString::from(parts[parts.len() - 1])))
    }
    pub fn read(
        &self,
        relative: &str,
        limit: usize,
        owned: bool,
    ) -> Result<(Vec<u8>, BoundFile), NamespaceError> {
        let (parent, name) = self.parent(relative)?;
        let mut file = fs::regular_at(&parent, &name, false, false)?;
        if owned {
            fs::require_private(&file, false)?;
        }
        let info = version(&file)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((limit as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        require(
            bytes.len() <= limit,
            "namespace source exceeds its byte limit",
        )?;
        unchanged(&file, info)?;
        fs::same_entry(&parent, &name, &file)?;
        Ok((
            bytes,
            BoundFile {
                relative: relative.into(),
                parent,
                name,
                file,
                info,
                generation: 0,
            },
        ))
    }
    pub fn inventory(
        &self,
        prefix: &str,
        owned: bool,
        cancel: &Cancellation,
    ) -> Result<Inventory, NamespaceError> {
        crate::validate_relative_target(prefix)
            .map_err(|_| invalid("unsafe namespace relative path"))?;
        self.verify()?;
        let directory = fs::directory_at(&self.file, OsStr::new(prefix))?;
        let mut result = Inventory::new();
        inventory(&directory, "", owned, cancel, 0, &mut result)?;
        Ok(result)
    }
}

pub(crate) struct BoundFile {
    relative: String,
    parent: File,
    name: OsString,
    file: File,
    info: Version,
    generation: usize,
}
impl BoundFile {
    pub fn generation(&self) -> usize {
        self.generation
    }
    pub fn verify(&self, root: &Root) -> Result<(), NamespaceError> {
        let (parent, _) = root.parent(&self.relative)?;
        require(
            fs::identity(&parent)? == fs::identity(&self.parent)?,
            "namespace parent directory changed",
        )?;
        fs::same_entry(&self.parent, &self.name, &self.file)?;
        unchanged(&self.file, self.info)?;
        fs::require_private(&self.file, false)?;
        Ok(())
    }
    pub fn replace(&mut self, root: &Root, bytes: &[u8]) -> Result<(), NamespaceError> {
        self.verify(root)?;
        let name = OsString::from(format!(".failforge-namespace-{}", fs::random_token()?));
        let mut file = fs::regular_at(&self.parent, &name, true, true)?;
        struct Temp<'a> {
            parent: &'a File,
            name: &'a OsStr,
            file: &'a File,
            armed: bool,
        }
        impl Drop for Temp<'_> {
            fn drop(&mut self) {
                if self.armed && fs::same_entry(self.parent, self.name, self.file).is_ok() {
                    let _ = fs::remove_handle(self.file);
                }
            }
        }
        // Retain an independent handle so the guard never trusts a recycled name.
        let temporary = file.try_clone()?;
        let mut guard = Temp {
            parent: &self.parent,
            name: &name,
            file: &temporary,
            armed: true,
        };
        file.write_all(bytes)?;
        file.set_permissions(self.file.metadata()?.permissions())?;
        sync(&file)?;
        let info = version(&file)?;
        #[cfg(test)]
        super::isolation::stage(super::isolation::Stage::TemporaryReady)?;
        self.verify(root)?;
        fs::same_entry(&self.parent, &name, &file)?;
        unchanged(&file, info)?;
        fs::require_private(&file, false)?;
        fs::rename_handle(&file, &self.parent, &self.name, true)?;
        guard.armed = false;
        self.file = file;
        self.info = info;
        self.generation += 1;
        #[cfg(test)]
        super::isolation::stage(super::isolation::Stage::Renamed)?;
        self.info = version(&self.file)?;
        self.verify(root)
    }
    pub fn restore(
        &mut self,
        root: &Root,
        expected: &[u8],
        original: &[u8],
    ) -> Result<(), NamespaceError> {
        let (actual, mut current) = root.read(&self.relative, expected.len(), true)?;
        require(
            current.info.identity == self.info.identity && actual == expected,
            "namespace rollback target changed",
        )?;
        current.generation = self.generation;
        *self = current;
        self.replace(root, original)
    }
}
fn inventory(
    root: &File,
    prefix: &str,
    owned: bool,
    cancel: &Cancellation,
    depth: usize,
    result: &mut Inventory,
) -> Result<(), NamespaceError> {
    cancel.check()?;
    require(depth <= 128, "namespace inventory depth exceeds limit")?;
    for name in fs::names(root)? {
        cancel.check()?;
        let text = name
            .to_str()
            .ok_or_else(|| invalid("namespace source path must be UTF-8"))?;
        crate::validate_relative_target(text)
            .map_err(|_| invalid("unsafe namespace entry name"))?;
        let relative = if prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{prefix}/{text}")
        };
        // A failed directory open must not turn a reparse point into a file:
        // regular_at independently enforces non-reparse regular-file type.
        if let Ok(child) = fs::directory_at(root, &name) {
            inventory(&child, &relative, owned, cancel, depth + 1, result)?;
        } else {
            let file = fs::regular_at(root, &name, false, false)?;
            if owned {
                fs::require_private(&file, false)?;
            }
            require(
                result.len() < 200_000,
                "namespace inventory exceeds entry limit",
            )?;
            result.insert(relative, hash(&file, cancel)?);
            fs::same_entry(root, &name, &file)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("ff-dart-handles-{}", fs::random_token().unwrap()));
            std::fs::create_dir(&path).unwrap();
            let parent = fs::directory(&path).unwrap();
            let directory = fs::create_directory_at(&parent, OsStr::new("lib")).unwrap();
            let mut file = fs::regular_at(&directory, OsStr::new("Ж 😀.dart"), true, true).unwrap();
            file.write_all(b"const value = true;\r\n").unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            crate::test_fs::remove_tree(&self.0);
        }
    }
    #[test]
    fn readonly_exact_byte_replace_and_restore_keep_original_and_private_identity() {
        let fixture = Fixture::new();
        let path = fixture.0.join("lib/Ж 😀.dart");
        std::fs::set_permissions(&path, crate::test_fs::permissions(0o444)).unwrap();
        let root = Root::open(&fixture.0).unwrap();
        let (original, mut target) = root.read("lib/Ж 😀.dart", 100, true).unwrap();
        let first = fs::identity(&target.file).unwrap();
        let patched = b"const value = false;\r\n";
        target.replace(&root, patched).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), patched);
        assert_ne!(fs::identity(&target.file).unwrap(), first);
        assert!(crate::test_fs::readonly(&path));
        fs::require_private(&target.file, false).unwrap();
        target.restore(&root, patched, &original).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(target.generation(), 2);
        assert!(crate::test_fs::readonly(path));
    }
    #[test]
    fn hardlinks_replaced_parents_and_same_length_source_writes_fail_closed() {
        let fixture = Fixture::new();
        let path = fixture.0.join("lib/Ж 😀.dart");
        let root = Root::open(&fixture.0).unwrap();
        std::fs::hard_link(&path, fixture.0.join("alias.dart")).unwrap();
        assert!(root.read("lib/Ж 😀.dart", 100, true).is_err());
        std::fs::remove_file(fixture.0.join("alias.dart")).unwrap();
        let (_, target) = root.read("lib/Ж 😀.dart", 100, true).unwrap();
        std::fs::write(&path, b"const value = null;\r\n").unwrap();
        assert!(target.verify(&root).is_err());
        let (_, target) = root.read("lib/Ж 😀.dart", 100, true).unwrap();
        std::fs::rename(fixture.0.join("lib"), fixture.0.join("moved")).unwrap();
        std::fs::create_dir(fixture.0.join("lib")).unwrap();
        assert!(target.verify(&root).is_err());
        assert_eq!(
            std::fs::read(fixture.0.join("moved/Ж 😀.dart")).unwrap(),
            b"const value = null;\r\n"
        );
    }
    #[test]
    fn reparse_target_cannot_redirect_an_owned_read() {
        let fixture = Fixture::new();
        let root = Root::open(&fixture.0).unwrap();
        let source = fixture.0.join("lib/Ж 😀.dart");
        let link = fixture.0.join("lib/alias.dart");
        std::os::windows::fs::symlink_file(&source, &link).unwrap();
        assert!(root.read("lib/alias.dart", 100, true).is_err());
        assert!(
            root.inventory("lib", true, &Cancellation::default())
                .is_err()
        );
        assert_eq!(std::fs::read(source).unwrap(), b"const value = true;\r\n");
    }
}
