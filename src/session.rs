//! Durable sessions: one append-only JSONL log per session.
//!
//! Layout: `<home>/melchior/sessions/<session-id>/events.jsonl` (the
//! directory is the distribution's `sessions_dir`), plus an
//! `owner.lock` file held with an exclusive OS lock by the one harness
//! process that has the session open. The lock dies with the process, so a
//! crashed harness never strands a session.
//!
//! Every record is written and fsynced before the harness acts on it (a user
//! message is durable before the model is called), and committed bytes are
//! never rewritten. The only repair performed is dropping a torn final line
//! that was never terminated by a newline, i.e. a write interrupted by a crash.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::jobj;
use crate::json::{self, Json};
use crate::util;

pub const LOG_FILE: &str = "events.jsonl";
pub const LOCK_FILE: &str = "owner.lock";
pub const FORMAT_VERSION: i64 = 1;

#[derive(Debug, PartialEq)]
pub enum SessionError {
    Exists(String),
    NotFound(String),
    AlreadyOpen(String),
    Owned(String),
    Corrupt(String, String),
    Invalid(String),
    Io(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Exists(id) => write!(f, "session \"{id}\" already exists"),
            SessionError::NotFound(id) => write!(f, "session \"{id}\" not found"),
            SessionError::AlreadyOpen(id) => write!(f, "session \"{id}\" is already open or opening"),
            SessionError::Owned(id) => write!(f, "session \"{id}\" is already owned by another harness process"),
            SessionError::Corrupt(id, detail) => {
                write!(f, "session \"{id}\" history is corrupt ({detail}); refusing to resume it")
            }
            SessionError::Invalid(id) => write!(f, "invalid session id {id:?}"),
            SessionError::Io(detail) => write!(f, "session storage error: {detail}"),
        }
    }
}

pub struct Session {
    pub id: String,
    dir: Option<PathBuf>,
    log: Option<Mutex<File>>,
    _lock: Option<File>,
    records: Mutex<Vec<Json>>,
    seq: AtomicU64,
    pub busy: AtomicBool,
    pub cancel: AtomicBool,
}

impl Session {
    /// An in-memory session (used for subagents); nothing touches disk.
    pub fn ephemeral(id: &str) -> Arc<Session> {
        Arc::new(Session {
            id: id.to_string(),
            dir: None,
            log: None,
            _lock: None,
            records: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        })
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Durably append one record and return it.
    pub fn append(&self, kind: &str, data: Json) -> Result<Json, String> {
        let mut records = self.records.lock().unwrap();
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let record = jobj! {
            "v" => FORMAT_VERSION,
            "seq" => seq,
            "ts" => util::now_iso(),
            "type" => kind,
            "data" => data,
        };
        if let Some(log) = &self.log {
            let mut file = log.lock().unwrap();
            let mut line = record.to_string();
            line.push('\n');
            file.write_all(line.as_bytes()).map_err(|e| format!("cannot write session log: {e}"))?;
            file.sync_data().map_err(|e| format!("cannot sync session log: {e}"))?;
        }
        records.push(record.clone());
        Ok(record)
    }

    pub fn records(&self) -> Vec<Json> {
        self.records.lock().unwrap().clone()
    }

    pub fn record_count(&self) -> usize {
        self.records.lock().unwrap().len()
    }

    /// Latest value written with `append("state", {key, value})`.
    pub fn state(&self, key: &str) -> Json {
        self.records
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|r| r.get("type").as_str() == Some("state") && r.get("data").get("key").as_str() == Some(key))
            .map(|r| r.get("data").get("value").clone())
            .unwrap_or(Json::Null)
    }

    pub fn set_state(&self, key: &str, value: Json) -> Result<(), String> {
        self.append("state", jobj! { "key" => key, "value" => value }).map(|_| ())
    }
}

pub struct SessionStore {
    root: PathBuf,
    open: Mutex<HashMap<String, Arc<Session>>>,
    opening: Mutex<HashSet<String>>,
}

