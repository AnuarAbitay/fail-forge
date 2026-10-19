use std::collections::{BTreeMap, BTreeSet};

use failforge_workspace::Cancellation;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::scanner::Scanner;
use super::{MAX_EDITS, NamespaceError, invalid, require};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Edit {
    pub kind: String,
    pub offset_bytes: usize,
    pub length_bytes: usize,
    pub expected: String,
    pub replacement: String,
}

fn keys(value: &Value, expected: &[&str], message: &'static str) -> Result<(), NamespaceError> {
    let object = value.as_object().ok_or_else(|| invalid(message))?;
    require(
        object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key)),
        message,
    )
}

pub(super) fn runtime_edits(
    own: &str,
    alias: &str,
    config_hash: &str,
    sources: &BTreeMap<String, Scanner>,
    plan: Option<&Value>,
    cancel: &Cancellation,
) -> Result<BTreeMap<String, Vec<Edit>>, NamespaceError> {
    let Some(plan) = plan else {
        return Ok(sources.keys().map(|name| (name.clone(), vec![])).collect());
    };
    keys(
        plan,
        &[
            "version",
            "kind",
            "packageName",
            "alias",
            "packageConfigHash",
            "files",
            "unsupported",
        ],
        "invalid namespace plan schema",
    )?;
    require(
        plan["version"].as_u64() == Some(1)
            && plan["kind"] == "dart_namespace_plan"
            && plan["packageName"] == own
            && plan["alias"] == alias,
        "namespace plan identifies another subject",
    )?;
    require(
        plan["packageConfigHash"] == config_hash,
        "namespace plan package configuration changed",
    )?;
    require(
        plan["unsupported"].as_array().is_some_and(Vec::is_empty),
        "unsupported runtime package URI in namespace plan",
    )?;
    let rows = plan["files"]
        .as_array()
        .ok_or_else(|| invalid("namespace plan must cover every author test"))?;
    require(
        rows.len() == sources.len(),
        "namespace plan must cover every author test",
    )?;
    let own_prefix = format!("package:{own}/");
    let alias_prefix = format!("package:{alias}/");
    let mut result = BTreeMap::new();
    let mut total = 0;
    for row in rows {
        cancel.check()?;
        keys(
            row,
            &["file", "sourceHash", "edits"],
            "invalid namespace plan file",
        )?;
        let name = row["file"]
            .as_str()
            .ok_or_else(|| invalid("unsafe or duplicate namespace plan file"))?;
        let scanner = sources
            .get(name)
            .filter(|_| !result.contains_key(name))
            .ok_or_else(|| invalid("unsafe or duplicate namespace plan file"))?;
        let raw = scanner.raw();
        require(
            row["sourceHash"] == crate::sha256_hex(raw),
            "namespace plan source changed",
        )?;
        let rows = row["edits"]
            .as_array()
            .ok_or_else(|| invalid("invalid namespace edit list"))?;
        require(rows.len() <= MAX_EDITS, "invalid namespace edit list")?;
        let positions: BTreeSet<_> = scanner
            .strings()
            .iter()
            .filter(|token| {
                let bytes = scanner.value(token);
                !token.interpolated
                    && bytes.starts_with(own_prefix.as_bytes())
                    && !bytes.contains(&b'\\')
                    && !bytes.contains(&b'%')
            })
            .map(|token| token.content_start)
            .collect();
        let headers: BTreeSet<_> = scanner
            .header_uris(cancel)?
            .into_iter()
            .map(|token| token.content_start)
            .collect();
        let mut last_end = 0;
        let mut edits = vec![];
        for row in rows {
            cancel.check()?;
            let edit: Edit = serde_json::from_value(row.clone())
                .map_err(|_| invalid("invalid namespace runtime edit"))?;
            let start = edit.offset_bytes;
            let end = start
                .checked_add(edit.length_bytes)
                .ok_or_else(|| invalid("namespace runtime edit is not a unique literal prefix"))?;
            require(
                edit.kind == "runtime_package_uri"
                    && edit.length_bytes == own_prefix.len()
                    && start >= last_end
                    && positions.contains(&start)
                    && !headers.contains(&start),
                "namespace runtime edit is not a unique literal prefix",
            )?;
            require(
                edit.expected == own_prefix
                    && edit.replacement == alias_prefix
                    && raw.get(start..end) == Some(own_prefix.as_bytes()),
                "namespace runtime edit bytes differ",
            )?;
            let text =
                std::str::from_utf8(raw).map_err(|_| invalid("namespace source is not UTF-8"))?;
            require(
                text.is_char_boundary(start) && text.is_char_boundary(end),
                "namespace runtime edit splits UTF-8",
            )?;
            last_end = end;
            edits.push(edit);
        }
        total += edits.len();
        require(total <= MAX_EDITS, "namespace plan edit budget exceeded")?;
        result.insert(name.to_owned(), edits);
    }
    require(
        result.len() == sources.len(),
        "namespace plan missing an author file",
    )?;
    Ok(result)
}
