//! Windows snapshots retain directory handles and never traverse reparse points.
use crate::inputs::{dependency_roots_from_value, file_uri};
use crate::windows_fs as win;
use crate::{
    Cancellation, InputError, PreparedProject, failure, inspect_project, parse_json, source_hashes,
    verify_source,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{File, FileTimes};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory { readonly: bool },
    File { hash: String, readonly: bool },
}
type Inventory = BTreeMap<PathBuf, Entry>;
#[derive(Clone, Copy)]
enum Stage {
    Inspected,
    Inventoried,
    Copied,
    Verified,
}

pub fn copy_project(
    source: &Path,
    destination: &Path,
    cancel: &Cancellation,
) -> Result<PreparedProject, InputError> {
    copy_with(source, destination, cancel, |_, _, _| {})
}
pub fn copy_campaign_snapshot(
    source: &Path,
    destination: &Path,
    cancel: &Cancellation,
) -> Result<PreparedProject, InputError> {
    copy_project(source, destination, cancel)
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
                name.to_str().is_some_and(|text| {
                    text.starts_with("incremental_kernel") && !text.ends_with(".dart")
                })
            })
}
fn check_depth(depth: usize) -> Result<(), InputError> {
    if depth > 128 {
        Err(failure("snapshot tree exceeds supported depth"))
    } else {
        Ok(())
    }
}
fn hash(file: &mut File, cancel: &Cancellation) -> Result<String, InputError> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        cancel.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn inventory(root: &File, cancel: &Cancellation, owned: bool) -> Result<Inventory, InputError> {
    fn visit(
        root: &File,
        prefix: &Path,
        result: &mut Inventory,
        cancel: &Cancellation,
        owned: bool,
        depth: usize,
    ) -> Result<(), InputError> {
        check_depth(depth)?;
        cancel.check()?;
        for name in win::names(root)? {
            let relative = prefix.join(&name);
            if skipped(&relative) {
                continue;
            }
            if let Ok(child) = win::directory_at(root, &name) {
                if owned {
                    win::require_private(&child, true)?;
                }
                result.insert(
                    relative.clone(),
                    Entry::Directory {
                        readonly: child.metadata()?.permissions().readonly(),
                    },
                );
                visit(&child, &relative, result, cancel, owned, depth + 1)?;
                win::same_entry(root, &name, &child)?;
            } else {
                let mut file = win::regular_at(root, &name, false, false)?;
                if owned {
                    win::require_private(&file, false)?;
                }
                let digest = hash(&mut file, cancel)?;
                win::same_entry(root, &name, &file)?;
                result.insert(
                    relative,
                    Entry::File {
                        hash: digest,
                        readonly: file.metadata()?.permissions().readonly(),
                    },
                );
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    result.insert(
        PathBuf::new(),
        Entry::Directory {
            readonly: root.metadata()?.permissions().readonly(),
        },
    );
    visit(root, Path::new(""), &mut result, cancel, owned, 0)?;
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
    let metadata = source.metadata()?;
    for name in win::names(source)? {
        let relative = prefix.join(&name);
        if skipped(&relative) {
            continue;
        }
        if let Ok(input) = win::directory_at(source, &name) {
            let output = win::create_directory_at(destination, &name)?;
            copy_tree(&input, &output, &relative, cancel, depth + 1)?;
            win::same_entry(source, &name, &input)?;
        } else {
            let mut input = win::regular_at(source, &name, false, false)?;
            let metadata = input.metadata()?;
            let mut output = win::regular_at(destination, &name, true, true)?;
            let mut buffer = [0; 65536];
            loop {
                cancel.check()?;
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
            }
            output.flush()?;
            output.set_permissions(metadata.permissions())?;
            output.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
            win::same_entry(source, &name, &input)?;
        }
    }
    destination.set_permissions(metadata.permissions())?;
    destination.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
    Ok(())
}
fn remove_contents(directory: &File, depth: usize) -> Result<(), InputError> {
    check_depth(depth)?;
    win::require_private(directory, true)?;
    for name in win::names(directory)? {
        if let Ok(child) = win::directory_at(directory, &name) {
            remove_contents(&child, depth + 1)?;
            win::same_entry(directory, &name, &child)?;
            win::remove_handle(&child)?;
        } else {
            let file = win::regular_at(directory, &name, false, false)?;
            win::require_owned(&file, false)?;
            win::remove_handle(&file)?;
        }
    }
    Ok(())
}
fn copy_with(
    source: &Path,
    destination: &Path,
    cancel: &Cancellation,
    mut hook: impl FnMut(Stage, &Path, &Path),
) -> Result<PreparedProject, InputError> {
    cancel.check()?;
    let original = win::directory(source)?;
    let project = inspect_project(source, cancel)?;
    if win::identity(&original)? != win::identity(&win::directory(&project.root)?)? {
        return Err(failure("snapshot source changed"));
    }
    hook(Stage::Inspected, &project.root, destination);
    let destination_name = destination
        .file_name()
        .ok_or_else(|| failure("snapshot destination needs a file name"))?;
    let parent_path = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = win::directory(parent_path)?;
    win::require_private(&parent, true)?;
    let parent_path = parent_path.canonicalize()?;
    let destination = parent_path.join(destination_name);
    if destination.starts_with(&project.root) || project.root.starts_with(&destination) {
        return Err(failure("snapshot source and destination overlap"));
    }
    let before = source_hashes(&project.root, cancel)?;
    let frozen = inventory(&original, cancel, false)?;
    hook(Stage::Inventoried, &project.root, &destination);
    cancel.check()?;
    let copied = win::create_directory_at(&parent, destination_name)?;
    let result = (|| {
        copy_tree(&original, &copied, Path::new(""), cancel, 0)?;
        hook(Stage::Copied, &project.root, &destination);
        if inventory(&copied, cancel, true)? != frozen {
            return Err(failure(
                "copied snapshot inputs do not match frozen project",
            ));
        }
        if source_hashes(&destination, cancel)? != before {
            return Err(failure("copied snapshot source inventory changed"));
        }
        verify_source(&project.root, &before, cancel)?;
        if inventory(&original, cancel, false)? != frozen {
            return Err(failure("prepared project inputs changed during copying"));
        }
        hook(Stage::Verified, &project.root, &destination);
        win::same_entry(&parent, destination_name, &copied)?;
        let tool = win::directory_at(&copied, OsStr::new(".dart_tool"))?;
        let read = |name: &str| -> Result<Vec<u8>, InputError> {
            let mut bytes = vec![];
            win::regular_at(&tool, OsStr::new(name), false, false)?
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 8 * 1024 * 1024 {
                return Err(failure("prepared metadata exceeds its byte limit"));
            }
            Ok(bytes)
        };
        let config_relative = Path::new(".dart_tool/package_config.json");
        let raw = read("package_config.json")?;
        let readonly = match frozen.get(config_relative) {
            Some(Entry::File { hash, readonly })
                if *hash == format!("{:x}", Sha256::digest(&raw)) =>
            {
                *readonly
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
        let graph = read("package_graph.json")?;
        if !matches!(frozen.get(Path::new(".dart_tool/package_graph.json")), Some(Entry::File { hash, .. }) if *hash == format!("{:x}", Sha256::digest(&graph)))
            || parse_json(&graph)? != project.graph
        {
            return Err(failure("copied package graph changed before rebase"));
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
        let config = win::regular_at(&tool, OsStr::new("package_config.json"), false, false)?;
        win::require_private(&config, false)?;
        win::set_readonly(&config, false)?;
        let mut output = win::regular_at(&tool, OsStr::new("package_config.json"), true, false)?;
        win::require_private(&output, false)?;
        if win::identity(&config)? != win::identity(&output)? {
            return Err(failure("copied package metadata changed before rebase"));
        }
        output.set_len(0)?;
        output.write_all(&rebased)?;
        output.flush()?;
        win::set_readonly(&output, readonly)?;
        win::same_entry(&parent, destination_name, &copied)?;
        let prepared = inspect_project(&destination, cancel)?;
        verify_source(&project.root, &before, cancel)?;
        if inventory(&original, cancel, false)? != frozen {
            return Err(failure("prepared project inputs changed during copying"));
        }
        let mut expected = frozen;
        expected.insert(
            config_relative.into(),
            Entry::File {
                hash: format!("{:x}", Sha256::digest(&rebased)),
                readonly,
            },
        );
        if inventory(&copied, cancel, true)? != expected {
            return Err(failure("copied snapshot changed during metadata rebase"));
        }
        win::same_entry(&parent, destination_name, &copied)?;
        Ok(prepared)
    })();
    if result.is_err() && win::same_entry(&parent, destination_name, &copied).is_ok() {
        let _ = remove_contents(&copied, 0).and_then(|()| {
            win::same_entry(&parent, destination_name, &copied)?;
            Ok(win::remove_handle(&copied)?)
        });
    }
    result
}

#[cfg(test)]
#[path = "copy_windows_tests.rs"]
mod tests;
