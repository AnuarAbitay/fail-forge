//! Final report publication with a single irreversible filesystem commit.
//!
//! The caller joins this process before resolving an ambiguous exit. No output,
//! cancellation check or fallible cleanup follows the commit rename.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::ExitCode;

use serde::Deserialize;
use serde_json::Value;

use crate::campaign_publish_fs as files;
use files::invalid;

const CAPS_ENV: &str = "FAILFORGE_REPORT_FDS";
const REQUEST_ENV: &str = "FAILFORGE_WORKSPACE_REQUEST_FD";
const FAMILIES: [&str; 3] = ["classic", "failure_mode", "language_specific"];

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Directory,
    Symlink,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    path: String,
    device: u64,
    inode: u64,
    kind: Kind,
    target: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    operation: String,
    output: PathBuf,
    token: String,
    profile: bool,
    complete: bool,
    run_id: String,
    bindings: Vec<Binding>,
    files: Vec<Payload>,
    progress: Option<Value>,
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode(raw: &[u8]) -> io::Result<Request> {
    let value =
        failforge_workspace::parse_json(raw).map_err(|_| invalid("invalid publication JSON"))?;
    if !value.is_object() {
        return Err(invalid("publication request must be an object"));
    }
    if value.get("progress").is_none() {
        return Err(invalid("publication requires explicit final progress"));
    }
    for key in ["bindings", "files"] {
        let rows = value[key]
            .as_array()
            .ok_or_else(|| invalid("publication entries must be an array"))?;
        if rows.iter().any(|row| !row.is_object()) {
            return Err(invalid("publication entries must be objects"));
        }
    }
    for row in value["bindings"].as_array().expect("checked bindings") {
        if (row["kind"] == "directory" && row.get("target").is_some())
            || (row["kind"] == "symlink" && !row["target"].is_string())
        {
            return Err(invalid("invalid publication binding target"));
        }
    }
    let request: Request =
        serde_json::from_value(value).map_err(|_| invalid("invalid publication request"))?;
    validate(&request)?;
    Ok(request)
}

fn expected_bindings(request: &Request) -> io::Result<BTreeMap<String, Option<String>>> {
    let mut result = BTreeMap::from([(String::new(), None)]);
    if !request.profile {
        if request.files.len() != 1 || request.files[0].path != "summary.json" {
            return Err(invalid("direct publication requires one summary"));
        }
        return Ok(result);
    }
    if request.files.len() < 2 || request.files.len() > 4 || request.files[0].path != "summary.json"
    {
        return Err(invalid(
            "profile publication requires selected family summaries",
        ));
    }
    result.extend([
        (".profile-reports".into(), None),
        (".profile-reports/pending".into(), None),
        ("families".into(), None),
        (
            ".profile-reports-current".into(),
            Some(".profile-reports/pending".into()),
        ),
        (
            "summary.json".into(),
            Some(".profile-reports-current/summary.json".into()),
        ),
    ]);
    let mut previous = None;
    for payload in &request.files[1..] {
        let index = FAMILIES
            .iter()
            .position(|family| payload.path == format!("families/{family}/summary.json"))
            .ok_or_else(|| invalid("invalid publication family filename"))?;
        if previous.is_some_and(|previous| index <= previous) {
            return Err(invalid("publication families must be unique and ordered"));
        }
        previous = Some(index);
        let family = FAMILIES[index];
        result.insert(format!("families/{family}"), None);
        result.insert(
            payload.path.clone(),
            Some(format!(
                "../../.profile-reports-current/families/{family}/summary.json"
            )),
        );
    }
    Ok(result)
}

fn validate(request: &Request) -> io::Result<()> {
    if request.operation != "campaign-publish"
        || !hex(&request.run_id, 32)
        || !hex(&request.token, 32)
        || request.files.is_empty()
        || request.files.len() > 4
        || request
            .files
            .iter()
            .any(|file| file.bytes == 0 || !hex(&file.sha256, 64))
    {
        return Err(invalid("invalid publication identity or payload"));
    }
    files::path_text(&request.output)?;
    match &request.progress {
        Some(progress) => crate::campaign_progress::validate_final(
            progress,
            &request.run_id,
            &request.output,
            request.complete,
        )?,
        None if request.complete => {
            return Err(invalid("complete publication requires final progress"));
        }
        None => {}
    }
    let expected = expected_bindings(request)?;
    let mut seen = BTreeSet::new();
    if request.bindings.len() != expected.len() {
        return Err(invalid("incomplete publication bindings"));
    }
    for binding in &request.bindings {
        let target = expected
            .get(&binding.path)
            .ok_or_else(|| invalid("unexpected publication binding"))?;
        if !seen.insert(&binding.path)
            || &binding.target != target
            || (binding.kind == Kind::Symlink) != target.is_some()
        {
            return Err(invalid("invalid publication binding"));
        }
    }
    Ok(())
}

