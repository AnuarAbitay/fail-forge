//! A byte scanner for namespace prefixes, not a Dart binding resolver.
//!
//! Ranges always refer to the original UTF-8 bytes. In particular, interpolation
//! recursively discovers literals and identifiers without flattening strings or
//! copying their overlapping contents. Only the adapter resolves SDK elements.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use failforge_workspace::Cancellation;
use regex::Regex;

use super::{NamespaceError, invalid};

pub(super) const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_NESTING: usize = 128;
pub(super) const MAX_ITEMS: usize = 1_000_000;
const CHECK_INTERVAL: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TokenKind {
    Identifier,
    Symbol,
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
    pub content_start: usize,
    pub content_end: usize,
    pub raw_string: bool,
    pub interpolated: bool,
}

#[derive(Debug)]
pub(super) struct Scanner {
    raw: Vec<u8>,
    tokens: Vec<Token>,
    strings: Vec<Token>,
    comments: Vec<(usize, usize)>,
    code_identifiers: BTreeSet<Vec<u8>>,
    item_count: usize,
}

fn check(cancel: &Cancellation) -> Result<(), NamespaceError> {
    match cancel.signum() {
        Some(signal) => Err(NamespaceError::Cancelled(signal)),
        None => Ok(()),
    }
}

struct Budget<'a> {
    cancel: &'a Cancellation,
    steps: usize,
    items: usize,
}

impl<'a> Budget<'a> {
    fn new(cancel: &'a Cancellation) -> Result<Self, NamespaceError> {
        check(cancel)?;
        Ok(Self {
            cancel,
            steps: 0,
            items: 0,
        })
    }

    fn step(&mut self) -> Result<(), NamespaceError> {
        self.steps += 1;
        if self.steps % CHECK_INTERVAL == 0 {
            check(self.cancel)?;
        }
        Ok(())
    }

    fn item(&mut self) -> Result<(), NamespaceError> {
        self.items += 1;
        if self.items > MAX_ITEMS {
            return Err(invalid("namespace scanner item limit exceeded"));
        }
        self.step()
    }
}

