//! Read framework identity without executing package code or expanding aliases.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::LazyLock;

use failforge_config::{MAX_YAML_BYTES, paths};
use libyaml_safer::{Encoding, EventData, Parser, ScalarStyle};
use regex::Regex;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};

use crate::ProfileError;

fn config(message: &str) -> ProfileError {
    ProfileError::Config(message.into())
}
fn invalid_yaml() -> ProfileError {
    config("project pubspec.yaml must contain valid UTF-8 YAML")
}

static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"\A([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})(?:(?:[Tt]|[ \t]+)([0-9]{1,2}):([0-9]{2}):([0-9]{2})(?:\.([0-9]*))?(?:[ \t]*(Z|([-+])([0-9]{1,2})(?::([0-9]{2}))?))?)?\z"
).expect("fixed author YAML timestamp grammar")
});
static IMPLICIT_TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"\A(?:[0-9]{4}-[0-9]{2}-[0-9]{2}|[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?)\z"
).expect("fixed author YAML implicit timestamp resolver")
});

fn valid_integer(value: &str) -> bool {
    fn digits(value: &str, radix: u32) -> bool {
        let value = value.trim().trim_start_matches(['+', '-']);
        !value.is_empty() && value.chars().all(|character| character.is_digit(radix))
    }
    let value = value.replace('_', "");
    let value = value.strip_prefix(['+', '-']).unwrap_or(&value);
    if let Some(value) = value.strip_prefix("0b") {
        digits(value, 2)
    } else if let Some(value) = value.strip_prefix("0x") {
        digits(value, 16)
    } else if value.starts_with('0') {
        digits(value, 8)
    } else if value.contains(':') {
        value.split(':').all(|part| digits(part, 10))
    } else {
        digits(value, 10)
    }
}

fn valid_float(value: &str) -> bool {
    let value = value.replace('_', "").to_lowercase();
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(&value);
    matches!(unsigned, ".inf" | ".nan")
        || if unsigned.contains(':') {
            unsigned
                .split(':')
                .all(|part| part.trim().parse::<f64>().is_ok())
        } else {
            value.trim().parse::<f64>().is_ok()
        }
}

fn valid_timestamp(value: &str) -> bool {
    let Some(captures) = TIMESTAMP.captures(value) else {
        return false;
    };
    let number = |index| {
        captures
            .get(index)
            .and_then(|value| value.as_str().parse::<u32>().ok())
    };
    let (Some(year), Some(month), Some(day)) = (number(1), number(2), number(3)) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    year > 0
        && day > 0
        && day <= max_day
        && number(4).is_none_or(|hour| hour < 24)
        && number(5).is_none_or(|minute| minute < 60)
        && number(6).is_none_or(|second| second < 60)
        && number(10).unwrap_or(0) * 60 + number(11).unwrap_or(0) < 24 * 60
}

fn valid_binary(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let mut position = 0;
    let mut pads = 0;
    for byte in value.bytes() {
        if byte == b'=' {
            if position >= 2 {
                pads += 1;
                if position + pads >= 4 {
                    return true;
                }
            }
        } else if byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/') {
            pads = 0;
            position = (position + 1) % 4;
        }
    }
    position == 0
}

#[derive(Debug)]
enum Node {
    String(String),
    Null,
    Other,
    Merge,
    ValueTag,
    Sequence(Vec<usize>),
    Pairs(Vec<usize>),
    Mapping(Vec<(usize, usize)>),
    Set(Vec<(usize, usize)>),
}

enum Frame {
    Sequence(usize),
    Mapping(usize, Option<usize>),
}

fn append(
    arena: &mut [Node],
    stack: &mut [Frame],
    root: &mut Option<usize>,
    id: usize,
) -> Result<(), ProfileError> {
    if matches!(arena[id], Node::Merge | Node::ValueTag)
        && !matches!(stack.last(), Some(Frame::Mapping(_, None)))
    {
        return Err(invalid_yaml());
    }
    match stack.last_mut() {
        Some(Frame::Sequence(parent)) => match &mut arena[*parent] {
            Node::Sequence(values) | Node::Pairs(values) => values.push(id),
            _ => unreachable!("sequence frame owns a sequence node"),
        },
        Some(Frame::Mapping(parent, key)) => {
            if let Some(key) = key.take() {
                match &mut arena[*parent] {
                    Node::Mapping(pairs) | Node::Set(pairs) => pairs.push((key, id)),
                    _ => unreachable!("mapping frame owns a mapping node"),
                }
            } else {
                *key = Some(id);
            }
        }
        None if root.replace(id).is_some() => return Err(invalid_yaml()),
        None => {}
    }
    Ok(())
}