struct Bound<'a> {
    request: &'a Request,
    directories: BTreeMap<String, File>,
}

fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

impl<'a> Bound<'a> {
    fn open(request: &'a Request, root: File) -> io::Result<Self> {
        let metadata = root.metadata()?;
        // SAFETY: inspect the current effective UID and inherited descriptor.
        let (uid, flags) = unsafe {
            (
                libc::geteuid(),
                libc::fcntl(root.as_raw_fd(), libc::F_GETFL),
            )
        };
        if !metadata.is_dir()
            || metadata.uid() != uid
            || flags < 0
            || flags & libc::O_ACCMODE != libc::O_RDONLY
        {
            return Err(invalid("invalid publication root capability"));
        }
        let mut bound = Self {
            request,
            directories: BTreeMap::from([(String::new(), root)]),
        };
        bound.verify_root()?;
        let mut bindings = request
            .bindings
            .iter()
            .filter(|binding| binding.kind == Kind::Directory && !binding.path.is_empty())
            .collect::<Vec<_>>();
        bindings.sort_by_key(|binding| (binding.path.matches('/').count(), &binding.path));
        for binding in bindings {
            let (parent, name) = split(&binding.path);
            let directory = files::directory(&bound.directories[parent], name)?;
            let metadata = directory.metadata()?;
            if metadata.dev() != binding.device || metadata.ino() != binding.inode {
                return Err(invalid("publication directory binding changed"));
            }
            bound.directories.insert(binding.path.clone(), directory);
        }
        bound.verify()?;
        Ok(bound)
    }

    fn root(&self) -> &File {
        &self.directories[""]
    }

    fn verify_root(&self) -> io::Result<()> {
        let binding = self
            .request
            .bindings
            .iter()
            .find(|binding| binding.path.is_empty())
            .ok_or_else(|| invalid("missing publication root"))?;
        let held = self.root().metadata()?;
        let named = files::walk(&self.request.output)?.metadata()?;
        if held.dev() != binding.device
            || held.ino() != binding.inode
            || held.dev() != named.dev()
            || held.ino() != named.ino()
        {
            return Err(invalid("publication root binding changed"));
        }
        Ok(())
    }

    fn verify(&self) -> io::Result<()> {
        self.verify_root()?;
        for binding in &self.request.bindings {
            if binding.path.is_empty() {
                continue;
            }
            let (parent, name) = split(&binding.path);
            let directory = &self.directories[parent];
            let named = files::stat(directory, name)?;
            let kind = if binding.kind == Kind::Directory {
                libc::S_IFDIR
            } else {
                libc::S_IFLNK
            };
            if named.st_mode & libc::S_IFMT != kind
                || named.st_dev as u64 != binding.device
                || named.st_ino != binding.inode
            {
                return Err(invalid("publication entry binding changed"));
            }
            if let Some(target) = &binding.target {
                if files::read_link(directory, name)? != *target {
                    return Err(invalid("publication alias changed"));
                }
            }
        }
        Ok(())
    }
}

fn verify_file(parent: &File, name: &str, file: &File) -> io::Result<()> {
    if !files::same(&files::stat(parent, name)?, file, libc::S_IFREG)? {
        return Err(invalid("staged publication file changed"));
    }
    Ok(())
}

fn direct_target(root: &File) -> io::Result<()> {
    match files::stat(root, "summary.json") {
        Ok(info) if matches!(info.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFLNK) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(invalid("invalid direct summary target")),
        Err(error) => Err(error),
    }
}

