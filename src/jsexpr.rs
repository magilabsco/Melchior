//! Safe evaluator for the `!!js` tag in Cordis patch files.
//!
//! DeepSeek Harness evaluated `!!js` with a JavaScript engine. Real-world
//! patches only read environment variables, so this evaluator accepts exactly
//! that expression language and nothing that can execute code:
//!
//! - `process.env.NAME` and `process.env['NAME']`
//! - string, number, boolean, `null`, `undefined` literals
//! - template literals: `` `Bearer ${process.env.TOKEN}` ``
//! - array and object literals: `[{ id: process.env.MODEL }]`
//! - `a || b` and `a ?? b` defaults, and parentheses
//!
//! Anything else (calls, assignment, other globals) is a load error.

use crate::json::Json;

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Undefined,
    Json(Json),
}

impl Val {
    fn truthy(&self) -> bool {
        match self {
            Val::Undefined => false,
            Val::Json(j) => match j {
                Json::Null => false,
                Json::Bool(b) => *b,
                Json::Int(i) => *i != 0,
                Json::Float(f) => *f != 0.0 && !f.is_nan(),
                Json::Str(s) => !s.is_empty(),
                _ => true,
            },
        }
    }

    fn nullish(&self) -> bool {
        matches!(self, Val::Undefined | Val::Json(Json::Null))
    }

    fn to_js_string(&self) -> String {
        match self {
            Val::Undefined => "undefined".into(),
            Val::Json(Json::Null) => "null".into(),
            Val::Json(Json::Str(s)) => s.clone(),
            Val::Json(Json::Int(i)) => i.to_string(),
            Val::Json(Json::Float(f)) => f.to_string(),
            Val::Json(Json::Bool(b)) => b.to_string(),
            Val::Json(Json::Arr(items)) => {
                items.iter().map(|i| Val::Json(i.clone()).to_js_string()).collect::<Vec<_>>().join(",")
            }
            Val::Json(Json::Obj(_)) => "[object Object]".into(),
        }
    }

    fn into_json(self) -> Json {
        match self {
            Val::Undefined => Json::Null,
            Val::Json(j) => j,
        }
    }
}

