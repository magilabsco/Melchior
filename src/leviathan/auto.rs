//! Index the workspace's data before the first model call.
//!
//! At the start of the first top-level turn the harness indexes every data
//! file in the workspace itself: CSV and TSV, JSON Lines, gzipped versions of
//! either, JSON that holds records, and every table of SQLite databases. No
//! model turn, no tokens, and recall works on the first message. Leviathan
//! sorts the files into kinds of record (tickets, customers, orders) and
//! infers a mapping for each, so one index covers all of them.
//!
//! Only an index Melchior manages is written: the implicit per-workspace
//! index, or a configured index file that does not exist yet. A configured
//! index that already exists is left alone. Rebuilds are cheap: Leviathan
//! skips a build when the sources did not change.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Leviathan, OwnedBackend};
use crate::json::{self, Json};
use crate::runtime::Runtime;
use crate::{log_info, log_warn, util};

/// Line-oriented data: always indexed, whatever the size.
const LINE_FORMATS: &[&str] = &["csv", "tsv", "tab", "jsonl", "ndjson"];
const SQLITE_FORMATS: &[&str] = &["sqlite", "sqlite3", "db"];
/// JSON files that configure tools rather than hold data.
const CONFIG_JSON: &[&str] = &[
    "package.json",
    "package-lock.json",
    "composer.json",
    "composer.lock",
    "deno.json",
    "tsconfig.json",
    "jsconfig.json",
    "manifest.json",
    "settings.json",
    "launch.json",
    "tasks.json",
    "extensions.json",
    "renovate.json",
    "appsettings.json",
    "firebase.json",
    "vercel.json",
    "turbo.json",
    "lerna.json",
    "nx.json",
];
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".melchior", ".leviathan", "dist", "build"];
const MAX_DEPTH: usize = 6;
/// Directory entries looked at, and data files indexed, at most.
const MAX_ENTRIES: usize = 20_000;
const MAX_FILES: usize = 1_000;

#[derive(Debug, Clone, PartialEq)]
pub struct DataFile {
    pub path: PathBuf,
    pub bytes: u64,
}

/// What a session's automatic indexing did, for the briefing.
#[derive(Default, Clone)]
pub struct Outcome {
    pub indexed: Vec<DataFile>,
    /// Data files beyond the limit, not indexed.
    pub left_out: usize,
    pub records: i64,
    /// Kinds of record and their counts, when Leviathan found several.
    pub kinds: Vec<(String, i64)>,
    pub seconds: f64,
    pub error: Option<String>,
}

fn is_sqlite(path: &Path) -> bool {
    let mut head = [0u8; 16];
    fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"SQLite format 3\0"
}

/// A small JSON file holds records when it is an array, or wraps an array
/// of objects (`{"data": {"items": [...]}}`); a config file does neither.
fn holds_records(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else { return false };
    fn wraps_records(value: &Json, depth: usize) -> bool {
        match value {
            Json::Arr(items) => items.iter().filter(|v| matches!(v, Json::Obj(_))).count() >= 2,
            Json::Obj(fields) if depth < 3 => fields.iter().any(|(_, v)| wraps_records(v, depth + 1)),
            _ => false,
        }
    }
    match json::parse(text.trim_start_matches('\u{feff}')) {
        Ok(Json::Arr(items)) => items.iter().any(|v| matches!(v, Json::Obj(_) | Json::Arr(_))),
        Ok(doc @ Json::Obj(_)) => wraps_records(&doc, 0),
        _ => false,
    }
}

/// Is this file data Leviathan should index? `json_min_bytes`: JSON files at
/// least this large are data without looking inside.
fn is_data(path: &Path, bytes: u64, json_min_bytes: u64) -> bool {
    if bytes == 0 {
        return false;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let (base, gz) = match name.strip_suffix(".gz") {
        Some(base) => (base, true),
        None => (name.as_str(), false),
    };
    let ext = base.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if LINE_FORMATS.contains(&ext) {
        return true;
    }
    if SQLITE_FORMATS.contains(&ext) {
        return !gz && is_sqlite(path);
    }
    if ext == "json" {
        if CONFIG_JSON.contains(&base) || base.ends_with("-lock.json") || base.ends_with(".lock.json") || base.starts_with("tsconfig.") {
            return false;
        }
        return gz || bytes >= json_min_bytes || holds_records(path);
    }
    false
}

/// Data files under `root`, largest first. Symlinks are not followed and
/// every file must resolve inside `root`, as for the file tools.
pub fn scan(root: &Path, json_min_bytes: u64, exclude: &[PathBuf]) -> Vec<DataFile> {
    let Ok(canonical_root) = root.canonicalize() else { return Vec::new() };
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                break;
            }
            let path = entry.path();
            if entry.file_type().map_or(true, |t| t.is_symlink()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || exclude.iter().any(|e| path.starts_with(e)) {
                continue;
            }
            if meta.is_dir() {
                if depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let inside = path.canonicalize().is_ok_and(|p| p.starts_with(&canonical_root));
            if meta.is_file() && inside && is_data(&path, meta.len(), json_min_bytes) {
                found.push(DataFile { path, bytes: meta.len() });
            }
        }
    }
    found.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    found
}