/// Hook is called before each publication operation. Production uses a no-op.
/// Failed private stages are deliberately preserved; no path-based rollback can
/// erase a substituted entry or a generation whose commit became ambiguous.
fn publish(
    request: &Request,
    mut capabilities: Vec<File>,
    hook: &mut impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    validate(request)?;
    if capabilities.len() != request.files.len() + 1 {
        return Err(invalid("invalid publication capability count"));
    }
    let root = capabilities.remove(0);
    let bound = Bound::open(request, root)?;
    let fingerprints = capabilities
        .iter()
        .zip(&request.files)
        .map(|(file, payload)| files::payload(file, payload.bytes, &payload.sha256))
        .collect::<io::Result<Vec<_>>>()?;
    if !request.profile {
        direct_target(bound.root())?;
    }
    if let Some(progress) = &request.progress {
        hook("progress:prepare")?;
        bound.verify()?;
        let mut writer =
            crate::campaign_progress::Writer::new(&request.output, bound.root().try_clone()?)?;
        writer.write_with_hook(progress, &mut |step| hook(&format!("progress:{step}")))?;
        bound.verify()?;
    }
    if !request.profile {
        direct_target(bound.root())?;
        let name = format!(".summary-{}.tmp", request.token);
        hook("create:summary")?;
        bound.verify()?;
        let mut output = files::create_file(bound.root(), &name)?;
        hook("write:summary")?;
        files::transfer(
            &capabilities[0],
            request.files[0].bytes,
            &request.files[0].sha256,
            &fingerprints[0],
            &mut output,
        )?;
        hook("copied:summary")?;
        let staged = files::staged(&output, request.files[0].bytes)?;
        hook("sync:summary")?;
        output.sync_all()?;
        hook("sync:root")?;
        bound.root().sync_all()?;
        hook("commit")?;
        bound.verify()?;
        verify_file(bound.root(), &name, &output)?;
        if files::staged(&output, request.files[0].bytes)? != staged {
            return Err(invalid("staged publication bytes changed"));
        }
        files::transfer(
            &output,
            request.files[0].bytes,
            &request.files[0].sha256,
            &staged,
            &mut io::sink(),
        )?;
        bound.verify()?;
        verify_file(bound.root(), &name, &output)?;
        direct_target(bound.root())?;
        // This successful rename is the final fallible operation.
        return files::rename(bound.root(), &name, "summary.json");
    }
    let storage = &bound.directories[".profile-reports"];
    let generation = format!(
        "{}-{}",
        if request.complete { "final" } else { "partial" },
        request.token
    );
    hook("mkdir:generation")?;
    bound.verify()?;
    let generation_fd = files::create_directory(storage, &generation)?;
    hook("mkdir:families")?;
    let families_fd = files::create_directory(&generation_fd, "families")?;
    let mut children = BTreeMap::new();
    for payload in &request.files[1..] {
        let family = payload
            .path
            .split('/')
            .nth(1)
            .expect("validated family filename");
        hook(&format!("mkdir:{family}"))?;
        children.insert(
            family.to_owned(),
            files::create_directory(&families_fd, family)?,
        );
    }
    let mut written = Vec::new();
    for ((payload, source), fingerprint) in
        request.files.iter().zip(&capabilities).zip(&fingerprints)
    {
        let parent = if payload.path == "summary.json" {
            &generation_fd
        } else {
            &children[payload.path.split('/').nth(1).expect("validated family")]
        };
        hook(&format!("create:{}", payload.path))?;
        let mut output = files::create_file(parent, "summary.json")?;
        hook(&format!("write:{}", payload.path))?;
        files::transfer(
            source,
            payload.bytes,
            &payload.sha256,
            fingerprint,
            &mut output,
        )?;
        hook(&format!("copied:{}", payload.path))?;
        let staged = files::staged(&output, payload.bytes)?;
        hook(&format!("sync:{}", payload.path))?;
        output.sync_all()?;
        written.push((parent, output, payload, staged));
    }
    for (name, directory) in &children {
        hook(&format!("sync:{name}"))?;
        directory.sync_all()?;
    }
    for (name, directory) in [
        ("families", &families_fd),
        ("generation", &generation_fd),
        ("storage", storage),
        ("root", bound.root()),
    ] {
        hook(&format!("sync:{name}"))?;
        directory.sync_all()?;
    }
    let pointer = format!(".profile-reports-current.{}.tmp", request.token);
    let target = format!(".profile-reports/{generation}");
    hook("create:pointer")?;
    bound.verify()?;
    files::symlink(bound.root(), &pointer, &target)?;
    let pointer_info = files::stat(bound.root(), &pointer)?;
    hook("verify:staged")?;
    bound.verify()?;
    let verify_directories = || -> io::Result<()> {
        if !files::same(
            &files::stat(storage, &generation)?,
            &generation_fd,
            libc::S_IFDIR,
        )? || !files::same(
            &files::stat(&generation_fd, "families")?,
            &families_fd,
            libc::S_IFDIR,
        )? {
            return Err(invalid("staged generation changed"));
        }
        for (name, child) in &children {
            if !files::same(&files::stat(&families_fd, name)?, child, libc::S_IFDIR)? {
                return Err(invalid("staged family directory changed"));
            }
        }
        Ok(())
    };
    verify_directories()?;
    for (parent, output, payload, staged) in &written {
        verify_file(parent, "summary.json", output)?;
        if files::staged(output, payload.bytes)? != *staged {
            return Err(invalid("staged publication bytes changed"));
        }
        files::transfer(
            output,
            payload.bytes,
            &payload.sha256,
            staged,
            &mut io::sink(),
        )?;
        verify_file(parent, "summary.json", output)?;
        hook(&format!("verified:{}", payload.path))?;
    }
    hook("commit")?;
    bound.verify()?;
    // Hashing later files must not leave earlier names, bytes or directory
    // bindings unchecked. Keep every FD alive through the final pointer commit.
    verify_directories()?;
    for (parent, output, payload, staged) in &written {
        verify_file(parent, "summary.json", output)?;
        if files::staged(output, payload.bytes)? != *staged {
            return Err(invalid("staged publication bytes changed"));
        }
    }
    let current_pointer = files::stat(bound.root(), &pointer)?;
    if current_pointer.st_mode & libc::S_IFMT != libc::S_IFLNK
        || current_pointer.st_dev != pointer_info.st_dev
        || current_pointer.st_ino != pointer_info.st_ino
        || files::read_link(bound.root(), &pointer)? != target
    {
        return Err(invalid("staged publication pointer changed"));
    }
    // No verification, callback, stdout or cleanup is permitted after this.
    files::rename(bound.root(), &pointer, ".profile-reports-current")
}

