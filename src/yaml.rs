//! YAML subset used by Cordis patch and settings files.
//!
//! Supported: block mappings and sequences, flow `[...]`/`{...}` collections,
//! plain/single/double-quoted scalars, `|` and `>` block scalars, comments,
//! and tags. Tagged scalars (`!!js`, `!env`, ...) are handed to the caller's
//! resolver, so the loader decides which tags are legal. Anchors, aliases,
//! multi-document streams, and complex keys are rejected.

use crate::json::Json;

pub type TagResolver<'a> = dyn FnMut(&str, &str) -> Result<Json, String> + 'a;

pub fn parse(text: &str, resolve: &mut TagResolver<'_>) -> Result<Json, String> {
    let mut lines = Vec::new();
    for (index, raw) in text.split('\n').enumerate() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.contains('\t') && raw.trim_start_matches(' ').starts_with('\t') {
            return Err(format!("line {}: tabs are not allowed for indentation", index + 1));
        }
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let content = raw[indent..].to_string();
        let blank = content.trim().is_empty() || content.starts_with('#');
        if index == 0 && content.starts_with("%") {
            return Err("YAML directives are not supported".into());
        }
        lines.push(Line { indent, text: content, blank, number: index + 1 });
    }
    let mut parser = Parser { lines, pos: 0, resolve };
    parser.skip_blank();
    if parser.pos < parser.lines.len() && parser.lines[parser.pos].text.trim_end() == "---" {
        parser.pos += 1;
    }
    parser.skip_blank();
    if parser.pos >= parser.lines.len() {
        return Ok(Json::Null);
    }
    let indent = parser.lines[parser.pos].indent;
    let value = parser.node(indent)?;
    parser.skip_blank();
    if parser.pos < parser.lines.len() {
        let line = &parser.lines[parser.pos];
        return Err(format!("line {}: unexpected content {:?}", line.number, line.text));
    }
    Ok(value)
}

struct Line {
    indent: usize,
    text: String,
    blank: bool,
    number: usize,
}

struct Parser<'r, 'a> {
    lines: Vec<Line>,
    pos: usize,
    resolve: &'r mut TagResolver<'a>,
}

fn is_seq_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

impl Parser<'_, '_> {
    fn skip_blank(&mut self) {
        while self.pos < self.lines.len() && self.lines[self.pos].blank {
            self.pos += 1;
        }
    }

    fn err(&self, message: &str) -> String {
        let number = self.lines.get(self.pos).map(|l| l.number).unwrap_or(self.lines.len());
        format!("line {number}: {message}")
    }

    fn node(&mut self, indent: usize) -> Result<Json, String> {
        self.skip_blank();
        let Some(line) = self.lines.get(self.pos) else { return Ok(Json::Null) };
        if line.text.starts_with("&") || line.text.starts_with("*") {
            return Err(self.err("anchors and aliases are not supported"));
        }
        if is_seq_item(&line.text) {
            self.sequence(indent)
        } else if split_key(&line.text)?.is_some() {
            self.mapping(indent)
        } else {
            let text = line.text.clone();
            self.pos += 1;
            self.inline(&text)
        }
    }

    fn sequence(&mut self, indent: usize) -> Result<Json, String> {
        let mut items = Vec::new();
        loop {
            self.skip_blank();
            let Some(line) = self.lines.get(self.pos) else { break };
            if line.indent != indent || !is_seq_item(&line.text) {
                if line.indent > indent {
                    return Err(self.err("bad indentation in sequence"));
                }
                break;
            }
            let rest = line.text[1..].to_string();
            let spaces = rest.len() - rest.trim_start_matches(' ').len();
            let rest = rest.trim_start_matches(' ').to_string();
            if rest.is_empty() || rest.starts_with('#') {
                self.pos += 1;
                self.skip_blank();
                match self.lines.get(self.pos) {
                    Some(next) if next.indent > indent => {
                        let child = next.indent;
                        items.push(self.node(child)?);
                    }
                    _ => items.push(Json::Null),
                }
            } else {
                // Re-read "- key: value" as a block starting at the item's column.
                let column = indent + 1 + spaces;
                let line = &mut self.lines[self.pos];
                line.indent = column;
                line.text = rest;
                items.push(self.node(column)?);
            }
        }
        Ok(Json::Arr(items))
    }

