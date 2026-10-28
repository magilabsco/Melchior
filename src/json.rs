//! Order-preserving JSON value, parser, and serializer.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Json {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

pub static NULL: Json = Json::Null;

/// Compact JSON, as sent on the wire.
impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        write_json(&mut out, self, None, 0);
        f.write_str(&out)
    }
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    pub fn get(&self, key: &str) -> &Json {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or(&NULL),
            _ => &NULL,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(fields) => fields.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn at(&self, index: usize) -> &Json {
        match self {
            Json::Arr(items) => items.get(index).unwrap_or(&NULL),
            _ => &NULL,
        }
    }

    /// Insert or replace a key, keeping the original position on replace.
    pub fn set(&mut self, key: &str, value: impl Into<Json>) -> &mut Json {
        if !matches!(self, Json::Obj(_)) {
            *self = Json::obj();
        }
        let value = value.into();
        if let Json::Obj(fields) = self {
            if let Some(slot) = fields.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                fields.push((key.to_string(), value));
            }
        }
        self
    }

    pub fn with(mut self, key: &str, value: impl Into<Json>) -> Json {
        self.set(key, value);
        self
    }

    pub fn remove(&mut self, key: &str) -> Option<Json> {
        if let Json::Obj(fields) = self
            && let Some(pos) = fields.iter().position(|(k, _)| k == key) {
                return Some(fields.remove(pos).1);
            }
        None
    }

    pub fn push(&mut self, value: impl Into<Json>) {
        if !matches!(self, Json::Arr(_)) {
            *self = Json::Arr(Vec::new());
        }
        if let Json::Arr(items) = self {
            items.push(value.into());
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn str_or<'a>(&'a self, default: &'a str) -> &'a str {
        self.as_str().unwrap_or(default)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            Json::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Vec<(String, Json)>> {
        match self {
            Json::Obj(fields) => Some(fields),
            _ => None,
        }
    }

    pub fn items(&self) -> &[Json] {
        match self {
            Json::Arr(items) => items,
            _ => &[],
        }
    }

    pub fn entries(&self) -> &[(String, Json)] {
        match self {
            Json::Obj(fields) => fields,
            _ => &[],
        }
    }

    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        write_json(&mut out, self, Some(2), 0);
        out
    }

    /// Recursively merge `patch` into `self`: objects merge key by key,
    /// everything else (including arrays) is replaced.
    pub fn deep_merge(&mut self, patch: &Json) {
        match (self, patch) {
            (Json::Obj(base), Json::Obj(fields)) => {
                for (key, value) in fields {
                    if let Some(slot) = base.iter_mut().find(|(k, _)| k == key) {
                        slot.1.deep_merge(value);
                    } else {
                        base.push((key.clone(), value.clone()));
                    }
                }
            }
            (slot, value) => *slot = value.clone(),
        }
    }
}

impl From<&str> for Json {
    fn from(v: &str) -> Self {
        Json::Str(v.to_string())
    }
}
impl From<String> for Json {
    fn from(v: String) -> Self {
        Json::Str(v)
    }
}
impl From<&String> for Json {
    fn from(v: &String) -> Self {
        Json::Str(v.clone())
    }
}
impl From<bool> for Json {
    fn from(v: bool) -> Self {
        Json::Bool(v)
    }
}
impl From<i64> for Json {
    fn from(v: i64) -> Self {
        Json::Int(v)
    }
}
impl From<i32> for Json {
    fn from(v: i32) -> Self {
        Json::Int(v as i64)
    }
}
impl From<u64> for Json {
    fn from(v: u64) -> Self {
        Json::Int(v as i64)
    }
}
impl From<usize> for Json {
    fn from(v: usize) -> Self {
        Json::Int(v as i64)
    }
}
impl From<f64> for Json {
    fn from(v: f64) -> Self {
        Json::Float(v)
    }
}
impl From<Vec<Json>> for Json {
    fn from(v: Vec<Json>) -> Self {
        Json::Arr(v)
    }
}
impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(v: Option<T>) -> Self {
        v.map(Into::into).unwrap_or(Json::Null)
    }
}

/// Build a JSON object: `jobj! { "a" => 1, "b" => "x" }`.
#[macro_export]
macro_rules! jobj {
    () => { $crate::json::Json::obj() };
    ($($key:expr => $value:expr),+ $(,)?) => {{
        let mut object = $crate::json::Json::obj();
        $( object.set($key, $value); )+
        object
    }};
}

/// Build a JSON array: `jarr![1, "x"]`.
#[macro_export]
macro_rules! jarr {
    ($($value:expr),* $(,)?) => {
        $crate::json::Json::Arr(vec![$($crate::json::Json::from($value)),*])
    };
}