fn capabilities(count: usize) -> io::Result<Vec<File>> {
    let text = std::env::var(CAPS_ENV).map_err(|_| invalid("missing publication capabilities"))?;
    let mut descriptors = Vec::new();
    for text in text.split(',') {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid("invalid publication descriptor"));
        }
        let descriptor: RawFd = text
            .parse()
            .map_err(|_| invalid("invalid publication descriptor"))?;
        if descriptor < 3 || descriptors.contains(&descriptor) {
            return Err(invalid("invalid publication descriptor"));
        }
        descriptors.push(descriptor);
    }
    if descriptors.len() != count {
        return Err(invalid("invalid publication capability count"));
    }
    for key in [REQUEST_ENV, crate::readiness::READY_ENV] {
        if std::env::var(key)
            .ok()
            .and_then(|value| value.parse::<RawFd>().ok())
            .is_some_and(|descriptor| descriptors.contains(&descriptor))
        {
            return Err(invalid("conflicting publication descriptor"));
        }
    }
    let mut result = Vec::with_capacity(count);
    for descriptor in descriptors {
        // SAFETY: descriptor validity is checked before adopting it uniquely.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the validated inherited descriptor is adopted once.
        let file = unsafe { File::from_raw_fd(descriptor) };
        // SAFETY: setting CLOEXEC affects only the live owned descriptor.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        result.push(file);
    }
    Ok(result)
}

pub(crate) fn execute() -> ExitCode {
    let result = (|| -> io::Result<()> {
        for key in [
            crate::workspace_handoff::CAPS_ENV,
            crate::workspace_handoff::HANDOFF_ENV,
            crate::test_context::CONTEXT_ENV,
            crate::test_evidence::EVIDENCE_ENV,
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_ADAPTER_CANCEL_FD",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(invalid("unexpected publication capability"));
            }
        }
        let descriptor =
            std::env::var_os(REQUEST_ENV).ok_or_else(|| invalid("missing publication request"))?;
        let request = decode(&crate::private_request::read(&descriptor)?)?;
        let capabilities = capabilities(request.files.len() + 1)?;
        if let Some(ready) = crate::readiness::Readiness::from_env()? {
            ready.acknowledge()?;
        }
        publish(&request, capabilities, &mut |_| Ok(()))
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "failforge: report publication failed: {error}"
            );
            ExitCode::from(125)
        }
    }
}

#[cfg(test)]
#[path = "campaign_publish_tests.rs"]
mod tests;
