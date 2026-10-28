//! File tools: `read`, `write`, `edit` (plugin ids `tool-fs`,
//! `tool-str-replace-editor`) and `glob`, `grep` (`tool-fs-search`).
//!
//! Without full access every path must resolve, after following symlinks,
//! inside the workspace; anything else fails with "sandbox: file access denied".

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Tool, ToolCx, ToolOutput, ToolSpec, opt_i64, opt_str, prop, schema, str_arg, try_out};
use crate::jobj;
use crate::json::Json;
use crate::util;

const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve `raw` and enforce the workspace boundary unless full access is on.
pub fn checked_path(cx: &ToolCx<'_>, raw: &str) -> Result<PathBuf, ToolOutput> {
    if raw.trim().is_empty() {
        return Err(ToolOutput::err("file_path must not be empty"));
    }
    let path = normalize(&cx.runtime.resolve_path(raw));
    if cx.runtime.full_access() {
        return Ok(path);
    }
    let root = fs::canonicalize(&cx.runtime.cwd).unwrap_or_else(|_| cx.runtime.cwd.clone());
    // Canonicalize the deepest existing ancestor so symlinks cannot escape.
    let mut existing = path.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => break,
        }
        if !existing.pop() {
            break;
        }
    }
    let mut resolved = fs::canonicalize(&existing).unwrap_or(existing);
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(ToolOutput::err(format!("sandbox: file access denied: {raw} is outside the workspace")))
    }
}

pub struct Read;
impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a text file. Returns numbered lines. Use offset/limit (1-based line numbers) for large files.".into(),
            parameters: schema(
                jobj! {
                    "file_path" => prop("string", "Absolute path, or relative to the workspace."),
                    "offset" => prop("integer", "First line to return (1-based)."),
                    "limit" => prop("integer", "Maximum number of lines (default 2000)."),
                },
                &["file_path"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let raw = try_out!(str_arg(args, "file_path"));
        let path = try_out!(checked_path(cx, raw));
        let meta = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) => return ToolOutput::err(format!("cannot read {raw}: {e}")),
        };
        if meta.is_dir() {
            return ToolOutput::err(format!("{raw} is a directory; use glob to list files"));
        }
        if meta.len() > MAX_READ_BYTES {
            return ToolOutput::err(format!("{raw} is {} bytes; too large to read whole (limit {MAX_READ_BYTES})", meta.len()));
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => return ToolOutput::err(format!("cannot read {raw}: {e}")),
        };
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return ToolOutput::ok(format!("{raw} is a binary file ({} bytes); it cannot be shown as text.", bytes.len()));
        }
        let text = String::from_utf8_lossy(&bytes);
        let offset = opt_i64(args, "offset").unwrap_or(1).max(1) as usize;
        let limit = opt_i64(args, "limit").unwrap_or(2000).clamp(1, 20_000) as usize;
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return ToolOutput::ok(format!("{raw} is empty."));
        }
        let mut out = String::new();
        for (index, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
            out.push_str(&format!("{:>6}\t{}\n", index + 1, util::truncate(line, 2000, "…")));
        }
        let shown_end = (offset - 1 + limit).min(lines.len());
        if shown_end < lines.len() {
            out.push_str(&format!("… {} more lines (use offset={} to continue)\n", lines.len() - shown_end, shown_end + 1));
        }
        ToolOutput::ok(out)
    }
}

pub struct Write;
impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Create or overwrite a text file with the given content. Parent directories are created.".into(),
            parameters: schema(
                jobj! {
                    "file_path" => prop("string", "Absolute path, or relative to the workspace."),
                    "content" => prop("string", "The complete file content."),
                },
                &["file_path", "content"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let raw = try_out!(str_arg(args, "file_path"));
        let content = try_out!(str_arg(args, "content"));
        let path = try_out!(checked_path(cx, raw));
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent) {
                return ToolOutput::err(format!("cannot create {}: {e}", parent.display()));
            }
        match fs::write(&path, content) {
            Ok(()) => ToolOutput::ok(format!("Wrote {} bytes to {raw}.", content.len())),
            Err(e) => ToolOutput::err(format!("cannot write {raw}: {e}")),
        }
    }
}