    fn mapping(&mut self, indent: usize) -> Result<Json, String> {
        let mut object = Json::obj();
        loop {
            self.skip_blank();
            let Some(line) = self.lines.get(self.pos) else { break };
            if line.indent != indent || is_seq_item(&line.text) {
                if line.indent > indent {
                    return Err(self.err("bad indentation in mapping"));
                }
                break;
            }
            let text = line.text.clone();
            let Some((key, value)) = split_key(&text)? else {
                return Err(self.err("expected 'key: value'"));
            };
            if object.as_object().is_some_and(|fields| fields.iter().any(|(k, _)| *k == key)) {
                return Err(self.err(&format!("duplicate key {key:?}")));
            }
            self.pos += 1;
            let value = value.trim();
            let parsed = if value.is_empty() || value.starts_with('#') {
                self.skip_blank();
                match self.lines.get(self.pos) {
                    Some(next) if next.indent > indent => {
                        let child = next.indent;
                        self.node(child)?
                    }
                    Some(next) if next.indent == indent && is_seq_item(&next.text) => self.sequence(indent)?,
                    _ => Json::Null,
                }
            } else if let Some(header) = block_header(value) {
                self.block_scalar(indent, header)?
            } else {
                self.inline(value)?
            };
            object.set(&key, parsed);
        }
        Ok(object)
    }

    fn block_scalar(&mut self, parent: usize, (folded, chomp): (bool, char)) -> Result<Json, String> {
        let mut body: Vec<String> = Vec::new();
        let mut block_indent = None;
        while let Some(line) = self.lines.get(self.pos) {
            let raw_blank = line.text.trim().is_empty();
            if raw_blank {
                body.push(String::new());
                self.pos += 1;
                continue;
            }
            let required = *block_indent.get_or_insert(line.indent);
            if line.indent < required || line.indent <= parent {
                break;
            }
            body.push(format!("{}{}", " ".repeat(line.indent - required), line.text));
            self.pos += 1;
        }
        let trailing = body.iter().rev().take_while(|l| l.is_empty()).count();
        let content_len = body.len() - trailing;
        let content = &body[..content_len];
        let mut text = if folded {
            let mut out = String::new();
            for (i, line) in content.iter().enumerate() {
                if i > 0 {
                    out.push(if line.is_empty() || content[i - 1].is_empty() { '\n' } else { ' ' });
                }
                out.push_str(line);
            }
            out
        } else {
            content.join("\n")
        };
        match chomp {
            '-' => {}
            '+' => {
                text.push('\n');
                for _ in 0..trailing {
                    text.push('\n');
                }
            }
            _ => {
                if !content.is_empty() {
                    text.push('\n');
                }
            }
        }
        Ok(Json::Str(text))
    }

    fn inline(&mut self, text: &str) -> Result<Json, String> {
        let text = text.trim();
        let (tag, rest) = if text.starts_with('!') {
            let end = text.find(' ').unwrap_or(text.len());
            (Some(&text[..end]), text[end..].trim_start())
        } else {
            (None, text)
        };
        if rest.starts_with('&') || rest.starts_with('*') {
            return Err(self.err("anchors and aliases are not supported"));
        }
        let mut cursor = Cursor { s: rest, pos: 0 };
        let value = if rest.starts_with('[') || rest.starts_with('{') {
            if tag.is_some() {
                return Err(self.err("tags on flow collections are not supported"));
            }
            let v = cursor.flow(0).map_err(|e| self.err(&e))?;
            cursor.trailing().map_err(|e| self.err(&e))?;
            return Ok(v);
        } else if rest.starts_with('"') || rest.starts_with('\'') {
            let s = cursor.quoted().map_err(|e| self.err(&e))?;
            cursor.trailing().map_err(|e| self.err(&e))?;
            Scalar { text: s, quoted: true }
        } else {
            Scalar { text: strip_comment(rest).to_string(), quoted: false }
        };
        match tag {
            None => Ok(resolve_plain(value)),
            Some("!!str") => Ok(Json::Str(value.text)),
            Some(tag) => (self.resolve)(tag, &value.text).map_err(|e| self.err(&e)),
        }
    }
}

struct Scalar {
    text: String,
    quoted: bool,
}

