//! Validate a file-scoped profile and render every operator switch explicitly.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use caseless::Caseless;
use failforge_config::{Context, MAX_RULE_VALUES, MAX_YAML_BYTES, catalogue, paths};
use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;

use crate::analysis::ApplicabilityResult;
use crate::output::{capture_output_binding, check_destination};
use crate::{GenerateOptions, ProfileDraft, ProfileError};

const RESERVED_FILES: &[&str] = &[
    "pubspec.yaml",
    "pubspec_overrides.yaml",
    "analysis_options.yaml",
    "build.yaml",
    "dart_test.yaml",
    "l10n.yaml",
];
const TOOL_DIRECTORIES: &[&str] = &[
    ".git",
    ".dart_tool",
    ".failforge",
    "build",
    "target",
    "__pycache__",
];

fn config(message: impl Into<String>) -> ProfileError {
    ProfileError::Config(message.into())
}

fn folded(value: &str) -> String {
    value.chars().default_case_fold().collect()
}

fn normalized_folded(value: &str) -> String {
    value.nfc().default_case_fold().collect()
}

fn same_file(left: &Path, right: &Path) -> io::Result<bool> {
    match (fs::metadata(left), fs::metadata(right)) {
        (Ok(a), Ok(b)) => {
            #[cfg(unix)]
            {
                Ok(a.dev() == b.dev() && a.ino() == b.ino())
            }
            #[cfg(windows)]
            {
                let _ = (a, b);
                use failforge_workspace::windows_fs as win;
                let open = |path: &Path| {
                    if path.is_dir() {
                        win::directory(path)
                    } else {
                        win::regular(path, false)
                    }
                };
                Ok(win::identity(&open(left)?)? == win::identity(&open(right)?)?)
            }
        }
        (Err(e), _) | (_, Err(e)) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}

fn inside(path: &Path, root: &Path) -> io::Result<bool> {
    for parent in path.ancestors() {
        if same_file(parent, root)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Retain the unresolved path as well, because resolving `link/../file` would
/// otherwise conceal an alias inside the package from the write policy.
fn requested_path(
    value: &Path,
    name: &str,
    cwd: &Path,
) -> Result<(PathBuf, PathBuf), ProfileError> {
    let canonical = paths::resolve_native_path(value, name, cwd)?;
    let text = value
        .to_str()
        .ok_or_else(|| config(format!("{name} must contain valid Unicode")))?;
    let expanded = if text.starts_with('~') {
        let (home, suffix) = text.split_once('/').unwrap_or((text, ""));
        paths::resolve_native_path(Path::new(home), name, cwd)?.join(suffix.trim_start_matches('/'))
    } else {
        value.to_path_buf()
    };
    let requested = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    Ok((requested, canonical))
}

fn reject_aliases(requested: &Path, project: &Path) -> Result<(), ProfileError> {
    for candidate in requested.ancestors() {
        let metadata = match fs::symlink_metadata(candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || {
            #[cfg(windows)]
            {
                failforge_workspace::windows_fs::is_reparse(&metadata)
            }
            #[cfg(unix)]
            {
                false
            }
        } {
            let resolved = paths::resolve_native_path(candidate, "profile path", project)?;
            let parent = candidate.parent().unwrap_or(Path::new("/"));
            let parent = paths::resolve_native_path(parent, "profile path", project)?;
            if inside(&parent, project)?
                || (!same_file(&resolved, project)? && inside(&resolved, project)?)
            {
                return Err(config(
                    "source and profile paths inside the package must not contain symlinks",
                ));
            }
        }
    }
    Ok(())
}

/// Directory entries provide the spelling used by the adapter's discovery.
/// Identity comparison also works on case-insensitive / normalization-insensitive
/// filesystems, without inspecting ancestors outside the package.
fn canonical_spelling(path: &Path, project: &Path) -> Result<PathBuf, ProfileError> {
    let relative = path
        .strip_prefix(project)
        .map_err(|_| config("profile path must be inside its package"))?;
    let mut current = project.to_path_buf();
    let parts: Vec<_> = relative.components().collect();
    for (index, part) in parts.iter().enumerate() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                for rest in &parts[index..] {
                    current.push(rest.as_os_str());
                }
                return Ok(current);
            }
            Err(error) => return Err(error.into()),
        };
        if entries
            .iter()
            .any(|entry| entry.file_name() == part.as_os_str())
        {
            current.push(part.as_os_str());
            continue;
        }
        let requested = current.join(part.as_os_str());
        match fs::metadata(&requested) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                for rest in &parts[index..] {
                    current.push(rest.as_os_str());
                }
                return Ok(current);
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let normalized = normalized_folded(&part.as_os_str().to_string_lossy());
        let mut matches = entries
            .into_iter()
            .filter(|entry| normalized_folded(&entry.file_name().to_string_lossy()) == normalized)
            .filter_map(|entry| match same_file(&entry.path(), &requested) {
                Ok(true) => Some(Ok(entry.file_name())),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            });
        let matched = matches.next().transpose()?.ok_or_else(|| {
            config("cannot identify the exact filesystem spelling of a profile path")
        })?;
        if matches.next().transpose()?.is_some() {
            return Err(config(
                "cannot identify the exact filesystem spelling of a profile path",
            ));
        }
        current.push(matched);
    }
    Ok(current)
}

fn package_paths(
    options: &GenerateOptions,
    context: &Context,
) -> Result<(PathBuf, PathBuf, PathBuf), ProfileError> {
    let (requested_source, source) = requested_path(&options.source, "file", &context.cwd)?;
    if source.extension().and_then(|v| v.to_str()) != Some("dart") {
        return Err(config("--file must name an existing .dart source file"));
    }
    paths::read_regular_bytes(&source, 0, false)
        .map_err(|_| config("--file must name a readable regular .dart source file"))?;
    let project = paths::profile_project(
        source
            .parent()
            .ok_or_else(|| config("--file has no parent"))?,
    )?;
    reject_aliases(&requested_source, &project)?;
    let source = canonical_spelling(&source, &project)?;
    let relative = source
        .strip_prefix(&project)
        .map_err(|_| config("source must be inside its package"))?;
    let relative_text = relative
        .to_str()
        .ok_or_else(|| config("file must contain valid Unicode"))?;
    #[cfg(windows)]
    let relative_text = relative_text.replace('\\', "/");
    if relative_text.contains(['*', '?', '[', ']', '{', '}', '\\']) {
        return Err(config(
            "--file must have a literal path without glob characters or backslashes",
        ));
    }
    if relative.parent().is_some_and(|parent| {
        parent.components().any(|part| {
            TOOL_DIRECTORIES.contains(&folded(&part.as_os_str().to_string_lossy()).as_str())
        })
    }) {
        return Err(config(
            "--file must be outside generated and tool directories",
        ));
    }
    if relative
        .components()
        .next()
        .is_none_or(|part| part.as_os_str() != "lib")
    {
        return Err(config(
            "--file must be inside lib/: the mutation engine discovers library sources only",
        ));
    }
    let default_output = project
        .join("profiles")
        .join(relative)
        .with_extension("yml");
    let output_path = options.output.as_deref().unwrap_or(&default_output);
    let (requested_output, output) = requested_path(output_path, "output", &context.cwd)?;
    reject_aliases(&requested_output, &project)?;
    let output_project = paths::profile_project(
        output
            .parent()
            .ok_or_else(|| config("output has no parent"))?,
    )
    .map_err(|_| config("profile output must be inside the same package as --file"))?;
    if !same_file(&output_project, &project)? {
        return Err(config(
            "profile output must be inside the same package as --file",
        ));
    }
    let relative = output
        .strip_prefix(&output_project)
        .map_err(|_| config("profile output must be inside the same package as --file"))?;
    let output = canonical_spelling(&project.join(relative), &project)?;
    if !matches!(
        output.extension().and_then(|v| v.to_str()),
        Some("yml" | "yaml")
    ) {
        return Err(config("profile output must end in .yml or .yaml"));
    }
    let relative = output
        .strip_prefix(&project)
        .map_err(|_| config("profile output must be inside its package"))?;
    if RESERVED_FILES
        .contains(&folded(&output.file_name().unwrap_or_default().to_string_lossy()).as_str())
        || relative.parent().is_some_and(|parent| {
            parent.components().any(|part| {
                TOOL_DIRECTORIES.contains(&folded(&part.as_os_str().to_string_lossy()).as_str())
            })
        })
    {
        return Err(config(
            "profile output must not replace package configuration or tool files",
        ));
    }
    Ok((project, source, output))
}

fn unique_strings<'a>(values: &'a [String], name: &str) -> Result<HashSet<&'a str>, ProfileError> {
    if values.len() > MAX_RULE_VALUES {
        return Err(config(format!("{name} must be a bounded list of strings")));
    }
    let mut result = HashSet::new();
    for value in values {
        if value.is_empty() || value.contains('\0') {
            return Err(config(format!(
                "{name} must contain nonempty strings without NUL"
            )));
        }
        if !result.insert(value.as_str()) {
            return Err(config(format!("{name} contains a duplicate value")));
        }
    }
    Ok(result)
}

