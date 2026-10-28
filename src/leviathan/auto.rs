//! Index the workspace's data before the first model call.
//!
//! When the workspace holds large data files (CSV/TSV, JSON Lines, JSON), the
//! harness indexes them itself at the start of the first top-level turn:
//! no model turn, no tokens, and recall works on the first message. Leviathan
//! infers the mapping. An index holds one record shape, so the largest set of
//! files that share a shape (CSV header or JSON keys) is indexed; the briefing
//! lists the rest, which the agent can index with `leviathan_index`.
//!
//! Only an index Melchior manages is written: the implicit per-workspace
//! index, or a configured index file that does not exist yet. A configured
//! index that already exists is left alone. Rebuilds are cheap: Leviathan
//! skips a build when the sources did not change.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Leviathan, OwnedBackend};
use crate::runtime::Runtime;
use crate::{log_info, log_warn, util};

const DATA_EXTENSIONS: &[&str] = &["csv", "tsv", "jsonl", "ndjson", "json"];
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".melchior", ".leviathan"];
const MAX_DEPTH: usize = 4;
const MAX_FILES: usize = 2_000;

#[derive(Debug, Clone, PartialEq)]
pub struct DataFile {
    pub path: PathBuf,
    pub bytes: u64,
    /// Files with the same shape can share one index.
    pub shape: String,
}

/// What a session's automatic indexing did, for the briefing.
#[derive(Default, Clone)]
pub struct Outcome {
    pub indexed: Vec<DataFile>,
    pub skipped: Vec<DataFile>,
    pub seconds: f64,
    pub error: Option<String>,
}

/// The shape of a data file: its CSV header, or the sorted keys of its first
/// JSON object.
fn shape(path: &Path, extension: &str) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).take_line(&mut first)?;
    let first = first.trim_start_matches('\u{feff}').trim();
    match extension {
        "csv" | "tsv" => (!first.is_empty()).then(|| format!("{extension}:{first}")),
        _ => {
            // JSON Lines: keys of the first record. A JSON file: its first
            // line is usually `[` or `{`; group JSON files by extension only.
            match crate::json::parse(first) {
                Ok(crate::json::Json::Obj(fields)) => {
                    let mut keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
                    keys.sort_unstable();
                    Some(format!("jsonl:{}", keys.join(",")))
                }
                _ if extension == "json" => Some("json".into()),
                _ => None,
            }
        }
    }
}

trait TakeLine {
    fn take_line(self, out: &mut String) -> Option<()>;
}

impl<R: std::io::Read> TakeLine for BufReader<R> {
    /// Read the first line, at most 1 MB of it.
    fn take_line(self, out: &mut String) -> Option<()> {
        let mut limited = BufReader::new(std::io::Read::take(self, 1 << 20));
        limited.read_line(out).ok().map(|_| ())
    }
}

/// Data files under `root` of at least `min_bytes`, largest first.
pub fn scan(root: &Path, min_bytes: u64, exclude: &[PathBuf]) -> Vec<DataFile> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_FILES {
                break;
            }
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if meta.is_dir() {
                if depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') && !exclude.iter().any(|e| path.starts_with(e)) {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let extension = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if !meta.is_file() || meta.len() < min_bytes || !DATA_EXTENSIONS.contains(&extension.as_str()) {
                continue;
            }
            if let Some(shape) = shape(&path, &extension) {
                found.push(DataFile { path, bytes: meta.len(), shape });
            }
        }
    }
    found.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    found
}

/// The largest same-shaped set of files, and the rest.
pub fn choose(files: Vec<DataFile>) -> (Vec<DataFile>, Vec<DataFile>) {
    let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
    for f in &files {
        *totals.entry(f.shape.as_str()).or_default() += f.bytes;
    }
    let Some(best) = totals.iter().max_by_key(|(shape, total)| (**total, std::cmp::Reverse(*shape))).map(|(s, _)| s.to_string()) else {
        return (Vec::new(), files);
    };
    files.into_iter().partition(|f| f.shape == best)
}

