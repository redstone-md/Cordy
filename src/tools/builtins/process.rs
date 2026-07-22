//! Background processes — start long-running commands (dev servers, watchers, builds) without
//! blocking the agent, then poll their output, wait for a condition, or kill them.
//!
//! [`BgRegistry`] is shared between the `bash` tool (which spawns background jobs when
//! `background: true`) and the [`Process`] tool (which inspects them). Each job streams its
//! combined stdout+stderr into a capped in-memory buffer; the agent reads new output on demand.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

use crate::core::types::ToolOutput;
use crate::tools::{Tool, ToolCtx};

/// Keep at most this many bytes of a background job's output in memory (the oldest is dropped).
const BUF_CAP: usize = 64 * 1024;

/// A single background job.
struct Job {
    command: String,
    /// OS pid of the spawned shell (for a synchronous tree-kill on app exit).
    pid: Option<u32>,
    output: Arc<Mutex<String>>,
    done: Arc<AtomicBool>,
    exit: Arc<Mutex<Option<i32>>>,
    /// Send to request a kill; taken once used.
    kill: Option<oneshot::Sender<()>>,
    /// How many bytes of `output` the agent has already been shown (for incremental reads).
    seen: usize,
}

impl Job {
    fn status(&self) -> String {
        if self.done.load(Ordering::SeqCst) {
            match *self.exit.lock().unwrap() {
                Some(-2) => "killed".into(),
                Some(c) => format!("exited (code {c})"),
                None => "exited".into(),
            }
        } else {
            "running".into()
        }
    }
}

/// Shared registry of background jobs, cloneable so tools can share one instance.
#[derive(Clone, Default)]
pub struct BgRegistry {
    inner: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    next: u64,
    jobs: HashMap<String, Job>,
}

impl BgRegistry {
    /// Spawn `command` via `program`+`args` in `cwd` as a background job; returns its id.
    pub fn spawn(
        &self,
        program: &str,
        args: Vec<String>,
        cwd: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<String> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // On unix, make the child its own process-group leader so we can kill the whole tree
        // (npm -> node -> vite) via the negative pid. On Windows, taskkill /T handles the tree.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let pid = child.id();

        let output = Arc::new(Mutex::new(String::new()));
        let done = Arc::new(AtomicBool::new(false));
        let exit = Arc::new(Mutex::new(None));
        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        // Reader tasks: fold stdout and stderr lines into the shared, capped buffer.
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, output.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, output.clone());
        }

        // Supervisor: wait for exit or a kill request, then record the outcome.
        let done_c = done.clone();
        let exit_c = exit.clone();
        tokio::spawn(async move {
            let code = tokio::select! {
                status = child.wait() => status.ok().and_then(|s| s.code()).unwrap_or(-1),
                _ = kill_rx => {
                    // Kill the whole tree so the actual dev server (a grandchild) dies too.
                    if let Some(pid) = pid {
                        kill_tree(pid).await;
                    }
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    -2 // sentinel: killed
                }
            };
            *exit_c.lock().unwrap() = Some(code);
            done_c.store(true, Ordering::SeqCst);
        });

        let mut st = self.inner.lock().unwrap();
        let id = format!("bg{}", st.next);
        st.next += 1;
        st.jobs.insert(
            id.clone(),
            Job {
                command: command.to_string(),
                pid,
                output,
                done,
                exit,
                kill: Some(kill_tx),
                seen: 0,
            },
        );
        Ok(id)
    }

    /// New output since the last read for `id`, plus its status. Advances the read cursor.
    fn read_new(&self, id: &str) -> Option<(String, String)> {
        let mut st = self.inner.lock().unwrap();
        let status = st.jobs.get(id)?.status();
        let job = st.jobs.get_mut(id)?;
        let buf = job.output.lock().unwrap().clone();
        let new = if job.seen <= buf.len() {
            buf[job.seen..].to_string()
        } else {
            buf.clone() // buffer was trimmed; show what we have
        };
        job.seen = buf.len();
        Some((new, status))
    }

    /// Whether the job has finished.
    fn is_done(&self, id: &str) -> Option<bool> {
        let st = self.inner.lock().unwrap();
        st.jobs.get(id).map(|j| j.done.load(Ordering::SeqCst))
    }

    /// The full current buffer (for `wait`'s condition check).
    fn snapshot(&self, id: &str) -> Option<String> {
        let st = self.inner.lock().unwrap();
        Some(st.jobs.get(id)?.output.lock().unwrap().clone())
    }

    /// Request a kill.
    fn kill(&self, id: &str) -> bool {
        let mut st = self.inner.lock().unwrap();
        match st.jobs.get_mut(id) {
            Some(job) => {
                if let Some(tx) = job.kill.take() {
                    let _ = tx.send(());
                }
                true
            }
            None => false,
        }
    }

    /// Synchronously tree-kill every still-running job. Called on app exit so background dev
    /// servers don't outlive Cordy (async tasks wouldn't get a chance to run during shutdown).
    pub fn kill_all(&self) {
        let st = self.inner.lock().unwrap();
        for job in st.jobs.values() {
            if job.done.load(Ordering::SeqCst) {
                continue;
            }
            if let Some(pid) = job.pid {
                kill_tree_blocking(pid);
            }
        }
    }

    /// Number of jobs that are still running (for the status bar).
    pub fn running_count(&self) -> usize {
        let st = self.inner.lock().unwrap();
        st.jobs
            .values()
            .filter(|j| !j.done.load(Ordering::SeqCst))
            .count()
    }

    /// `(id, command, status)` for every known job.
    fn list(&self) -> Vec<(String, String, String)> {
        let st = self.inner.lock().unwrap();
        let mut v: Vec<_> = st
            .jobs
            .iter()
            .map(|(id, j)| (id.clone(), j.command.clone(), j.status()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

/// Terminate a process and all of its descendants. Killing just the spawned shell leaves the
/// real server (a grandchild) running, so we kill the whole tree: `taskkill /T /F` on Windows,
/// the process group (negative pid) on unix.
async fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    #[cfg(unix)]
    {
        // The child leads its own group (process_group(0)); `-pid` targets the whole group.
        let _ = tokio::process::Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
}

/// Blocking tree-kill for shutdown (no async runtime needed).
fn kill_tree_blocking(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .status();
    }
}

/// Stream a child pipe line-by-line into the shared buffer, capping its size.
fn spawn_reader<R>(reader: R, buf: Arc<Mutex<String>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut b = buf.lock().unwrap();
            b.push_str(&line);
            b.push('\n');
            if b.len() > BUF_CAP {
                let cut = b.len() - BUF_CAP;
                b.drain(..cut);
            }
        }
    });
}