fn scalar(value: String, style: ScalarStyle, tag: Option<String>) -> Result<Node, ProfileError> {
    let explicit = tag.as_deref();
    match explicit {
        Some("!" | "tag:yaml.org,2002:str") => return Ok(Node::String(value)),
        Some("tag:yaml.org,2002:null") => return Ok(Node::Null),
        Some("tag:yaml.org,2002:merge") => return Ok(Node::Merge),
        Some("tag:yaml.org,2002:value") => return Ok(Node::ValueTag),
        Some("tag:yaml.org,2002:bool") => {
            if !matches!(
                value.as_str(),
                "true"
                    | "True"
                    | "TRUE"
                    | "false"
                    | "False"
                    | "FALSE"
                    | "yes"
                    | "Yes"
                    | "YES"
                    | "no"
                    | "No"
                    | "NO"
                    | "on"
                    | "On"
                    | "ON"
                    | "off"
                    | "Off"
                    | "OFF"
            ) {
                return Err(invalid_yaml());
            }
            return Ok(Node::Other);
        }
        Some("tag:yaml.org,2002:int") if valid_integer(&value) => return Ok(Node::Other),
        Some("tag:yaml.org,2002:float") if valid_float(&value) => return Ok(Node::Other),
        Some("tag:yaml.org,2002:timestamp") if valid_timestamp(&value) => return Ok(Node::Other),
        Some("tag:yaml.org,2002:binary") if valid_binary(&value) => return Ok(Node::Other),
        Some(_) => return Err(invalid_yaml()),
        None => {}
    }
    if style != ScalarStyle::Plain {
        return Ok(Node::String(value));
    }
    if value == "<<" {
        return Ok(Node::Merge);
    }
    if value == "=" {
        return Ok(Node::ValueTag);
    }
    if matches!(value.as_str(), "" | "~" | "null" | "Null" | "NULL") {
        return Ok(Node::Null);
    }
    if IMPLICIT_TIMESTAMP.is_match(&value) {
        return if valid_timestamp(&value) {
            Ok(Node::Other)
        } else {
            Err(invalid_yaml())
        };
    }
    // Only dependency-name / sdk string equality and null sections are queried.
    // Ordinary YAML booleans and numeric values cannot equal those identifiers.
    Ok(Node::String(value))
}