pub struct Edit;
impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Replace an exact string in a file. old_string must match exactly once unless replace_all is true.".into(),
            parameters: schema(
                jobj! {
                    "file_path" => prop("string", "Absolute path, or relative to the workspace."),
                    "old_string" => prop("string", "Exact text to replace (include enough context to be unique)."),
                    "new_string" => prop("string", "Replacement text."),
                    "replace_all" => prop("boolean", "Replace every occurrence."),
                },
                &["file_path", "old_string", "new_string"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let raw = try_out!(str_arg(args, "file_path"));
        let old = try_out!(str_arg(args, "old_string"));
        let new = try_out!(str_arg(args, "new_string"));
        let all = args.get("replace_all").as_bool().unwrap_or(false);
        if old.is_empty() {
            return ToolOutput::err("old_string must not be empty; use write to create a file");
        }
        let path = try_out!(checked_path(cx, raw));
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => return ToolOutput::err(format!("cannot read {raw}: {e}")),
        };
        let count = text.matches(old).count();
        if count == 0 {
            return ToolOutput::err(format!("old_string was not found in {raw}"));
        }
        if count > 1 && !all {
            return ToolOutput::err(format!("old_string occurs {count} times in {raw}; add context or set replace_all"));
        }
        let updated = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        match fs::write(&path, updated) {
            Ok(()) => ToolOutput::ok(format!("Replaced {} occurrence(s) in {raw}.", if all { count } else { 1 })),
            Err(e) => ToolOutput::err(format!("cannot write {raw}: {e}")),
        }
    }
}

// ---- glob ------------------------------------------------------------------

/// Match a `/`-separated relative path against a glob with `**`, `*`, `?`,
/// `[...]`, and `{a,b}`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    if let Some(start) = pattern.find('{')
        && let Some(len) = pattern[start..].find('}') {
            let end = start + len;
            return pattern[start + 1..end]
                .split(',')
                .any(|alt| glob_match(&format!("{}{alt}{}", &pattern[..start], &pattern[end + 1..]), path));
        }
    let pattern: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(&"**") => (0..=path.len()).any(|skip| match_segments(&pattern[1..], &path[skip..])),
        Some(segment) => !path.is_empty() && match_segment(segment.as_bytes(), path[0].as_bytes()) && match_segments(&pattern[1..], &path[1..]),
    }
}

fn match_segment(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.first() {
        None => name.is_empty(),
        Some(b'*') => (0..=name.len()).any(|skip| match_segment(&pattern[1..], &name[skip..])),
        Some(b'?') => !name.is_empty() && match_segment(&pattern[1..], &name[1..]),
        Some(b'[') => {
            let Some(close) = pattern.iter().position(|b| *b == b']') else {
                return !name.is_empty() && name[0] == b'[' && match_segment(&pattern[1..], &name[1..]);
            };
            if name.is_empty() {
                return false;
            }
            let class = &pattern[1..close];
            let (negate, class) = match class.first() {
                Some(b'!' | b'^') => (true, &class[1..]),
                _ => (false, class),
            };
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == b'-' {
                    hit |= class[i] <= name[0] && name[0] <= class[i + 2];
                    i += 3;
                } else {
                    hit |= class[i] == name[0];
                    i += 1;
                }
            }
            hit != negate && match_segment(&pattern[close + 1..], &name[1..])
        }
        Some(c) => !name.is_empty() && *c == name[0] && match_segment(&pattern[1..], &name[1..]),
    }
}

const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, limit: usize, visited: &mut usize) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        *visited += 1;
        if out.len() >= limit || *visited > 200_000 {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk(root, &path, out, limit, visited);
            }
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().into_owned());
        }
    }
}