/// The `process` tool: inspect and control background jobs started by `bash` (`background: true`).
pub struct Process {
    reg: BgRegistry,
}

impl Process {
    pub fn new(reg: BgRegistry) -> Self {
        Process { reg }
    }
}

#[async_trait]
impl Tool for Process {
    fn name(&self) -> &str {
        "process"
    }

    fn description(&self) -> &str {
        "Inspect background jobs started by `bash` with background:true. \
         action=output reads new output; action=wait blocks until `until` (regex) appears, the job \
         exits, or timeout_secs elapses; action=kill stops it; action=list shows all jobs."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["output", "wait", "kill", "list"] },
                "id": { "type": "string", "description": "Job id (e.g. bg0). Not needed for list." },
                "until": { "type": "string", "description": "wait: regex to watch for in the output." },
                "timeout_secs": { "type": "integer", "description": "wait: max seconds (default 30)." }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let action = input["action"].as_str().unwrap_or("output");
        if action == "list" {
            let jobs = self.reg.list();
            if jobs.is_empty() {
                return ToolOutput::ok("no background jobs");
            }
            let body = jobs
                .into_iter()
                .map(|(id, cmd, st)| format!("{id}  [{st}]  {cmd}"))
                .collect::<Vec<_>>()
                .join("\n");
            return ToolOutput::ok(body);
        }

        let Some(id) = input["id"].as_str() else {
            return ToolOutput::error("process: missing `id`");
        };

        match action {
            "kill" => {
                if self.reg.kill(id) {
                    ToolOutput::ok(format!("{id}: kill requested"))
                } else {
                    ToolOutput::error(format!("process: no job {id}"))
                }
            }
            "wait" => {
                let until = input["until"].as_str();
                let re = match until.map(regex::Regex::new) {
                    Some(Ok(r)) => Some(r),
                    Some(Err(e)) => return ToolOutput::error(format!("process: bad `until`: {e}")),
                    None => None,
                };
                let timeout = input["timeout_secs"].as_u64().unwrap_or(30).min(600);
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
                loop {
                    let Some(done) = self.reg.is_done(id) else {
                        return ToolOutput::error(format!("process: no job {id}"));
                    };
                    let matched = re
                        .as_ref()
                        .is_some_and(|r| self.reg.snapshot(id).is_some_and(|s| r.is_match(&s)));
                    if matched || done || tokio::time::Instant::now() >= deadline {
                        let (new, status) = self.reg.read_new(id).unwrap_or_default_pair();
                        let why = if matched {
                            "condition met"
                        } else if done {
                            "job finished"
                        } else {
                            "timeout"
                        };
                        return ToolOutput::ok(format!("[{id} · {status} · {why}]\n{new}"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
            _ => {
                // output
                match self.reg.read_new(id) {
                    Some((new, status)) => {
                        let body = if new.is_empty() {
                            format!("[{id} · {status}] (no new output)")
                        } else {
                            format!("[{id} · {status}]\n{new}")
                        };
                        ToolOutput::ok(body)
                    }
                    None => ToolOutput::error(format!("process: no job {id}")),
                }
            }
        }
    }
}

/// Small helper so `read_new(...).unwrap_or_default_pair()` reads cleanly.
trait PairDefault {
    fn unwrap_or_default_pair(self) -> (String, String);
}
impl PairDefault for Option<(String, String)> {
    fn unwrap_or_default_pair(self) -> (String, String) {
        self.unwrap_or_else(|| (String::new(), "gone".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> (&'static str, fn(&str) -> Vec<String>) {
        if cfg!(windows) {
            ("powershell", |c: &str| {
                vec!["-NoProfile".into(), "-Command".into(), c.into()]
            })
        } else {
            ("sh", |c: &str| vec!["-c".into(), c.into()])
        }
    }

    #[tokio::test]
    async fn background_job_captures_output_and_exits() {
        let dir = tempfile::tempdir().unwrap();
        let reg = BgRegistry::default();
        let (prog, mkargs) = shell();
        let id = reg
            .spawn(prog, mkargs("echo hello_bg"), dir.path(), "echo hello_bg")
            .unwrap();

        // Wait for it to finish.
        for _ in 0..40 {
            if reg.is_done(&id) == Some(true) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (out, status) = reg.read_new(&id).unwrap();
        assert!(out.contains("hello_bg"), "got: {out}");
        assert!(status.starts_with("exited"), "status: {status}");
    }

    #[tokio::test]
    async fn list_and_kill() {
        let dir = tempfile::tempdir().unwrap();
        let reg = BgRegistry::default();
        let (prog, mkargs) = shell();
        let sleeper = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let id = reg
            .spawn(prog, mkargs(sleeper), dir.path(), sleeper)
            .unwrap();
        assert_eq!(reg.list().len(), 1);
        assert!(reg.kill(&id));
    }
}