impl SessionStore {
    /// A store whose sessions live in `root` (one directory per session).
    pub fn new(root: PathBuf) -> SessionStore {
        SessionStore {
            root,
            open: Mutex::new(HashMap::new()),
            opening: Mutex::new(HashSet::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir_for(&self, id: &str) -> Result<PathBuf, SessionError> {
        if !util::valid_session_id(id) {
            return Err(SessionError::Invalid(id.to_string()));
        }
        Ok(self.root.join(id))
    }

    pub fn get_open(&self, id: &str) -> Option<Arc<Session>> {
        self.open.lock().unwrap().get(id).cloned()
    }

    pub fn exists(&self, id: &str) -> bool {
        self.dir_for(id).map(|dir| dir.exists()).unwrap_or(false)
    }

    pub fn open_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.open.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn stored_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = fs::read_dir(&self.root)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().join(LOG_FILE).is_file())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    fn begin_opening(&self, id: &str) -> Result<OpeningGuard<'_>, SessionError> {
        if self.open.lock().unwrap().contains_key(id) {
            return Err(SessionError::AlreadyOpen(id.to_string()));
        }
        let mut opening = self.opening.lock().unwrap();
        if !opening.insert(id.to_string()) {
            return Err(SessionError::AlreadyOpen(id.to_string()));
        }
        Ok(OpeningGuard { store: self, id: id.to_string() })
    }

    fn lock(&self, dir: &Path, id: &str) -> Result<File, SessionError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK_FILE))
            .map_err(|e| SessionError::Io(e.to_string()))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => Err(SessionError::Owned(id.to_string())),
            Err(fs::TryLockError::Error(e)) => Err(SessionError::Io(e.to_string())),
        }
    }

    /// Create a brand-new session. Fails if any session with that id exists.
    pub fn create(&self, id: &str) -> Result<Arc<Session>, SessionError> {
        let dir = self.dir_for(id)?;
        let _guard = self.begin_opening(id)?;
        fs::create_dir_all(&self.root).map_err(|e| SessionError::Io(e.to_string()))?;
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(SessionError::Exists(id.to_string())),
            Err(e) => return Err(SessionError::Io(e.to_string())),
        }
        let lock = self.lock(&dir, id)?;
        let log = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(dir.join(LOG_FILE))
            .map_err(|e| SessionError::Io(e.to_string()))?;
        let session = Arc::new(Session {
            id: id.to_string(),
            dir: Some(dir.clone()),
            log: Some(Mutex::new(log)),
            _lock: Some(lock),
            records: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        });
        session
            .append(
                "session/created",
                jobj! { "sessionId" => id, "harness" => concat!("melchior/", env!("CARGO_PKG_VERSION")) },
            )
            .map_err(SessionError::Io)?;
        if let Ok(handle) = File::open(&dir) {
            let _ = handle.sync_all();
        }
        self.open.lock().unwrap().insert(id.to_string(), session.clone());
        Ok(session)
    }

    /// Open a stored session for appending, after validating its whole history.
    pub fn resume(&self, id: &str) -> Result<Arc<Session>, SessionError> {
        let dir = self.dir_for(id)?;
        let _guard = self.begin_opening(id)?;
        if !dir.is_dir() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let lock = self.lock(&dir, id)?;
        let path = dir.join(LOG_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SessionError::Corrupt(id.to_string(), "history file is missing".into()));
            }
            Err(e) => return Err(SessionError::Io(e.to_string())),
        };
        let (records, committed_len) = parse_log(id, &bytes)?;
        if committed_len < bytes.len() {
            crate::log_warn!("session", "{id}: dropping {} bytes of a torn final record", bytes.len() - committed_len);
            let file = OpenOptions::new().write(true).open(&path).map_err(|e| SessionError::Io(e.to_string()))?;
            file.set_len(committed_len as u64).map_err(|e| SessionError::Io(e.to_string()))?;
            file.sync_all().map_err(|e| SessionError::Io(e.to_string()))?;
        }
        let log = OpenOptions::new().append(true).open(&path).map_err(|e| SessionError::Io(e.to_string()))?;
        let last_seq = records.last().and_then(|r| r.get("seq").as_i64()).unwrap_or(0) as u64;
        let session = Arc::new(Session {
            id: id.to_string(),
            dir: Some(dir),
            log: Some(Mutex::new(log)),
            _lock: Some(lock),
            records: Mutex::new(records),
            seq: AtomicU64::new(last_seq),
            busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        });
        self.open.lock().unwrap().insert(id.to_string(), session.clone());
        Ok(session)
    }

    pub fn close(&self, id: &str) -> bool {
        self.open.lock().unwrap().remove(id).is_some()
    }
}