pub fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_json(out: &mut String, value: &Json, indent: Option<usize>, depth: usize) {
    let newline = |out: &mut String, depth: usize| {
        if let Some(width) = indent {
            out.push('\n');
            for _ in 0..width * depth {
                out.push(' ');
            }
        }
    };
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Json::Float(f) => {
            if !f.is_finite() {
                out.push_str("null");
            } else if f.fract() == 0.0 && f.abs() < 1e15 {
                let _ = write!(out, "{f:.1}");
            } else {
                let _ = write!(out, "{f}");
            }
        }
        Json::Str(s) => escape_into(out, s),
        Json::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_json(out, item, indent, depth + 1);
            }
            newline(out, depth);
            out.push(']');
        }
        Json::Obj(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                escape_into(out, key);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_json(out, item, indent, depth + 1);
            }
            newline(out, depth);
            out.push('}');
        }
    }
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

const MAX_DEPTH: usize = 256;

pub fn parse(input: &str) -> Result<Json, ParseError> {
    let mut parser = Parser { bytes: input.as_bytes(), src: input, pos: 0 };
    parser.skip_ws();
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("trailing characters"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    src: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> ParseError {
        ParseError { message: message.to_string(), offset: self.pos }
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, literal: &str, value: Json) -> Result<Json, ParseError> {
        if self.src[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        match self.bytes.get(self.pos) {
            None => Err(self.error("unexpected end of input")),
            Some(b'n') => self.expect("null", Json::Null),
            Some(b't') => self.expect("true", Json::Bool(true)),
            Some(b'f') => self.expect("false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.bytes.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                loop {
                    self.skip_ws();
                    items.push(self.value(depth + 1)?);
                    self.skip_ws();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Json::Arr(items));
                        }
                        _ => return Err(self.error("expected ',' or ']'")),
                    }
                }
            }
            Some(b'{') => {
                self.pos += 1;
                let mut fields: Vec<(String, Json)> = Vec::new();
                self.skip_ws();
                if self.bytes.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(Json::Obj(fields));
                }
                loop {
                    self.skip_ws();
                    if self.bytes.get(self.pos) != Some(&b'"') {
                        return Err(self.error("expected object key"));
                    }
                    let key = self.string()?;
                    self.skip_ws();
                    if self.bytes.get(self.pos) != Some(&b':') {
                        return Err(self.error("expected ':'"));
                    }
                    self.pos += 1;
                    self.skip_ws();
                    let value = self.value(depth + 1)?;
                    if let Some(slot) = fields.iter_mut().find(|(k, _)| *k == key) {
                        slot.1 = value;
                    } else {
                        fields.push((key, value));
                    }
                    self.skip_ws();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Json::Obj(fields));
                        }
                        _ => return Err(self.error("expected ',' or '}'")),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.pos == digits_start {
            return Err(self.error("invalid number"));
        }
        let mut float = false;
        if self.bytes.get(self.pos) == Some(&b'.') {
            float = true;
            self.pos += 1;
            let frac = self.pos;
            while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == frac {
                return Err(self.error("invalid number"));
            }
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            float = true;
            self.pos += 1;
            if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exp = self.pos;
            while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == exp {
                return Err(self.error("invalid number"));
            }
        }
        let text = &self.src[start..self.pos];
        if !float
            && let Ok(i) = text.parse::<i64>() {
                return Ok(Json::Int(i));
            }
        text.parse::<f64>().map(Json::Float).map_err(|_| self.error("invalid number"))
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let chunk = self.src.get(self.pos..self.pos + 4).ok_or_else(|| self.error("short unicode escape"))?;
        let code = u32::from_str_radix(chunk, 16).map_err(|_| self.error("invalid unicode escape"))?;
        self.pos += 4;
        Ok(code)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&b) = self.bytes.get(self.pos) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(&self.src[start..self.pos]);
            match self.bytes.get(self.pos) {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = *self.bytes.get(self.pos).ok_or_else(|| self.error("bad escape"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&first) {
                                if self.src[self.pos..].starts_with("\\u") {
                                    self.pos += 2;
                                    let second = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&second) {
                                        return Err(self.error("invalid surrogate pair"));
                                    }
                                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                                } else {
                                    0xFFFD
                                }
                            } else if (0xDC00..0xE000).contains(&first) {
                                0xFFFD
                            } else {
                                first
                            };
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(self.error("bad escape")),
                    }
                }
                Some(_) => return Err(self.error("control character in string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_nested_values() {
        let text = r#"{"a":[1,2.5,-3e2,true,null],"b":{"c":"x\ny\u00e9\ud83d\ude00"},"a2":""}"#;
        let value = parse(text).unwrap();
        assert_eq!(value.get("b").get("c").as_str(), Some("x\nyé😀"));
        assert_eq!(value.get("a").at(2).as_f64(), Some(-300.0));
        assert_eq!(parse(&value.to_string()).unwrap(), value);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("deliberately corrupt").is_err());
        assert!(parse("{\"a\":1,}").is_err());
        assert!(parse("[1] x").is_err());
    }

    #[test]
    fn deep_merge_replaces_arrays_and_merges_objects() {
        let mut base = parse(r#"{"a":{"x":1,"y":[1]},"b":2}"#).unwrap();
        base.deep_merge(&parse(r#"{"a":{"y":[2],"z":3}}"#).unwrap());
        assert_eq!(base.to_string(), r#"{"a":{"x":1,"y":[2],"z":3},"b":2}"#);
    }
}