fn author_yaml(text: &str) -> Result<(Vec<Node>, usize), ProfileError> {
    if text.len() > MAX_YAML_BYTES {
        return Err(config("project pubspec.yaml exceeds 1 MiB"));
    }
    if text.chars().any(|character| !matches!(character, '\t' | '\n' | '\r' | '\u{20}'..='\u{7e}' | '\u{85}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')) { return Err(invalid_yaml()); }
    let mut parser = Parser::new();
    let prepared =
        failforge_config::yaml::prepare_author_preamble(text).map_err(|_| invalid_yaml())?;
    parser.set_input(BufReader::new(prepared.as_bytes()));
    parser.set_encoding(Encoding::Utf8);
    let mut arena = Vec::new();
    let mut anchors = HashMap::new();
    let mut stack = Vec::new();
    let mut root = None;
    let mut documents = 0;
    loop {
        let event = parser.parse().map_err(|_| invalid_yaml())?;
        let (node, anchor, container) = match event.data {
            EventData::StreamStart { .. } | EventData::DocumentEnd { .. } => continue,
            EventData::DocumentStart { .. } => {
                documents += 1;
                if documents > 1 {
                    return Err(invalid_yaml());
                }
                continue;
            }
            EventData::StreamEnd => break,
            EventData::Alias { anchor } => {
                let id = *anchors.get(&anchor).ok_or_else(invalid_yaml)?;
                append(&mut arena, &mut stack, &mut root, id)?;
                continue;
            }
            EventData::Scalar {
                value,
                style,
                tag,
                anchor,
                ..
            } => (scalar(value, style, tag)?, anchor, false),
            EventData::SequenceStart { anchor, tag, .. } => {
                if tag.as_deref().is_some_and(|tag| {
                    !matches!(
                        tag,
                        "!" | "tag:yaml.org,2002:seq"
                            | "tag:yaml.org,2002:omap"
                            | "tag:yaml.org,2002:pairs"
                    )
                }) {
                    return Err(invalid_yaml());
                }
                let node = if matches!(
                    tag.as_deref(),
                    Some("tag:yaml.org,2002:omap" | "tag:yaml.org,2002:pairs")
                ) {
                    Node::Pairs(Vec::new())
                } else {
                    Node::Sequence(Vec::new())
                };
                (node, anchor, true)
            }
            EventData::MappingStart { anchor, tag, .. } => {
                if tag.as_deref().is_some_and(|tag| {
                    !matches!(tag, "!" | "tag:yaml.org,2002:map" | "tag:yaml.org,2002:set")
                }) {
                    return Err(invalid_yaml());
                }
                let node = if tag.as_deref() == Some("tag:yaml.org,2002:set") {
                    Node::Set(Vec::new())
                } else {
                    Node::Mapping(Vec::new())
                };
                (node, anchor, true)
            }
            EventData::SequenceEnd => {
                if !matches!(stack.pop(), Some(Frame::Sequence(_))) {
                    return Err(invalid_yaml());
                }
                continue;
            }
            EventData::MappingEnd => {
                if !matches!(stack.pop(), Some(Frame::Mapping(_, None))) {
                    return Err(invalid_yaml());
                }
                continue;
            }
        };
        let id = arena.len();
        arena.push(node);
        if let Some(anchor) = anchor {
            if anchors.insert(anchor, id).is_some() {
                return Err(invalid_yaml());
            }
        }
        append(&mut arena, &mut stack, &mut root, id)?;
        if container {
            if stack.len() >= 128 {
                return Err(invalid_yaml());
            }
            stack.push(if matches!(arena[id], Node::Mapping(_) | Node::Set(_)) {
                Frame::Mapping(id, None)
            } else {
                Frame::Sequence(id)
            });
        }
    }
    if !stack.is_empty() {
        return Err(invalid_yaml());
    }
    let root = root.ok_or_else(|| config("project pubspec.yaml must contain a mapping"))?;
    Ok((arena, root))
}

/// Retain only the four keys needed for framework discovery. Aliases share IDs
/// and each cached projection has at most four entries; nested merges therefore
/// cannot amplify a bounded pubspec into an unbounded expanded mapping.
fn mapping(
    arena: &[Node],
    id: usize,
    cache: &mut HashMap<usize, HashMap<String, usize>>,
    active: &mut HashSet<usize>,
) -> Result<HashMap<String, usize>, ProfileError> {
    if let Some(cached) = cache.get(&id) {
        return Ok(cached.clone());
    }
    let pairs = match &arena[id] {
        Node::Mapping(pairs) | Node::Set(pairs) => pairs,
        _ => return Err(invalid_yaml()),
    };
    if !active.insert(id) {
        return Ok(HashMap::new());
    }
    let mut merged = HashMap::new();
    let mut explicit = Vec::new();
    for (key, value) in pairs {
        match &arena[*key] {
            Node::Merge => match &arena[*value] {
                Node::Mapping(_) => merged.extend(mapping(arena, *value, cache, active)?),
                Node::Sequence(items) => {
                    for item in items.iter().rev() {
                        merged.extend(mapping(arena, *item, cache, active)?);
                    }
                }
                _ => return Err(invalid_yaml()),
            },
            Node::String(key)
                if ["dependencies", "dev_dependencies", "flutter_test", "sdk"]
                    .contains(&key.as_str()) =>
            {
                explicit.push((key.clone(), *value))
            }
            Node::String(_) => {}
            Node::Null | Node::Other | Node::ValueTag => {}
            Node::Mapping(_) | Node::Sequence(_) | Node::Pairs(_) | Node::Set(_) => {
                return Err(invalid_yaml());
            }
        }
    }
    merged.extend(explicit);
    active.remove(&id);
    cache.insert(id, merged.clone());
    Ok(merged)
}

