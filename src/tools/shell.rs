//! `bash` (plugin `tool-bash`) and background jobs (`tool-jobs`:
//! `job_list`, `job_output`, `job_kill`).
//!
//! With full access commands run directly in the workspace. Otherwise they
//! run inside Bubblewrap with a read-only root, the workspace writable, the
//! harness home hidden, and no network. If Bubblewrap is unusable, `bash`
//! refuses instead of running unconfined.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Tool, ToolCx, ToolOutput, ToolSpec, opt_i64, prop, schema, str_arg, try_out};
use crate::jobj;
use crate::json::Json;
use crate::runtime::Runtime;
use crate::util;

const OUTPUT_CAP: usize = 1024 * 1024;

fn build_command(runtime: &Runtime, script: &str) -> Result<Command, String> {
    let cwd = &runtime.cwd;
    if runtime.full_access() {
        let mut command = Command::new("bash");
        command.arg("-c").arg(script).current_dir(cwd);
        return Ok(command);
    }
    let bwrap = ["/usr/bin/bwrap", "/usr/local/bin/bwrap"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .ok_or("sandbox: shell unavailable (bubblewrap is not installed)")?;
    let mut command = Command::new(bwrap);
    command
        .args(["--die-with-parent", "--new-session", "--unshare-all", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"])
        .arg("--tmpfs")
        .arg(&runtime.home)
        .arg("--bind")
        .arg(cwd)
        .arg(cwd)
        .arg("--chdir")
        .arg(cwd)
        .args(["bash", "-c", script]);
    Ok(command)
}

struct Captured {
    text: Arc<Mutex<String>>,
}

fn capture(mut stream: impl Read + Send + 'static, sink: Arc<Mutex<String>>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut text = sink.lock().unwrap();
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if text.len() > OUTPUT_CAP {
                        let cut = text.len() - OUTPUT_CAP / 2;
                        let mut boundary = cut;
                        while !text.is_char_boundary(boundary) {
                            boundary += 1;
                        }
                        text.replace_range(..boundary, "…[earlier output dropped]\n");
                    }
                }
            }
        }
    })
}

type Spawned = (Arc<Mutex<std::process::Child>>, Captured, Vec<std::thread::JoinHandle<()>>);

fn spawn(runtime: &Runtime, script: &str) -> Result<Spawned, String> {
    let mut command = build_command(runtime, script)?;
    command.env_clear().envs(runtime.child_env()).env("TERM", "dumb").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("cannot start bash: {e}"))?;
    let text = Arc::new(Mutex::new(String::new()));
    let readers = vec![
        capture(child.stdout.take().expect("piped"), text.clone()),
        capture(child.stderr.take().expect("piped"), text.clone()),
    ];
    Ok((crate::proc::track(child), Captured { text }, readers))
}

pub struct Bash {
    pub jobs: Arc<Jobs>,
    pub background: bool,
}

impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        let mut properties = jobj! {
            "command" => prop("string", "The bash command to run in the workspace."),
            "description" => prop("string", "Five to ten words on what the command does."),
            "timeout_ms" => prop("integer", "How long to wait for the command, in milliseconds (default 120000, max 600000)."),
        };
        let description = if self.background {
            properties.set("run_in_background", prop("boolean", "Start as a background job and return at once; read it with job_output."));
            "Run a bash command in the workspace and return its combined output and exit code. A command still running \
             when timeout_ms elapses is not killed: it continues as a background job, and the result says which. Start \
             builds, installs, downloads, training runs and scans of large files with run_in_background, keep working, \
             and wait for them with job_output's wait_seconds instead of sleep."
        } else {
            "Run a bash command in the workspace and return its combined output and exit code. A command still running \
             when timeout_ms elapses is killed."
        };
        ToolSpec { name: "bash".into(), description: description.into(), parameters: schema(properties, &["command"]) }
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let script = try_out!(str_arg(args, "command"));
        if self.background && args.get("run_in_background").as_bool() == Some(true) {
            return match self.jobs.start(cx.runtime, script) {
                Ok(id) => ToolOutput::ok(format!("Started background job {id}. Use job_output to read it.")),
                Err(e) => ToolOutput::err(e),
            };
        }
        let timeout = Duration::from_millis(opt_i64(args, "timeout_ms").unwrap_or(120_000).clamp(1_000, 600_000) as u64);
        let (child, captured, readers) = match spawn(cx.runtime, script) {
            Ok(parts) => parts,
            Err(e) => return ToolOutput::err(e),
        };
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.lock().unwrap().try_wait().ok().flatten() {
                break Some(status);
            }
            let cancelled = cx.session.cancel.load(Ordering::SeqCst);
            if started.elapsed() > timeout && self.background && !cancelled {
                // Keep the work: the readers go on filling the job's output.
                let so_far = util::truncate(&captured.text.lock().unwrap().clone(), 30_000, "\n…[output truncated]").to_string();
                let id = self.jobs.adopt(script, child, captured.text);
                return ToolOutput::ok(format!(
                    "{so_far}\n[still running after {}s; it continues as background job {id}. Read it with job_output \
                     (wait_seconds waits for it to finish), stop it with job_kill.]",
                    timeout.as_secs()
                ));
            }
            if started.elapsed() > timeout || cancelled {
                let mut child = child.lock().unwrap();
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        for reader in readers {
            let _ = reader.join();
        }
        let output = captured.text.lock().unwrap().clone();
        let output = util::truncate(&output, 30_000, "\n…[output truncated]");
        match status {
            Some(status) => {
                let code = status.code().unwrap_or(-1);
                let body = if output.trim().is_empty() { "(no output)".to_string() } else { output };
                if code == 0 {
                    ToolOutput::ok(body)
                } else {
                    ToolOutput::ok(format!("{body}\n[exit code {code}]"))
                }
            }
            None => ToolOutput::err(format!("{output}\n[command timed out after {}s and was killed]", timeout.as_secs())),
        }
    }
}

struct Job {
    command: String,
    child: Arc<Mutex<std::process::Child>>,
    output: Arc<Mutex<String>>,
    started: String,
}

#[derive(Default)]
pub struct Jobs {
    next: AtomicU64,
    jobs: Mutex<BTreeMap<String, Job>>,
}

impl Jobs {
    fn start(&self, runtime: &Runtime, script: &str) -> Result<String, String> {
        let (child, captured, _readers) = spawn(runtime, script)?;
        Ok(self.adopt(script, child, captured.text))
    }

    /// Register a process that is already running (a foreground command that
    /// outlived its timeout) as a background job.
    fn adopt(&self, script: &str, child: Arc<Mutex<std::process::Child>>, output: Arc<Mutex<String>>) -> String {
        let id = format!("job-{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        self.jobs.lock().unwrap().insert(id.clone(), Job { command: script.to_string(), child, output, started: util::now_iso() });
        id
    }

    fn running(&self, id: &str) -> Option<bool> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(id)?;
        Some(matches!(job.child.lock().unwrap().try_wait(), Ok(None)))
    }

    fn status(job: &Job) -> String {
        match job.child.lock().unwrap().try_wait() {
            Ok(Some(status)) => format!("exited ({})", status.code().map(|c| c.to_string()).unwrap_or("signal".into())),
            Ok(None) => "running".into(),
            Err(e) => format!("unknown ({e})"),
        }
    }

    pub fn kill_all(&self) {
        for job in self.jobs.lock().unwrap().values() {
            let _ = job.child.lock().unwrap().kill();
        }
    }
}