fn resolve_plain(value: Scalar) -> Json {
    if value.quoted {
        return Json::Str(value.text);
    }
    let t = value.text.trim();
    match t {
        "" | "~" | "null" | "Null" | "NULL" => return Json::Null,
        "true" | "True" | "TRUE" => return Json::Bool(true),
        "false" | "False" | "FALSE" => return Json::Bool(false),
        _ => {}
    }
    let numeric = t.trim_start_matches(['-', '+']);
    if !numeric.is_empty() && numeric.bytes().all(|b| b.is_ascii_digit() || b == b'_')
        && let Ok(i) = t.replace('_', "").parse::<i64>() {
            return Json::Int(i);
        }
    if let Some(hex) = t.strip_prefix("0x")
        && let Ok(i) = i64::from_str_radix(hex, 16) {
            return Json::Int(i);
        }
    if t.bytes().any(|b| b.is_ascii_digit()) && t.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b))
        && let Ok(f) = t.parse::<f64>() {
            return Json::Float(f);
        }
    Json::Str(t.to_string())
}

fn block_header(value: &str) -> Option<(bool, char)> {
    let value = strip_comment(value).trim();
    let mut chars = value.chars();
    let folded = match chars.next()? {
        '|' => false,
        '>' => true,
        _ => return None,
    };
    let chomp = match chars.next() {
        None => ' ',
        Some(c @ ('-' | '+')) if chars.next().is_none() => c,
        _ => return None,
    };
    Some((folded, chomp))
}

fn strip_comment(text: &str) -> &str {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'#' && (i == 0 || bytes[i - 1] == b' ') {
            return text[..i].trim_end();
        }
    }
    text.trim_end()
}

/// Split `key: value` (or `key:`) outside quotes. Returns None for scalars.
fn split_key(text: &str) -> Result<Option<(String, String)>, String> {
    if text.starts_with('"') || text.starts_with('\'') {
        let mut cursor = Cursor { s: text, pos: 0 };
        let key = cursor.quoted()?;
        let rest = &text[cursor.pos..];
        let rest_trim = rest.trim_start();
        if let Some(after) = rest_trim.strip_prefix(':')
            && (after.is_empty() || after.starts_with(' ')) {
                return Ok(Some((key, after.to_string())));
            }
        return Ok(None);
    }
    if text.starts_with('[') || text.starts_with('{') || text.starts_with('!') {
        return Ok(None);
    }
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'#' && i > 0 && bytes[i - 1] == b' ' {
            return Ok(None);
        }
        if bytes[i] == b':' && (i + 1 == bytes.len() || bytes[i + 1] == b' ') {
            let key = text[..i].trim();
            if key.is_empty() {
                return Ok(None);
            }
            if key.starts_with('?') {
                return Err("complex mapping keys are not supported".into());
            }
            return Ok(Some((key.to_string(), text[i + 1..].to_string())));
        }
    }
    Ok(None)
}