struct OpeningGuard<'a> {
    store: &'a SessionStore,
    id: String,
}

impl Drop for OpeningGuard<'_> {
    fn drop(&mut self) {
        self.store.opening.lock().unwrap().remove(&self.id);
    }
}

/// Validate a log. Returns the records and the byte length of the committed
/// prefix (everything up to the last newline).
fn parse_log(id: &str, bytes: &[u8]) -> Result<(Vec<Json>, usize), SessionError> {
    let corrupt = |detail: String| SessionError::Corrupt(id.to_string(), detail);
    let committed_len = bytes.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let text = std::str::from_utf8(&bytes[..committed_len]).map_err(|_| corrupt("history is not UTF-8".into()))?;
    let mut records = Vec::new();
    let mut last_seq = 0;
    for (index, line) in text.split_terminator('\n').enumerate() {
        let number = index + 1;
        let record = json::parse(line).map_err(|e| corrupt(format!("line {number}: {e}")))?;
        let kind = record.get("type").as_str().ok_or_else(|| corrupt(format!("line {number}: record has no type")))?;
        let seq = record.get("seq").as_i64().ok_or_else(|| corrupt(format!("line {number}: record has no seq")))?;
        if record.get("v").as_i64() != Some(FORMAT_VERSION) {
            return Err(corrupt(format!("line {number}: unsupported record version")));
        }
        if seq <= last_seq {
            return Err(corrupt(format!("line {number}: sequence goes backwards")));
        }
        if number == 1 && (kind != "session/created" || record.get("data").get("sessionId").as_str() != Some(id)) {
            return Err(corrupt("first record does not create this session".into()));
        }
        last_seq = seq;
        records.push(record);
    }
    if records.is_empty() {
        return Err(corrupt("history is empty".into()));
    }
    Ok((records, committed_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("melchior-test-{name}-{}", util::random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_resume_and_ownership_rules() {
        let home = temp_home("sessions");
        let store = SessionStore::new(home.join("sessions"));
        let session = store.create("s1").unwrap();
        session.append("message/user", jobj! { "id" => "m1", "content" => "hello" }).unwrap();
        assert_eq!(store.create("s1").err(), Some(SessionError::AlreadyOpen("s1".into())));
        assert_eq!(store.resume("s1").err(), Some(SessionError::AlreadyOpen("s1".into())));
        let other = SessionStore::new(home.join("sessions"));
        assert_eq!(other.resume("s1").err(), Some(SessionError::Owned("s1".into())));
        assert_eq!(other.create("s1").err(), Some(SessionError::Exists("s1".into())));
        assert_eq!(other.resume("missing").err(), Some(SessionError::NotFound("missing".into())));
        assert!(!home.join("sessions/missing").exists());
        drop(session);
        store.close("s1");
        let resumed = other.resume("s1").unwrap();
        assert_eq!(resumed.record_count(), 2);
        resumed.append("message/user", jobj! { "id" => "m2", "content" => "again" }).unwrap();
        assert_eq!(resumed.records()[2].get("seq").as_i64(), Some(3));
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn refuses_corrupt_history_without_touching_it_and_repairs_torn_tail() {
        let home = temp_home("corrupt");
        let store = SessionStore::new(home.join("sessions"));
        store.create("bad").unwrap();
        store.create("torn").unwrap();
        store.close("bad");
        store.close("torn");
        let bad = home.join("sessions/bad").join(LOG_FILE);
        fs::write(&bad, b"deliberately corrupt session for regression test\n").unwrap();
        let fresh = SessionStore::new(home.join("sessions"));
        let error = fresh.resume("bad").err().unwrap();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(fs::read(&bad).unwrap(), b"deliberately corrupt session for regression test\n");
        let torn = home.join("sessions/torn").join(LOG_FILE);
        let mut contents = fs::read(&torn).unwrap();
        let committed = contents.len();
        contents.extend_from_slice(b"{\"v\":1,\"seq\":2,\"ty");
        fs::write(&torn, &contents).unwrap();
        let resumed = fresh.resume("torn").unwrap();
        assert_eq!(resumed.record_count(), 1);
        assert_eq!(fs::read(&torn).unwrap().len(), committed);
        fs::remove_dir_all(home).ok();
    }
}