pub struct JobList(pub Arc<Jobs>);
impl Tool for JobList {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "job_list".into(), description: "List background jobs and their status.".into(), parameters: schema(jobj! {}, &[]) }
    }
    fn call(&self, _cx: &ToolCx<'_>, _args: &Json) -> ToolOutput {
        let jobs = self.0.jobs.lock().unwrap();
        if jobs.is_empty() {
            return ToolOutput::ok("No background jobs.");
        }
        ToolOutput::ok(
            jobs.iter()
                .map(|(id, job)| format!("{id}  {}  started {}  {}", Jobs::status(job), job.started, util::truncate(&job.command, 120, "…")))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

pub struct JobOutput(pub Arc<Jobs>);
impl Tool for JobOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_output".into(),
            description: "Read the output of a background job (the most recent output is kept). With wait_seconds, \
                first wait up to that long for the job to finish; it returns as soon as the job exits."
                .into(),
            parameters: schema(
                jobj! {
                    "job_id" => prop("string", "Id from bash or job_list."),
                    "tail_chars" => prop("integer", "Return at most this many trailing characters (default 20000)."),
                    "wait_seconds" => prop("integer", "Wait up to this many seconds (max 600) for the job to finish before reading."),
                },
                &["job_id"],
            ),
        }
    }
    fn call(&self, cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let id = try_out!(str_arg(args, "job_id"));
        let wait = Duration::from_secs(opt_i64(args, "wait_seconds").unwrap_or(0).clamp(0, 600) as u64);
        let started = Instant::now();
        while started.elapsed() < wait && self.0.running(id) == Some(true) && !cx.session.cancel.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let jobs = self.0.jobs.lock().unwrap();
        let Some(job) = jobs.get(id) else { return ToolOutput::err(format!("no job {id:?}")) };
        let text = job.output.lock().unwrap().clone();
        let tail = opt_i64(args, "tail_chars").unwrap_or(20_000).clamp(100, 200_000) as usize;
        let mut start = text.len().saturating_sub(tail);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        ToolOutput::ok(format!("[{id} {}]\n{}", Jobs::status(job), &text[start..]))
    }
}

pub struct JobKill(pub Arc<Jobs>);
impl Tool for JobKill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_kill".into(),
            description: "Stop a background job.".into(),
            parameters: schema(jobj! { "job_id" => prop("string", "Id from bash or job_list.") }, &["job_id"]),
        }
    }
    fn call(&self, _cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let id = try_out!(str_arg(args, "job_id"));
        let jobs = self.0.jobs.lock().unwrap();
        let Some(job) = jobs.get(id) else { return ToolOutput::err(format!("no job {id:?}")) };
        let _ = job.child.lock().unwrap().kill();
        ToolOutput::ok(format!("Stopped {id}."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cordis::App;
    use crate::rpc::Notifier;
    use crate::runtime::Permission;
    use crate::session::Session;

    fn runtime() -> Arc<Runtime> {
        let cwd = std::env::temp_dir();
        let mut runtime = Runtime::new(cwd.clone(), cwd, "test".into(), "test".into(), None, None, Arc::new(Notifier::with_sink(|_| {})));
        runtime.permission = Permission::FullAccess;
        Arc::new(runtime)
    }

    #[test]
    fn a_command_that_outlives_its_timeout_becomes_a_job() {
        let (app, runtime, session) = (App::new(), runtime(), Session::ephemeral("s"));
        let cx = ToolCx { app: &app, runtime: &runtime, session: &session, call_id: "c", depth: 0 };
        let jobs = Arc::new(Jobs::default());
        let bash = Bash { jobs: jobs.clone(), background: true };
        let started = Instant::now();
        let out = bash.call(&cx, &jobj! { "command" => "echo begun; sleep 2; echo finished", "timeout_ms" => 1000 });
        assert!(started.elapsed() < Duration::from_millis(1900), "returned at the timeout");
        assert!(!out.is_error && out.content.contains("begun") && out.content.contains("background job job-1"), "{}", out.content);

        let waited = JobOutput(jobs.clone()).call(&cx, &jobj! { "job_id" => "job-1", "wait_seconds" => 30 });
        assert!(waited.content.contains("exited (0)") && waited.content.contains("finished"), "{}", waited.content);
        assert!(started.elapsed() < Duration::from_secs(10), "the wait ended when the job did");
    }

    #[test]
    fn without_jobs_a_timed_out_command_is_killed() {
        let (app, runtime, session) = (App::new(), runtime(), Session::ephemeral("s"));
        let cx = ToolCx { app: &app, runtime: &runtime, session: &session, call_id: "c", depth: 0 };
        let bash = Bash { jobs: Arc::new(Jobs::default()), background: false };
        let out = bash.call(&cx, &jobj! { "command" => "sleep 5", "timeout_ms" => 1000 });
        assert!(out.is_error && out.content.contains("timed out"), "{}", out.content);
    }
}