pub struct AutoIndex {
    pub leviathan: Arc<Leviathan>,
    pub backend: Arc<OwnedBackend>,
    pub bin: PathBuf,
    pub index: PathBuf,
    /// JSON files this large are data without looking inside.
    pub json_min_bytes: u64,
    pub timeout: Duration,
    /// The index may be (re)built: Melchior manages it.
    pub managed: bool,
    pub exclude: Vec<PathBuf>,
    attempted: AtomicBool,
    pub outcome: Mutex<Option<Outcome>>,
}

impl AutoIndex {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        leviathan: Arc<Leviathan>,
        backend: Arc<OwnedBackend>,
        bin: PathBuf,
        index: PathBuf,
        json_min_bytes: u64,
        timeout: Duration,
        managed: bool,
        exclude: Vec<PathBuf>,
    ) -> AutoIndex {
        AutoIndex {
            leviathan,
            backend,
            bin,
            index,
            json_min_bytes,
            timeout,
            managed,
            exclude,
            attempted: AtomicBool::new(false),
            outcome: Mutex::new(None),
        }
    }

    /// Index the workspace once per process, before the first model call.
    pub fn run_once(&self, runtime: &Runtime) {
        if self.attempted.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.managed && self.index.exists() {
            return;
        }
        let mut exclude = self.exclude.clone();
        exclude.push(self.index.clone());
        let mut files = scan(&runtime.cwd, self.json_min_bytes, &exclude);
        if files.is_empty() {
            return;
        }
        let left_out = files.len().saturating_sub(MAX_FILES);
        files.truncate(MAX_FILES);
        if let Some(dir) = self.index.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let started = Instant::now();
        let mut result = self.build(runtime, &files);
        // Leviathan before 0.3 reads one table per SQLite file and fails on
        // the rest: index everything else rather than nothing.
        if let Ok((false, text)) | Err(text) = &result
            && text.contains("tables (")
            && files.iter().any(|f| is_sqlite(&f.path))
        {
            log_warn!("leviathan", "this Leviathan cannot index every SQLite table; indexing the other files");
            files.retain(|f| !is_sqlite(&f.path));
            result = if files.is_empty() { Err(text.clone()) } else { self.build(runtime, &files) };
        }
        let mut outcome = Outcome { indexed: files.clone(), left_out, ..Outcome::default() };
        match result {
            Ok((true, text)) => {
                outcome.seconds = started.elapsed().as_secs_f64();
                let report = text.lines().rev().find_map(|l| json::parse(l.trim()).ok()).unwrap_or(Json::Null);
                outcome.records = report.get("record_count").as_i64().unwrap_or(0);
                outcome.kinds = report
                    .get("kinds")
                    .items()
                    .iter()
                    .map(|k| (k.get("name").str_or("?").to_string(), k.get("records").as_i64().unwrap_or(0)))
                    .collect();
                self.leviathan.invalidate();
                if let Err(e) = self.backend.connect() {
                    log_warn!("leviathan", "indexed, but Leviathan did not restart: {e}");
                }
                let mb: u64 = files.iter().map(|f| f.bytes).sum::<u64>() / (1 << 20);
                log_info!(
                    "leviathan",
                    "auto-indexed {} file(s), {mb} MB, {} records in {} kind(s), in {:.1}s",
                    files.len(),
                    outcome.records,
                    outcome.kinds.len().max(1),
                    outcome.seconds
                );
            }
            Ok((false, text)) | Err(text) => {
                outcome.indexed.clear();
                // Files that looked like data but held none are not a failure.
                if !(text.contains("no data files found") || text.contains("no readable records")) {
                    let text = util::truncate(text.trim(), 300, "…").to_string();
                    log_warn!("leviathan", "auto-index failed: {text}");
                    outcome.error = Some(text);
                }
            }
        }
        *self.outcome.lock().unwrap() = Some(outcome);
    }

    fn build(&self, runtime: &Runtime, files: &[DataFile]) -> Result<(bool, String), String> {
        let mut command = Command::new(&self.bin);
        // Options first, then `--`: a file name is never read as a flag.
        command.arg("--json").arg("index").arg("--index").arg(&self.index).arg("--quiet").arg("--");
        for f in files {
            command.arg(&f.path);
        }
        command
            .env_clear()
            .envs(runtime.child_env())
            .current_dir(&runtime.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::tools::run(command, self.timeout)
    }

    /// One line for the briefing, if anything happened.
    pub fn note(&self, cwd: &Path) -> Option<String> {
        let outcome = self.outcome.lock().unwrap().clone()?;
        let rel = |f: &DataFile| f.path.strip_prefix(cwd).unwrap_or(&f.path).display().to_string();
        let mut out = String::new();
        if !outcome.indexed.is_empty() {
            let mut names: Vec<String> = outcome.indexed.iter().take(8).map(rel).collect();
            if outcome.indexed.len() > names.len() {
                names.push(format!("{} more", outcome.indexed.len() - names.len()));
            }
            out.push_str(&format!(
                "The harness indexed the workspace's data before this conversation ({:.0}s, mappings inferred by Leviathan): {}.",
                outcome.seconds,
                names.join(", ")
            ));
            if !outcome.kinds.is_empty() {
                let kinds: Vec<String> = outcome.kinds.iter().map(|(k, n)| format!("{k} ({n})")).collect();
                out.push_str(&format!(" It holds {} kinds of record: {}.", kinds.len(), kinds.join(", ")));
            }
            out.push_str(" Re-index with leviathan_index to set fields such as group.");
        }
        if outcome.left_out > 0 {
            out.push_str(&format!(" {} smaller data files were left out (limit {MAX_FILES}).", outcome.left_out));
        }
        if let Some(error) = outcome.error {
            out.push_str(&format!(" Automatic indexing failed: {error}"));
        }
        (!out.is_empty()).then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("melchior-auto-{name}-{}", util::random_hex(6)));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(found: &[DataFile]) -> Vec<String> {
        let mut out: Vec<String> = found.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        out.sort();
        out
    }

    #[test]
    fn scans_every_data_file_of_any_size_but_not_config_or_tooling() {
        let root = dir("scan");
        fs::create_dir_all(root.join("exports/2026")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        let rows = |n: usize| (0..n).map(|i| format!("{{\"id\":\"r{i}\",\"text\":\"row {i}\"}}\n")).collect::<String>();
        fs::write(root.join("big.jsonl"), rows(400)).unwrap();
        fs::write(root.join("exports/2026/more.ndjson"), rows(3)).unwrap();
        fs::write(root.join("tiny.csv"), "id,x\n1,2\n").unwrap();
        fs::write(root.join("semi.tsv"), "id\tx\n1\t2\n").unwrap();
        fs::write(root.join("logs.jsonl.gz"), [0x1f, 0x8b, 8, 0]).unwrap();
        fs::write(root.join("customers.json"), r#"{"data": {"customers": [{"id": 1}, {"id": 2}]}}"#).unwrap();
        fs::write(root.join("rows.json"), r#"[{"id": 1}]"#).unwrap();
        fs::write(root.join("package.json"), r#"{"name": "x", "files": ["a", "b"]}"#).unwrap();
        fs::write(root.join("config.json"), r#"{"retries": 3, "hosts": {"a": {"port": 1}, "b": {"port": 2}}}"#).unwrap();
        fs::write(root.join("app.sqlite"), b"SQLite format 3\0rest-of-a-database").unwrap();
        fs::write(root.join("fake.db"), "not a database").unwrap();
        fs::write(root.join("empty.csv"), "").unwrap();
        fs::write(root.join("notes.txt"), "x".repeat(5_000)).unwrap();
        fs::write(root.join("node_modules/pkg/data.json"), rows(2000)).unwrap();
        fs::write(root.join(".git/index.csv"), "a,b\n").unwrap();

        let found = scan(&root, 1 << 20, &[]);
        assert_eq!(
            names(&found),
            ["app.sqlite", "big.jsonl", "customers.json", "logs.jsonl.gz", "more.ndjson", "rows.json", "semi.tsv", "tiny.csv"]
        );
        assert_eq!(found[0].path.file_name().unwrap(), "big.jsonl", "largest first");
        let excluded = scan(&root, 1 << 20, &[root.join("exports")]);
        assert!(!names(&excluded).contains(&"more.ndjson".into()), "excluded paths are skipped");
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_workspace_are_not_followed() {
        let outside = dir("outside");
        fs::write(outside.join("secrets.csv"), "user,password\na,b\n").unwrap();
        fs::create_dir_all(outside.join("more")).unwrap();
        fs::write(outside.join("more/rows.jsonl"), "{\"id\":1}\n").unwrap();
        let root = dir("links");
        fs::write(root.join("mine.csv"), "id\n1\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secrets.csv"), root.join("linked.csv")).unwrap();
        std::os::unix::fs::symlink(outside.join("more"), root.join("linked_dir")).unwrap();
        assert_eq!(names(&scan(&root, 1 << 20, &[])), ["mine.csv"]);
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(outside).ok();
    }

    #[test]
    fn large_json_is_data_without_looking_inside() {
        let root = dir("json");
        fs::write(root.join("export.json"), format!("{{\"x\": \"{}\"}}", "y".repeat(2_000))).unwrap();
        assert!(scan(&root, 1_000, &[]).len() == 1);
        assert!(scan(&root, 1 << 20, &[]).is_empty(), "a small object without records is not data");
        fs::remove_dir_all(root).ok();
    }
}