pub struct Glob;
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files by glob pattern (e.g. **/*.csv). Returns paths relative to the search directory.".into(),
            parameters: schema(
                jobj! {
                    "pattern" => prop("string", "Glob pattern; supports **, *, ?, [abc], {a,b}."),
                    "path" => prop("string", "Directory to search (default: the workspace)."),
                },
                &["pattern"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let pattern = try_out!(str_arg(args, "pattern"));
        let base = try_out!(checked_path(cx, opt_str(args, "path").unwrap_or(".")));
        if !base.is_dir() {
            return ToolOutput::err(format!("{} is not a directory", base.display()));
        }
        let mut files = Vec::new();
        let mut visited = 0;
        walk(&base, &base, &mut files, 100_000, &mut visited);
        let matches: Vec<String> = files.into_iter().filter(|f| glob_match(pattern, f)).take(500).collect();
        if matches.is_empty() {
            ToolOutput::ok(format!("No files match {pattern:?} under {}.", base.display()))
        } else {
            ToolOutput::ok(matches.join("\n"))
        }
    }
}

pub struct Grep;
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regular expression (ripgrep syntax). Returns file:line:text matches.".into(),
            parameters: schema(
                jobj! {
                    "pattern" => prop("string", "Regular expression."),
                    "path" => prop("string", "File or directory to search (default: the workspace)."),
                    "glob" => prop("string", "Only search files matching this glob, e.g. *.py."),
                    "case_insensitive" => prop("boolean", "Ignore case."),
                },
                &["pattern"],
            ),
        }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let pattern = try_out!(str_arg(args, "pattern"));
        let target = try_out!(checked_path(cx, opt_str(args, "path").unwrap_or(".")));
        let insensitive = args.get("case_insensitive").as_bool().unwrap_or(false);
        let rg = ["/usr/bin/rg", "/usr/local/bin/rg"].into_iter().find(|p| Path::new(p).exists());
        let mut command = match rg {
            Some(rg) => {
                let mut c = Command::new(rg);
                c.args(["--no-heading", "--line-number", "--color", "never", "--max-columns", "400", "--max-count", "50"]);
                if insensitive {
                    c.arg("-i");
                }
                if let Some(glob) = opt_str(args, "glob") {
                    c.args(["--glob", glob]);
                }
                c.arg("-e").arg(pattern).arg(&target);
                c
            }
            None => {
                let mut c = Command::new("grep");
                c.args(["-rnE", "--binary-files=without-match"]);
                if insensitive {
                    c.arg("-i");
                }
                if let Some(glob) = opt_str(args, "glob") {
                    c.arg(format!("--include={glob}"));
                }
                c.arg("-e").arg(pattern).arg(&target);
                c
            }
        };
        command.env_clear().envs(cx.runtime.child_env()).stdin(Stdio::null());
        match command.output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                match output.status.code() {
                    Some(0) => ToolOutput::ok(util::truncate(&stdout, 30_000, "\n…[more matches truncated]")),
                    Some(1) => ToolOutput::ok(format!("No matches for {pattern:?}.")),
                    _ => ToolOutput::err(format!("grep failed: {}", String::from_utf8_lossy(&output.stderr).trim())),
                }
            }
            Err(e) => ToolOutput::err(format!("cannot run search: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_patterns() {
        assert!(glob_match("**/*.csv", "data/2024/sales.csv"));
        assert!(glob_match("**/*.csv", "sales.csv"));
        assert!(glob_match("src/*.{rs,toml}", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/a/main.rs"));
        assert!(glob_match("file[0-9].txt", "file7.txt"));
        assert!(!glob_match("file[!0-9].txt", "file7.txt"));
    }

    #[test]
    fn normalizes_parent_components() {
        assert_eq!(normalize(Path::new("/w/a/../b/./c")), PathBuf::from("/w/b/c"));
        assert_eq!(normalize(Path::new("/w/../../etc")), PathBuf::from("/etc"));
    }
}