fn pubspec_flutter(text: &str) -> Result<bool, ProfileError> {
    let (arena, root) = author_yaml(text)?;
    if !matches!(arena[root], Node::Mapping(_)) {
        return Err(config("project pubspec.yaml must contain a mapping"));
    }
    let mut cache = HashMap::new();
    let mut active = HashSet::new();
    // Validate mapping keys and merges even in unrelated author metadata.
    for (id, node) in arena.iter().enumerate() {
        match node {
            Node::Mapping(_) | Node::Set(_) => {
                mapping(&arena, id, &mut cache, &mut active)?;
            }
            Node::Pairs(items) => {
                for item in items {
                    if !matches!(&arena[*item], Node::Mapping(pairs) if pairs.len() == 1) {
                        return Err(invalid_yaml());
                    }
                }
            }
            _ => {}
        }
    }
    let root = &cache[&root];
    let mut flutter = false;
    for section in ["dependencies", "dev_dependencies"] {
        let Some(id) = root.get(section) else {
            continue;
        };
        match &arena[*id] {
            Node::Null => continue,
            Node::Mapping(_) => {}
            _ => return Err(config(&format!("pubspec {section} must contain a mapping"))),
        }
        let Some(dependency) = cache[id].get("flutter_test") else {
            continue;
        };
        if matches!(arena[*dependency], Node::Mapping(_)) {
            let dependency = &cache[dependency];
            if dependency.get("sdk").is_some_and(
                |sdk| matches!(&arena[*sdk], Node::String(value) if value == "flutter"),
            ) {
                flutter = true;
            }
        }
    }
    Ok(flutter)
}

/// serde_json's default object decoding accepts duplicate keys; metadata must
/// reject them recursively before its framework identity can be trusted.
struct UniqueJson(Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueJson;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Bool(value)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(value.into())))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(value.into())))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Number::from_f64(value)
                    .map(|number| UniqueJson(Value::Number(number)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(value.into())))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(value)))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueJson(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some((key, UniqueJson(value))) = map.next_entry::<String, UniqueJson>()? {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate package_config.json key"));
                    }
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

fn prepared_flutter_package(project: &Path) -> Result<Option<bool>, ProfileError> {
    let metadata = project.join(".dart_tool");
    match fs::symlink_metadata(&metadata) {
        Ok(value) if value.file_type().is_symlink() => {
            return Err(config("project .dart_tool must not be a symlink"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let limit = 8 * 1024 * 1024;
    let bytes = match paths::read_regular_bytes(&metadata.join("package_config.json"), limit, false)
    {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(config(
                "project package_config.json must be a readable regular file",
            ));
        }
    };
    if bytes.len() > limit {
        return Err(config("project package_config.json exceeds 8 MiB"));
    }
    let UniqueJson(values) = serde_json::from_slice(&bytes).map_err(|error| {
        if error
            .to_string()
            .contains("duplicate package_config.json key")
        {
            config("duplicate package_config.json key")
        } else {
            config("project package_config.json must contain valid UTF-8 JSON")
        }
    })?;
    let Some(packages) = values
        .get("packages")
        .and_then(Value::as_array)
        .filter(|_| values.get("configVersion").and_then(Value::as_u64) == Some(2))
    else {
        return Err(config(
            "project package_config.json requires version 2 and a package list",
        ));
    };
    let mut names = HashSet::new();
    for row in packages {
        if !row.is_object() {
            return Err(config("invalid package_config.json dependency entry"));
        }
        let name = row.get("name").and_then(Value::as_str);
        let root = row.get("rootUri").and_then(Value::as_str);
        let valid = |text: &str| !text.is_empty() && !text.contains('\0');
        match (name, root) {
            (Some(name), Some(root)) if valid(name) && valid(root) && names.insert(name) => {}
            _ => {
                return Err(config(
                    "invalid or duplicate package_config.json dependency entry",
                ));
            }
        }
    }
    Ok(Some(names.contains("flutter_test")))
}

pub(crate) fn flutter_package(project: &Path) -> Result<bool, ProfileError> {
    let bytes = paths::read_regular_bytes(&project.join("pubspec.yaml"), MAX_YAML_BYTES, false)?;
    if bytes.len() > MAX_YAML_BYTES {
        return Err(config("project pubspec.yaml exceeds 1 MiB"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid_yaml())?;
    let declared = pubspec_flutter(text)?;
    Ok(prepared_flutter_package(project)?.unwrap_or(declared))
}

#[cfg(test)]
mod tests;