pub fn eval(source: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Json, String> {
    let mut parser = Parser { src: source, pos: 0, env };
    let value = parser.expr(0)?;
    parser.ws();
    if parser.pos != source.len() {
        return Err(parser.unsupported());
    }
    Ok(value.into_json())
}

pub fn eval_process_env(source: &str) -> Result<Json, String> {
    eval(source, &|name| std::env::var(name).ok())
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    env: &'a dyn Fn(&str) -> Option<String>,
}

impl Parser<'_> {
    fn unsupported(&self) -> String {
        format!(
            "unsupported !!js expression {:?} near byte {} (Melchior evaluates only process.env reads, literals, templates, || and ??)",
            self.src, self.pos
        )
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn eat(&mut self, token: &str) -> bool {
        self.ws();
        if self.src[self.pos..].starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    fn ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn expr(&mut self, depth: usize) -> Result<Val, String> {
        if depth > 64 {
            return Err("!!js expression nested too deeply".into());
        }
        let mut left = self.primary(depth)?;
        loop {
            if self.eat("||") {
                let right = self.primary(depth)?;
                if !left.truthy() {
                    left = right;
                }
            } else if self.eat("??") {
                let right = self.primary(depth)?;
                if left.nullish() {
                    left = right;
                }
            } else {
                return Ok(left);
            }
        }
    }

    fn primary(&mut self, depth: usize) -> Result<Val, String> {
        self.ws();
        match self.peek() {
            Some('\'' | '"') => Ok(Val::Json(Json::Str(self.string()?))),
            Some('`') => self.template(depth),
            Some('[') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    if self.eat("]") {
                        return Ok(Val::Json(Json::Arr(items)));
                    }
                    items.push(self.expr(depth + 1)?.into_json());
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("]") {
                        return Ok(Val::Json(Json::Arr(items)));
                    }
                    return Err(self.unsupported());
                }
            }
            Some('{') => {
                self.pos += 1;
                let mut object = Json::obj();
                loop {
                    if self.eat("}") {
                        return Ok(Val::Json(object));
                    }
                    self.ws();
                    let key = match self.peek() {
                        Some('\'' | '"') => self.string()?,
                        _ => self.ident().ok_or_else(|| self.unsupported())?,
                    };
                    if !self.eat(":") {
                        return Err(self.unsupported());
                    }
                    let value = self.expr(depth + 1)?;
                    // JavaScript keeps `undefined` members; JSON consumers treat them as absent.
                    if value != Val::Undefined {
                        object.set(&key, value.into_json());
                    }
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("}") {
                        return Ok(Val::Json(object));
                    }
                    return Err(self.unsupported());
                }
            }
            Some('(') => {
                self.pos += 1;
                let value = self.expr(depth + 1)?;
                if !self.eat(")") {
                    return Err(self.unsupported());
                }
                Ok(value)
            }
            Some(c) if c.is_ascii_digit() || c == '-' => self.number(),
            Some(_) => {
                let start = self.pos;
                let name = self.ident().ok_or_else(|| self.unsupported())?;
                match name.as_str() {
                    "true" => Ok(Val::Json(Json::Bool(true))),
                    "false" => Ok(Val::Json(Json::Bool(false))),
                    "null" => Ok(Val::Json(Json::Null)),
                    "undefined" => Ok(Val::Undefined),
                    "process" => {
                        if !self.eat(".") || self.ident().as_deref() != Some("env") {
                            self.pos = start;
                            return Err(self.unsupported());
                        }
                        let key = if self.eat(".") {
                            self.ident().ok_or_else(|| self.unsupported())?
                        } else if self.eat("[") {
                            self.ws();
                            let key = self.string()?;
                            if !self.eat("]") {
                                return Err(self.unsupported());
                            }
                            key
                        } else {
                            return Err(self.unsupported());
                        };
                        self.ws();
                        if matches!(self.peek(), Some('.' | '[' | '(')) {
                            return Err(self.unsupported());
                        }
                        Ok((self.env)(&key).map(|v| Val::Json(Json::Str(v))).unwrap_or(Val::Undefined))
                    }
                    _ => {
                        self.pos = start;
                        Err(self.unsupported())
                    }
                }
            }
            None => Err(self.unsupported()),
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let name = &self.src[start..self.pos];
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            self.pos = start;
            None
        } else {
            Some(name.to_string())
        }
    }

    fn number(&mut self) -> Result<Val, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E') {
            self.pos += 1;
        }
        let text = &self.src[start..self.pos];
        if let Ok(i) = text.parse::<i64>() {
            return Ok(Val::Json(Json::Int(i)));
        }
        text.parse::<f64>().map(|f| Val::Json(Json::Float(f))).map_err(|_| self.unsupported())
    }

    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        let c = self.peek().ok_or_else(|| self.unsupported())?;
        self.pos += c.len_utf8();
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '0' => out.push('\0'),
            'u' => {
                let hex = self.src.get(self.pos..self.pos + 4).ok_or_else(|| self.unsupported())?;
                let code = u32::from_str_radix(hex, 16).map_err(|_| self.unsupported())?;
                self.pos += 4;
                out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
            }
            other => out.push(other),
        }
        Ok(())
    }

    fn string(&mut self) -> Result<String, String> {
        let quote = self.peek().ok_or_else(|| self.unsupported())?;
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or_else(|| self.unsupported())?;
            self.pos += c.len_utf8();
            if c == quote {
                return Ok(out);
            }
            if c == '\\' {
                self.escape(&mut out)?;
            } else {
                out.push(c);
            }
        }
    }

    fn template(&mut self, depth: usize) -> Result<Val, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or_else(|| self.unsupported())?;
            self.pos += c.len_utf8();
            match c {
                '`' => return Ok(Val::Json(Json::Str(out))),
                '\\' => self.escape(&mut out)?,
                '$' if self.peek() == Some('{') => {
                    self.pos += 1;
                    let value = self.expr(depth + 1)?;
                    if !self.eat("}") {
                        return Err(self.unsupported());
                    }
                    out.push_str(&value.to_js_string());
                }
                other => out.push(other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "MODEL" => Some("deepseek/v4".into()),
            "TOKEN" => Some("s3cret".into()),
            _ => None,
        }
    }

    #[test]
    fn evaluates_typical_patch_expressions() {
        assert_eq!(eval("process.env.MODEL", &env).unwrap(), Json::Str("deepseek/v4".into()));
        assert_eq!(eval("`Bearer ${process.env.TOKEN}`", &env).unwrap(), Json::Str("Bearer s3cret".into()));
        assert_eq!(
            eval("[{ id: process.env.MODEL }]", &env).unwrap().to_string(),
            r#"[{"id":"deepseek/v4"}]"#
        );
        assert_eq!(eval("process.env.MISSING || 'fallback'", &env).unwrap(), Json::Str("fallback".into()));
        assert_eq!(eval("process.env['TOKEN'] ?? 'x'", &env).unwrap(), Json::Str("s3cret".into()));
        assert_eq!(eval("process.env.MISSING", &env).unwrap(), Json::Null);
    }

    #[test]
    fn rejects_code_execution() {
        for source in [
            "require('child_process').execSync('id')",
            "process.exit(1)",
            "eval('1')",
            "process.env.A = 'x'",
            "process.mainModule",
            "process.env.TOKEN.toString()",
            "globalThis",
            "(() => 1)()",
            "`${require('fs')}`",
        ] {
            assert!(eval(source, &env).is_err(), "{source} should be rejected");
        }
    }
}