impl Scanner {
    pub fn new(raw: &[u8], cancel: &Cancellation) -> Result<Self, NamespaceError> {
        let mut budget = Budget::new(cancel)?;
        if raw.len() > MAX_SOURCE_BYTES {
            return Err(invalid("namespace source exceeds eight MiB"));
        }
        std::str::from_utf8(raw).map_err(|_| invalid("namespace source is not UTF-8"))?;
        let mut scanner = Self {
            raw: raw.to_vec(),
            tokens: Vec::new(),
            strings: Vec::new(),
            comments: Vec::new(),
            code_identifiers: BTreeSet::new(),
            item_count: 0,
        };
        let start = if raw.starts_with(b"\xef\xbb\xbf") {
            3
        } else {
            0
        };
        scanner.tokens = scanner.code(start, false, 0, &mut budget)?.0;
        scanner.item_count = budget.items;
        scanner.comments.sort_unstable();
        check(cancel)?;
        Ok(scanner)
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn value(&self, token: &Token) -> &[u8] {
        if token.kind == TokenKind::String {
            &self.raw[token.content_start..token.content_end]
        } else {
            &self.raw[token.start..token.end]
        }
    }

    pub fn strings(&self) -> &[Token] {
        &self.strings
    }

    pub fn code_identifiers(&self) -> &BTreeSet<Vec<u8>> {
        &self.code_identifiers
    }

    pub fn item_count(&self) -> usize {
        self.item_count
    }

    fn starts(&self, index: usize, bytes: &[u8]) -> bool {
        self.raw[index..].starts_with(bytes)
    }

    fn comment(&mut self, start: usize, budget: &mut Budget<'_>) -> Result<usize, NamespaceError> {
        budget.item()?;
        let mut index = start + 2;
        if self.starts(start, b"//") || (matches!(start, 0 | 3) && self.starts(start, b"#!")) {
            while index < self.raw.len() {
                budget.step()?;
                let byte = self.raw[index];
                index += 1;
                if byte == b'\n' {
                    break;
                }
            }
        } else {
            let mut depth = 1_usize;
            while index < self.raw.len() && depth > 0 {
                budget.step()?;
                if self.starts(index, b"/*") {
                    depth += 1;
                    index += 2;
                } else if self.starts(index, b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            if depth != 0 {
                return Err(invalid("unterminated header comment"));
            }
        }
        self.comments.push((start, index));
        Ok(index)
    }

    fn string(
        &mut self,
        start: usize,
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<Token, NamespaceError> {
        let raw_string = self.raw[start] == b'r';
        let quote = start + usize::from(raw_string);
        let delimiter = self.raw[quote];
        let width = if self.starts(quote, &[delimiter; 3]) {
            3
        } else {
            1
        };
        let content_start = quote + width;
        let mut index = content_start;
        let mut interpolated = false;
        while index < self.raw.len() {
            budget.step()?;
            if self.starts(index, &[delimiter; 3][..width]) {
                let token = Token {
                    kind: TokenKind::String,
                    start,
                    end: index + width,
                    content_start,
                    content_end: index,
                    raw_string,
                    interpolated,
                };
                self.strings.push(token);
                return Ok(token);
            }
            if !raw_string && self.raw[index] == b'\\' {
                index += 2;
            } else if !raw_string && self.starts(index, b"${") {
                interpolated = true;
                if depth >= MAX_NESTING {
                    return Err(invalid("namespace interpolation nesting limit exceeded"));
                }
                index = self.code(index + 2, true, depth + 1, budget)?.1;
            } else if !raw_string && self.raw[index] == b'$' {
                interpolated = true;
                index += 1;
                if self
                    .raw
                    .get(index)
                    .is_some_and(|byte| identifier_start(*byte))
                {
                    index = self.identifier_end(index, budget)?;
                }
            } else {
                index += 1;
            }
        }
        Err(invalid("unterminated Dart string literal"))
    }

    fn identifier_end(
        &self,
        start: usize,
        budget: &mut Budget<'_>,
    ) -> Result<usize, NamespaceError> {
        let mut end = start + 1;
        while self
            .raw
            .get(end)
            .is_some_and(|byte| identifier_continue(*byte))
        {
            budget.step()?;
            end += 1;
        }
        Ok(end)
    }

    fn code(
        &mut self,
        mut index: usize,
        interpolation: bool,
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<(Vec<Token>, usize), NamespaceError> {
        let mut tokens = Vec::new();
        let mut braces = 0_usize;
        while index < self.raw.len() {
            budget.step()?;
            let byte = self.raw[index];
            // Python's bytes.isspace() also includes vertical tab; Rust's
            // ASCII helper intentionally omits it.
            if byte.is_ascii_whitespace() || byte == b'\x0b' {
                index += 1;
                continue;
            }
            if self.starts(index, b"//")
                || self.starts(index, b"/*")
                || (matches!(index, 0 | 3) && self.starts(index, b"#!"))
            {
                index = self.comment(index, budget)?;
                continue;
            }
            budget.item()?;
            let token = if matches!(byte, b'\'' | b'"')
                || (byte == b'r'
                    && self
                        .raw
                        .get(index + 1)
                        .is_some_and(|byte| matches!(byte, b'\'' | b'"')))
            {
                self.string(index, depth, budget)?
            } else {
                if interpolation && byte == b'}' {
                    if braces == 0 {
                        return Ok((tokens, index + 1));
                    }
                    braces -= 1;
                } else if interpolation && byte == b'{' {
                    braces += 1;
                }
                let identifier = identifier_start(byte);
                let end = if identifier {
                    self.identifier_end(index, budget)?
                } else {
                    index + 1
                };
                if identifier {
                    self.code_identifiers.insert(self.raw[index..end].to_vec());
                }
                Token {
                    kind: if identifier {
                        TokenKind::Identifier
                    } else {
                        TokenKind::Symbol
                    },
                    start: index,
                    end,
                    content_start: 0,
                    content_end: 0,
                    raw_string: false,
                    interpolated: false,
                }
            };
            index = token.end;
            // Interpolated code contributes identifiers, comments and strings,
            // but cannot introduce a top-level import directive.
            if !interpolation {
                tokens.push(token);
            }
        }
        if interpolation {
            return Err(invalid("unterminated Dart interpolation"));
        }
        Ok((tokens, index))
    }

    fn balanced(
        &self,
        mut position: usize,
        opening: &[u8],
        closing: &[u8],
        budget: &mut Budget<'_>,
    ) -> Result<usize, NamespaceError> {
        let mut depth = 0_usize;
        while let Some(token) = self.tokens.get(position) {
            budget.step()?;
            let value = self.value(token);
            if token.kind != TokenKind::String {
                if value == opening {
                    depth += 1;
                } else if value == closing {
                    depth = depth
                        .checked_sub(1)
                        .ok_or_else(|| invalid("unterminated header annotation or condition"))?;
                    if depth == 0 {
                        return Ok(position + 1);
                    }
                }
            }
            position += 1;
        }
        Err(invalid("unterminated header annotation or condition"))
    }

    fn token_is(&self, position: usize, value: &[u8]) -> bool {
        self.tokens
            .get(position)
            .is_some_and(|token| self.value(token) == value)
    }

    pub fn header_uris(&self, cancel: &Cancellation) -> Result<Vec<&Token>, NamespaceError> {
        let mut budget = Budget::new(cancel)?;
        let mut result = Vec::new();
        let mut index = 0;
        while index < self.tokens.len() {
            budget.step()?;
            while self.token_is(index, b"@") {
                index += 1;
                self.annotation_identifier(index)?;
                index += 1;
                loop {
                    budget.step()?;
                    if self.token_is(index, b".") {
                        index += 1;
                        self.annotation_identifier(index)?;
                        index += 1;
                    } else if self.token_is(index, b"<") {
                        index = self.balanced(index, b"<", b">", &mut budget)?;
                    } else {
                        break;
                    }
                }
                if self.token_is(index, b"(") {
                    index = self.balanced(index, b"(", b")", &mut budget)?;
                }
            }
            let Some(token) = self.tokens.get(index) else {
                break;
            };
            let keyword = self.value(token);
            if !matches!(keyword, b"import" | b"export" | b"part" | b"library") {
                break;
            }
            index += 1;
            if keyword == b"part" && self.token_is(index, b"of") {
                index += 1;
            }
            let conditional = matches!(keyword, b"import" | b"export");
            if let Some(uri) = self
                .tokens
                .get(index)
                .filter(|token| token.kind == TokenKind::String)
            {
                result.push(uri);
                index += 1;
            } else if conditional {
                return Err(invalid("unsupported header URI expression"));
            }
            while index < self.tokens.len() && !self.token_is(index, b";") {
                budget.step()?;
                let token = &self.tokens[index];
                if self.value(token) == b"if" && conditional {
                    index += 1;
                    if !self.token_is(index, b"(") {
                        return Err(invalid("unsupported conditional URI"));
                    }
                    index = self.balanced(index, b"(", b")", &mut budget)?;
                    let uri = self
                        .tokens
                        .get(index)
                        .filter(|token| token.kind == TokenKind::String)
                        .ok_or_else(|| invalid("unsupported conditional URI"))?;
                    result.push(uri);
                } else if token.kind == TokenKind::String {
                    return Err(invalid("unsupported header directive string"));
                }
                index += 1;
            }
            if index == self.tokens.len() {
                return Err(invalid("unterminated header directive"));
            }
            index += 1;
        }
        check(cancel)?;
        Ok(result)
    }

    fn annotation_identifier(&self, index: usize) -> Result<(), NamespaceError> {
        if self
            .tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Identifier)
        {
            Ok(())
        } else {
            Err(invalid("unsupported header annotation"))
        }
    }

    pub fn references(
        &self,
        prefix: &[u8],
        cancel: &Cancellation,
    ) -> Result<Vec<usize>, NamespaceError> {
        let mut budget = Budget::new(cancel)?;
        let prefix =
            std::str::from_utf8(prefix).map_err(|_| invalid("namespace prefix is not UTF-8"))?;
        if prefix.is_empty() {
            return Err(invalid("empty namespace prefix"));
        }
        let raw = std::str::from_utf8(&self.raw).expect("scanner source was validated");
        let mut index = 0;
        let mut result = Vec::new();
        while let Some(found) = raw[index..].find(prefix) {
            budget.step()?;
            let start = index + found;
            let comment = self
                .comments
                .partition_point(|(offset, _)| *offset <= start);
            if comment == 0 || self.comments[comment - 1].1 <= start {
                result.push(start);
            }
            index = start + prefix.len();
        }
        check(cancel)?;
        Ok(result)
    }
}

fn identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$' | 0x80..=0xff)
}

fn identifier_continue(byte: u8) -> bool {
    identifier_start(byte) || byte.is_ascii_digit()
}

fn literal_value(
    scanner: &Scanner,
    token: &Token,
    cancel: &Cancellation,
) -> Result<Option<String>, NamespaceError> {
    if token.interpolated {
        return Ok(None);
    }
    let raw = std::str::from_utf8(scanner.value(token)).expect("literal is a UTF-8 source range");
    if token.raw_string {
        return Ok(Some(raw.to_owned()));
    }
    let mut budget = Budget::new(cancel)?;
    let mut value = String::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        budget.step()?;
        let character = raw[index..].chars().next().expect("nonempty suffix");
        index += character.len_utf8();
        if character != '\\' || index == raw.len() {
            value.push(character);
            continue;
        }
        let character = raw[index..].chars().next().expect("escape suffix");
        index += character.len_utf8();
        let (decoded, consumed) = match character {
            'n' => ('\n', 0),
            'r' => ('\r', 0),
            't' => ('\t', 0),
            'b' => ('\u{8}', 0),
            'f' => ('\u{c}', 0),
            'v' => ('\u{b}', 0),
            'u' | 'x' => unicode_escape(raw, index, character)?,
            _ => (character, 0),
        };
        value.push(decoded);
        index += consumed;
    }
    Ok(Some(value))
}

fn unicode_escape(raw: &str, index: usize, kind: char) -> Result<(char, usize), NamespaceError> {
    let bytes = raw.as_bytes();
    let (start, count, consumed) = if kind == 'u' && bytes.get(index) == Some(&b'{') {
        let start = index + 1;
        let count = bytes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_hexdigit())
            .take(7)
            .count();
        if count == 0 || count > 6 || bytes.get(start + count) != Some(&b'}') {
            return Err(invalid("invalid Dart Unicode escape"));
        }
        (start, count, count + 2)
    } else {
        let count = if kind == 'u' { 4 } else { 2 };
        if !bytes
            .get(index..index + count)
            .is_some_and(|bytes| bytes.iter().all(u8::is_ascii_hexdigit))
        {
            return Err(invalid("invalid Dart Unicode escape"));
        }
        (index, count, count)
    };
    let code =
        u32::from_str_radix(&raw[start..start + count], 16).expect("hex escape was validated");
    if code > 0x10ffff {
        return Err(invalid("Dart Unicode escape exceeds the Unicode range"));
    }
    // Dart permits UTF-16 surrogate escapes. They cannot spell any ASCII
    // package-prefix byte; replacing only these non-scalar guard characters
    // retains prefix detection without constructing an invalid Rust String.
    Ok((char::from_u32(code).unwrap_or('\u{fffd}'), consumed))
}

fn percent_decode(value: &str, cancel: &Cancellation) -> Result<String, NamespaceError> {
    let mut budget = Budget::new(cancel)?;
    let mut decoded = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        budget.step()?;
        if bytes[index] == b'%' {
            if let Some(hex) = bytes.get(index + 1..index + 3) {
                if let (Some(high), Some(low)) = (hex_digit(hex[0]), hex_digit(hex[1])) {
                    decoded.push(high * 16 + low);
                    index += 3;
                    continue;
                }
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    Ok(String::from_utf8_lossy(&decoded).into_owned())
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(super) fn check_uris(
    scanner: &Scanner,
    own_prefix: &[u8],
    alias_prefix: &[u8],
    allowed: &BTreeSet<usize>,
    cancel: &Cancellation,
) -> Result<(), NamespaceError> {
    let mut budget = Budget::new(cancel)?;
    for token in scanner.header_uris(cancel)? {
        budget.step()?;
        let value = scanner.value(token);
        if token.interpolated || value.contains(&b'%') || value.contains(&b'\\') {
            return Err(invalid(
                "encoded package URI is outside supported instrumentation",
            ));
        }
        if value
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"package:") && prefix != b"package:")
        {
            return Err(invalid(
                "noncanonical package scheme is outside supported instrumentation",
            ));
        }
    }
    let own =
        std::str::from_utf8(own_prefix).map_err(|_| invalid("namespace prefix is not UTF-8"))?;
    let alias =
        std::str::from_utf8(alias_prefix).map_err(|_| invalid("namespace prefix is not UTF-8"))?;
    static SCHEME: OnceLock<Regex> = OnceLock::new();
    let scheme =
        SCHEME.get_or_init(|| Regex::new("(?i)package:").expect("constant scheme expression"));
    for token in scanner.strings() {
        budget.step()?;
        let Some(value) = literal_value(scanner, token, cancel)? else {
            continue;
        };
        let decoded = percent_decode(&value, cancel)?;
        let normalized = scheme.replace_all(&decoded, "package:");
        let raw = std::str::from_utf8(scanner.value(token)).expect("validated source literal");
        for (prefix, message) in [
            (own, "unsupported author test package reference"),
            (alias, "author test already uses subject alias"),
        ] {
            if normalized.contains(prefix)
                && (value != decoded || !raw.contains(prefix) || normalized != value)
            {
                return Err(invalid(message));
            }
        }
    }
    if !scanner.references(alias_prefix, cancel)?.is_empty() {
        return Err(invalid("author test already uses subject alias"));
    }
    if scanner
        .references(own_prefix, cancel)?
        .iter()
        .any(|offset| !allowed.contains(offset))
    {
        return Err(invalid("unsupported author test package reference"));
    }
    check(cancel)
}

#[cfg(test)]
#[path = "scanner_tests.rs"]
mod tests;