pub struct AutoIndex {
    pub leviathan: Arc<Leviathan>,
    pub backend: Arc<OwnedBackend>,
    pub bin: PathBuf,
    pub index: PathBuf,
    pub min_bytes: u64,
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
        min_bytes: u64,
        timeout: Duration,
        managed: bool,
        exclude: Vec<PathBuf>,
    ) -> AutoIndex {
        AutoIndex { leviathan, backend, bin, index, min_bytes, timeout, managed, exclude, attempted: AtomicBool::new(false), outcome: Mutex::new(None) }
    }

    /// Index the workspace once per process, before the first model call.
    pub fn run_once(&self, runtime: &Runtime) {
        if self.attempted.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.managed && self.index.exists() {
            return;
        }
        let (chosen, skipped) = choose(scan(&runtime.cwd, self.min_bytes, &self.exclude));
        if chosen.is_empty() {
            return;
        }
        if let Some(dir) = self.index.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let started = Instant::now();
        let mut command = Command::new(&self.bin);
        command.arg("index");
        for f in &chosen {
            command.arg(&f.path);
        }
        command
            .arg("--index")
            .arg(&self.index)
            .arg("--quiet")
            .env_clear()
            .envs(runtime.child_env())
            .current_dir(&runtime.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut outcome = Outcome { indexed: chosen.clone(), skipped, ..Outcome::default() };
        match super::tools::run(command, self.timeout) {
            Ok((true, _)) => {
                outcome.seconds = started.elapsed().as_secs_f64();
                self.leviathan.invalidate();
                if let Err(e) = self.backend.connect() {
                    log_warn!("leviathan", "indexed, but Leviathan did not restart: {e}");
                }
                let mb: u64 = chosen.iter().map(|f| f.bytes).sum::<u64>() / (1 << 20);
                log_info!("leviathan", "auto-indexed {} file(s), {mb} MB, in {:.1}s", chosen.len(), outcome.seconds);
            }
            Ok((false, text)) | Err(text) => {
                let text = util::truncate(text.trim(), 300, "…").to_string();
                log_warn!("leviathan", "auto-index failed: {text}");
                outcome.indexed.clear();
                outcome.error = Some(text);
            }
        }
        *self.outcome.lock().unwrap() = Some(outcome);
    }

    /// One line for the briefing, if anything happened.
    pub fn note(&self, cwd: &Path) -> Option<String> {
        let outcome = self.outcome.lock().unwrap().clone()?;
        let rel = |f: &DataFile| f.path.strip_prefix(cwd).unwrap_or(&f.path).display().to_string();
        let mut out = String::new();
        if !outcome.indexed.is_empty() {
            let names: Vec<String> = outcome.indexed.iter().take(8).map(rel).collect();
            out.push_str(&format!(
                "The harness indexed {} for you before this conversation ({:.0}s, mapping inferred by Leviathan). \
                 Re-index with leviathan_index to set fields such as group.",
                names.join(", "),
                outcome.seconds
            ));
        }
        if !outcome.skipped.is_empty() {
            let names: Vec<String> = outcome.skipped.iter().take(8).map(rel).collect();
            out.push_str(&format!(" Not indexed (a different record shape): {}.", names.join(", ")));
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

    #[test]
    fn scans_large_data_files_and_groups_them_by_shape() {
        let root = dir("scan");
        fs::create_dir_all(root.join("exports/2026")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        let rows = |n: usize| (0..n).map(|i| format!("{{\"id\":\"r{i}\",\"text\":\"row {i}\"}}\n")).collect::<String>();
        fs::write(root.join("big.jsonl"), rows(400)).unwrap();
        fs::write(root.join("exports/2026/more.jsonl"), rows(300)).unwrap();
        fs::write(root.join("tickets.csv"), format!("id,subject\n{}", "T-1,login loop\n".repeat(500))).unwrap();
        fs::write(root.join("tiny.csv"), "id,x\n1,2\n").unwrap();
        fs::write(root.join("node_modules/huge.json"), rows(2000)).unwrap();
        fs::write(root.join("notes.txt"), "x".repeat(50_000)).unwrap();

        let found = scan(&root, 1_000, &[]);
        let names: Vec<String> = found.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert_eq!(names[0], "big.jsonl", "largest first");
        assert!(names.contains(&"more.jsonl".into()) && names.contains(&"tickets.csv".into()));
        assert!(!names.contains(&"tiny.csv".into()) && !names.contains(&"huge.json".into()) && !names.contains(&"notes.txt".into()));

        let (chosen, rest) = choose(found);
        let chosen: Vec<String> = chosen.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(chosen, vec!["big.jsonl".to_string(), "more.jsonl".to_string()], "the two JSON Lines files share a shape and outweigh the CSV");
        assert_eq!(rest.len(), 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn nothing_to_choose_from_an_empty_workspace() {
        let (chosen, rest) = choose(Vec::new());
        assert!(chosen.is_empty() && rest.is_empty());
    }
}