fn mutations(
    options: &GenerateOptions,
    project: &Path,
) -> Result<(Value, Option<bool>), ProfileError> {
    let selected = unique_strings(&options.families, "families")?;
    if selected.is_empty()
        || selected
            .iter()
            .any(|family| !catalogue::FAMILIES.contains(family))
    {
        return Err(config(
            "--family must select classic, failure_mode or language_specific",
        ));
    }
    if selected.contains("language_specific") {
        if options.language.as_deref() != Some("dart") {
            return Err(config(
                "language_specific requires explicit --language dart",
            ));
        }
    } else if options.language.is_some() {
        return Err(config("--language requires family language_specific"));
    }
    let explicit = unique_strings(&options.operators, "operators")?;
    if explicit.iter().any(|operator| {
        !selected.iter().any(|family| {
            catalogue::operators(family)
                .unwrap_or_default()
                .contains(operator)
        })
    }) {
        return Err(config(
            "--operator must belong to one of the selected mutation families",
        ));
    }
    let framework_flutter = if selected.contains("failure_mode") {
        Some(crate::pubspec::flutter_package(project)?)
    } else {
        None
    };
    let flutter = framework_flutter.unwrap_or(false);
    if explicit.contains("METHOD_CHANNEL_ERROR") && !flutter {
        return Err(config("METHOD_CHANNEL_ERROR requires a Flutter package"));
    }
    let mut result = Map::new();
    for family in catalogue::FAMILIES {
        if !selected.contains(family) {
            continue;
        }
        let defaults = catalogue::default_operators(family, flutter).unwrap_or_default();
        let operators = catalogue::operators(family)
            .unwrap_or_default()
            .iter()
            .map(|operator| {
                (
                    (*operator).into(),
                    Value::Bool(if explicit.is_empty() {
                        defaults.contains(operator)
                    } else {
                        explicit.contains(operator)
                    }),
                )
            })
            .collect::<Map<_, _>>();
        let mut settings = json!({"enabled": true, "operators": operators});
        if *family == "language_specific" {
            settings["language"] = json!("dart");
        }
        result.insert((*family).into(), settings);
    }
    Ok((Value::Object(result), framework_flutter))
}