struct Cursor<'a> {
    s: &'a str,
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<char> {
        self.s[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn ws(&mut self) {
        while self.peek() == Some(' ') {
            self.pos += 1;
        }
    }

    fn trailing(&mut self) -> Result<(), String> {
        self.ws();
        match self.peek() {
            None | Some('#') => Ok(()),
            Some(c) => Err(format!("unexpected {c:?} after value")),
        }
    }

    fn quoted(&mut self) -> Result<String, String> {
        let quote = self.bump().ok_or("expected quote")?;
        let mut out = String::new();
        loop {
            let c = self.bump().ok_or("unterminated quoted string (multi-line quoted scalars are not supported)")?;
            if c == quote {
                if quote == '\'' && self.peek() == Some('\'') {
                    self.pos += 1;
                    out.push('\'');
                    continue;
                }
                return Ok(out);
            }
            if c == '\\' && quote == '"' {
                let escape = self.bump().ok_or("bad escape")?;
                match escape {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    'e' => out.push('\u{1b}'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    ' ' => out.push(' '),
                    'u' | 'x' | 'U' => {
                        let len = match escape {
                            'x' => 2,
                            'u' => 4,
                            _ => 8,
                        };
                        let hex = self.s.get(self.pos..self.pos + len).ok_or("short escape")?;
                        let code = u32::from_str_radix(hex, 16).map_err(|_| "bad escape")?;
                        self.pos += len;
                        out.push(char::from_u32(code).ok_or("bad code point")?);
                    }
                    other => return Err(format!("unknown escape \\{other}")),
                }
                continue;
            }
            out.push(c);
        }
    }

    fn flow(&mut self, depth: usize) -> Result<Json, String> {
        if depth > 64 {
            return Err("flow collection nested too deeply".into());
        }
        self.ws();
        match self.peek() {
            Some('[') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.peek() == Some(']') {
                        self.pos += 1;
                        return Ok(Json::Arr(items));
                    }
                    items.push(self.flow(depth + 1)?);
                    self.ws();
                    match self.bump() {
                        Some(',') => continue,
                        Some(']') => return Ok(Json::Arr(items)),
                        _ => return Err("expected ',' or ']' in flow sequence".into()),
                    }
                }
            }
            Some('{') => {
                self.pos += 1;
                let mut object = Json::obj();
                loop {
                    self.ws();
                    if self.peek() == Some('}') {
                        self.pos += 1;
                        return Ok(object);
                    }
                    let key = match self.peek() {
                        Some('"' | '\'') => self.quoted()?,
                        _ => self.plain(&[':', ',', '}'])?,
                    };
                    self.ws();
                    if self.bump() != Some(':') {
                        return Err("expected ':' in flow mapping".into());
                    }
                    let value = self.flow(depth + 1)?;
                    object.set(key.trim(), value);
                    self.ws();
                    match self.bump() {
                        Some(',') => continue,
                        Some('}') => return Ok(object),
                        _ => return Err("expected ',' or '}' in flow mapping".into()),
                    }
                }
            }
            Some('"' | '\'') => Ok(Json::Str(self.quoted()?)),
            _ => {
                let text = self.plain(&[',', ']', '}'])?;
                Ok(resolve_plain(Scalar { text, quoted: false }))
            }
        }
    }

    fn plain(&mut self, stops: &[char]) -> Result<String, String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if stops.contains(&c) {
                break;
            }
            self.pos += c.len_utf8();
        }
        Ok(self.s[start..self.pos].trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_tags(tag: &str, _: &str) -> Result<Json, String> {
        Err(format!("tag {tag} not allowed"))
    }

    #[test]
    fn parses_patch_shapes() {
        let text = r#"
# comment
- id: system-prompt
  config:
    personaPrefix: !!js process.env.DSH_SYSTEM_PROMPT
- id: llm-deepseek
  disabled: true   # trailing comment
- insert:
    - id: mcp-tools
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        url: http://127.0.0.1:8000/v1/agents/a%20b/mcp
        headers:
          Authorization: !!js '`Bearer ${process.env.GATEWAY_TOKEN}`'
"#;
        let mut seen = Vec::new();
        let value = parse(text, &mut |tag, body| {
            seen.push((tag.to_string(), body.to_string()));
            Ok(Json::Str(format!("<{body}>")))
        })
        .unwrap();
        assert_eq!(value.at(0).get("id").as_str(), Some("system-prompt"));
        assert_eq!(value.at(1).get("disabled").as_bool(), Some(true));
        let inserted = value.at(2).get("insert").at(0);
        assert_eq!(inserted.get("name").as_str(), Some("@deepseek-ai/dsh-mcp-client"));
        assert_eq!(inserted.get("config").get("url").as_str(), Some("http://127.0.0.1:8000/v1/agents/a%20b/mcp"));
        assert_eq!(seen[1].1, "`Bearer ${process.env.GATEWAY_TOKEN}`");
    }

    #[test]
    fn parses_scalars_flow_and_blocks() {
        let text = "a: 1\nb: 2.5\nc: [x, 'y', {k: v}]\nd: |\n  line1\n  line2\ne: \"q\\n\"\nf:\n- 1\n- two\n";
        let value = parse(text, &mut no_tags).unwrap();
        assert_eq!(value.get("a").as_i64(), Some(1));
        assert_eq!(value.get("b").as_f64(), Some(2.5));
        assert_eq!(value.get("c").at(2).get("k").as_str(), Some("v"));
        assert_eq!(value.get("d").as_str(), Some("line1\nline2\n"));
        assert_eq!(value.get("e").as_str(), Some("q\n"));
        assert_eq!(value.get("f").at(1).as_str(), Some("two"));
    }

    #[test]
    fn rejects_unknown_tags_and_aliases() {
        assert!(parse("a: !!python/object x", &mut no_tags).is_err());
        assert!(parse("a: *ref", &mut no_tags).is_err());
        assert!(parse("a: 1\na: 2", &mut no_tags).is_err());
    }
}