fn quoted(value: &str) -> String {
    // JSON double-quoted strings are valid YAML; escaping line/paragraph
    // separators additionally avoids YAML 1.1's physical line folding.
    serde_json::to_string(value)
        .expect("a string always serializes")
        .replace('\u{85}', "\\u0085")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn profile_text(
    values: &Value,
    project: &Path,
    output: &Path,
    context: &Context,
) -> Result<String, ProfileError> {
    let source = values["rules"]["include"][0]
        .as_str()
        .ok_or_else(|| config("profile has no file scope"))?;
    let mut text = format!(
        "workers: 1\ntimeouts:\n  test: 30s\n  command: 120s\nrules:\n  include:\n    - {}\nmutations:\n",
        quoted(source)
    );
    for family in catalogue::FAMILIES {
        let Some(selection) = values["mutations"].get(*family) else {
            continue;
        };
        text.push_str(&format!("  {family}:\n    enabled: true\n"));
        if *family == "language_specific" {
            text.push_str("    language: dart\n");
        }
        text.push_str("    operators:\n");
        for operator in catalogue::operators(family).unwrap_or_default() {
            let enabled = selection["operators"][*operator]
                .as_bool()
                .ok_or_else(|| config("profile operator switches must be booleans"))?;
            text.push_str(&format!("      {operator}: {enabled}\n"));
        }
    }
    text.push_str("limits:\n  max_mutants: 1000\n");
    for name in ["dart_sdk", "flutter_sdk", "adapter_root"] {
        if let Some(value) = values.get(name) {
            let value = value
                .as_str()
                .ok_or_else(|| config(format!("{name} must be a path")))?;
            text.push_str(&format!("{name}: {}\n", quoted(value)));
        }
    }
    if text.len() > MAX_YAML_BYTES {
        return Err(config("generated profile exceeds 1 MiB"));
    }
    let roundtrip = failforge_config::yaml::parse_yaml(&text)?;
    if &roundtrip != values {
        return Err(ProfileError::Analysis(
            "generated profile failed YAML round-trip".into(),
        ));
    }
    failforge_config::from_profile_mapping(&roundtrip, project, context, output.parent())?;
    Ok(text)
}

pub fn prepare_profile(
    options: &GenerateOptions,
    context: &Context,
) -> Result<ProfileDraft, ProfileError> {
    let (project, source, output) = package_paths(options, context)?;
    check_destination(&output, options.force)?;
    let binding = capture_output_binding(&project, &output)?;
    let (mutations, framework_flutter) = mutations(options, &project)?;
    let relative = source
        .strip_prefix(&project)
        .map_err(|_| config("source must be inside its package"))?;
    let source_text = relative
        .to_str()
        .ok_or_else(|| config("file must contain valid Unicode"))?;
    #[cfg(windows)]
    let source_text = source_text.replace('\\', "/");
    let mut values = json!({"workers": 1, "timeouts": {"test": "30s", "command": "120s"},
        "rules": {"include": [source_text]}, "mutations": mutations, "limits": {"max_mutants": 1000}});
    for (name, path) in [
        ("dart_sdk", &options.dart_sdk),
        ("flutter_sdk", &options.flutter_sdk),
        ("adapter_root", &options.adapter_root),
    ] {
        if let Some(path) = path {
            let normalized = paths::resolve_native_path(path, name, &context.cwd)?;
            let text = normalized
                .to_str()
                .ok_or_else(|| config(format!("{name} must contain valid Unicode")))?;
            values[name] = json!(text);
        }
    }
    let text = profile_text(&values, &project, &output, context)?;
    Ok(ProfileDraft {
        project,
        source,
        output,
        text,
        values,
        binding,
        framework_flutter,
    })
}

pub fn apply_applicability(
    draft: &mut ProfileDraft,
    result: &ApplicabilityResult,
    context: &Context,
) -> Result<(), ProfileError> {
    // Build the replacement first, preserving the original draft on a malformed
    // adapter result or a result with no active operators.
    let mut values = draft.values.clone();
    let settings = values["mutations"]
        .as_object_mut()
        .ok_or_else(|| config("profile has no mutation selections"))?;
    let mut found = HashMap::new();
    for row in &result.families {
        if found.insert(row.family.as_str(), row).is_some() {
            return Err(ProfileError::Analysis(
                "applicability result does not match the requested mutation families".into(),
            ));
        }
    }
    if found.len() != settings.len()
        || settings
            .keys()
            .any(|family| !found.contains_key(family.as_str()))
    {
        return Err(ProfileError::Analysis(
            "applicability result does not match the requested mutation families".into(),
        ));
    }
    let mut any = false;
    for (family, selection) in settings.iter_mut() {
        let operators = selection["operators"]
            .as_object_mut()
            .ok_or_else(|| config("profile has no operator switches"))?;
        let active: HashSet<_> = found[family.as_str()]
            .active_operators
            .iter()
            .map(String::as_str)
            .collect();
        if active
            .iter()
            .any(|operator| operators.get(*operator).and_then(Value::as_bool) != Some(true))
        {
            return Err(ProfileError::Analysis(
                "applicability result contains an unrequested operator".into(),
            ));
        }
        for (operator, enabled) in operators {
            let selected = enabled.as_bool() == Some(true) && active.contains(operator.as_str());
            any |= selected;
            *enabled = Value::Bool(selected);
        }
    }
    if !any {
        return Err(config(
            "no applicable mutation operators found in the selected file; no profile was written",
        ));
    }
    let text = profile_text(&values, &draft.project, &draft.output, context)?;
    draft.text = text;
    draft.values = values;
    Ok(())
}

#[cfg(test)]
mod tests;
