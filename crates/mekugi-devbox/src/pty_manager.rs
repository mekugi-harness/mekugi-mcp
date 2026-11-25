use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use codex_utils_pty::{
    ChildFds, ProcessHandle, TerminalSize, spawn_pipe_process, spawn_pty_process,
};
use tokio::sync::{Mutex, Notify, watch};
use tokio::task::JoinHandle;

pub const DEFAULT_SHELL_YIELD_TIME_MS: u64 = 10_000;
pub const MAX_SHELL_YIELD_TIME_MS: u64 = 40_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
const CHARS_PER_TOKEN: usize = 4;
const MAX_BUFFER_BYTES: usize = 2 * 1024 * 1024;
const EXITED_SESSION_TTL: Duration = Duration::from_secs(600);

pub fn truncate_output(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(CHARS_PER_TOKEN).max(256);
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_owned();
    }
    let head = max_chars / 2;
    let tail = max_chars - head;
    let omitted = total_chars - max_chars;
    let head_str: String = text.chars().take(head).collect();
    let tail_str: String = text
        .chars()
        .skip(total_chars.saturating_sub(tail))
        .collect();
    format!("{head_str}
...[{omitted} chars truncated]...
{tail_str}")
}

struct ManagedSession {
    handle: ProcessHandle,
    started_at: Instant,
    buffer: Arc<Mutex<Vec<u8>>>,
    notify: Arc<Notify>,
    exit_rx: watch::Receiver<bool>,
    reader_task: JoinHandle<()>,
    reaper_task: JoinHandle<()>,
}

impl ManagedSession {
    async fn drain_output(&self) -> String {
        let mut buf = self.buffer.lock().await;
        if buf.is_empty() {
            return String::new();
        }
        let data = std::mem::take(&mut *buf);
        String::from_utf8_lossy(&data).into_owned()
    }

    fn terminate(&self) {
        self.handle.terminate();
        self.reader_task.abort();
        self.reaper_task.abort();
    }
}

pub struct ProcessManager {
    default_workdir: PathBuf,
    default_shell: String,
    session_tmp: PathBuf,
    next_id: AtomicI64,
    sessions: Arc<Mutex<HashMap<i64, Arc<ManagedSession>>>>,
}

impl ProcessManager {
    pub fn new(default_workdir: PathBuf, default_shell: String) -> Self {
        let session_tmp = std::env::var("SESSION_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/mekugi-session"));
        let _ = std::fs::create_dir_all(&session_tmp);
        Self {
            default_workdir,
            default_shell,
            session_tmp,
            next_id: AtomicI64::new(1000),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn active_session_count(&self) -> u32 {
        let map = self.sessions.lock().await;
        map.values().filter(|s| !s.handle.has_exited()).count() as u32
    }

    fn resolve_workdir(&self, workdir: Option<&str>) -> PathBuf {
        let path = match workdir.map(str::trim).filter(|s| !s.is_empty()) {
            Some(w) => {
                let p = Path::new(w);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    self.default_workdir.join(p)
                }
            }
            None => self.default_workdir.clone(),
        };
        let _ = std::fs::create_dir_all(&path);
        std::fs::canonicalize(&path).unwrap_or(path)
    }

    fn build_env(&self) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert(
            "SESSION_TMP".into(),
            self.session_tmp.display().to_string(),
        );
        env.entry("TERM".into()).or_insert_with(|| "xterm-256color".into());
        env
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn exec_command(
        &self,
        cmd: &str,
        workdir: Option<&str>,
        shell: Option<&str>,
        login: bool,
        tty: bool,
        yield_time_ms: Option<u64>,
        max_output_tokens: Option<usize>,
    ) -> Result<String, String> {
        let cwd = self.resolve_workdir(workdir);
        let shell_bin = shell
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.default_shell);
        let args = if login {
            vec!["-lc".to_owned(), cmd.to_owned()]
        } else {
            vec!["-c".to_owned(), cmd.to_owned()]
        };
        let env = self.build_env();
        let spawned = if tty {
            spawn_pty_process(
                shell_bin,
                &args,
                &cwd,
                &env,
                &None,
                TerminalSize::default(),
                ChildFds::Attached(&[]),
            )
            .await
            .map_err(|e| format!("Failed to spawn PTY command: {e}"))?
        } else {
            spawn_pipe_process(shell_bin, &args, &cwd, &env, &None, &[])
                .await
                .map_err(|e| format!("Failed to spawn command: {e}"))?
        };

        let session_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let notify = Arc::new(Notify::new());
        let (exit_watch_tx, exit_watch_rx) = watch::channel(false);

        // Drain stdout_rx and stderr_rx directly over lossless mpsc channels (no broadcast Lagged drops).
        let mut stdout_rx = spawned.stdout_rx;
        let mut stderr_rx = spawned.stderr_rx;
        let buf_clone = buffer.clone();
        let notify_clone = notify.clone();
        let reader_task = tokio::spawn(async move {
            let mut stdout_open = true;
            let mut stderr_open = true;
            while stdout_open || stderr_open {
                let maybe_chunk = tokio::select! {
                    res = stdout_rx.recv(), if stdout_open => {
                        if res.is_none() {
                            stdout_open = false;
                        }
                        res
                    }
                    res = stderr_rx.recv(), if stderr_open => {
                        if res.is_none() {
                            stderr_open = false;
                        }
                        res
                    }
                };
                if let Some(chunk) = maybe_chunk
                    && !chunk.is_empty()
                {
                    let mut b = buf_clone.lock().await;
                    b.extend_from_slice(&chunk);
                    if b.len() > MAX_BUFFER_BYTES {
                        let excess = b.len() - MAX_BUFFER_BYTES;
                        b.drain(..excess);
                    }
                    drop(b);
                    notify_clone.notify_waiters();
                }
            }
            notify_clone.notify_waiters();
        });

        let sessions_clone = self.sessions.clone();
        let notify_exit = notify.clone();
        let exit_rx = spawned.exit_rx;
        let reaper_task = tokio::spawn(async move {
            let _ = exit_rx.await;
            let _ = exit_watch_tx.send(true);
            notify_exit.notify_waiters();
            tokio::time::sleep(EXITED_SESSION_TTL).await;
            let mut map = sessions_clone.lock().await;
            if let Some(entry) = map.remove(&session_id) {
                entry.reader_task.abort();
            }
        });

        let managed = Arc::new(ManagedSession {
            handle: spawned.session,
            started_at: Instant::now(),
            buffer,
            notify,
            exit_rx: exit_watch_rx,
            reader_task,
            reaper_task,
        });

        self.sessions.lock().await.insert(session_id, managed.clone());

        let wait_ms = yield_time_ms
            .unwrap_or(DEFAULT_SHELL_YIELD_TIME_MS)
            .min(MAX_SHELL_YIELD_TIME_MS);
        let max_tokens = max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS).max(100);

        self.wait_for_session(session_id, managed, wait_ms, max_tokens)
            .await
    }

    pub async fn write_stdin(
        &self,
        session_id: i64,
        chars: &str,
        yield_time_ms: Option<u64>,
        max_output_tokens: Option<usize>,
    ) -> Result<String, String> {
        let managed = self
            .sessions
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| format!("Unknown or expired shell session ID: {session_id}"))?;

        if !chars.is_empty() {
            let tx = managed.handle.writer_sender();
            let _ = tx.send(chars.as_bytes().to_vec()).await;
        }

        let wait_ms = yield_time_ms.unwrap_or(250).min(MAX_SHELL_YIELD_TIME_MS);
        let max_tokens = max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS).max(100);

        self.wait_for_session(session_id, managed, wait_ms, max_tokens)
            .await
    }

    async fn wait_for_session(
        &self,
        session_id: i64,
        managed: Arc<ManagedSession>,
        wait_ms: u64,
        max_tokens: usize,
    ) -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut exit_watch = managed.exit_rx.clone();
        loop {
            if *exit_watch.borrow() || managed.handle.has_exited() {
                // Brief drain so reader_task flushes final pipe bytes.
                tokio::time::sleep(Duration::from_millis(15)).await;
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            // Wait on either exit watch (no lost wakeups) or new output notification.
            let _ = tokio::time::timeout(remaining, async {
                tokio::select! {
                    _ = exit_watch.changed() => {}
                    _ = managed.notify.notified() => {}
                }
            })
            .await;
        }

        let elapsed = managed.started_at.elapsed().as_secs_f64();
        let output = truncate_output(&managed.drain_output().await, max_tokens);

        if *exit_watch.borrow() || managed.handle.has_exited() {
            let code = managed.handle.exit_code().unwrap_or(-1);
            let mut map = self.sessions.lock().await;
            if let Some(removed) = map.remove(&session_id) {
                removed.reader_task.abort();
                removed.reaper_task.abort();
            }
            Ok(format!(
                "Wall time: {elapsed:.4} seconds
Process exited with code {code}
Output:
{output}"
            ))
        } else {
            Ok(format!(
                "Wall time: {elapsed:.4} seconds
Process running with session ID {session_id}
Output:
{output}"
            ))
        }
    }

    pub async fn close_all(&self) {
        let mut map = self.sessions.lock().await;
        for (_, session) in map.drain() {
            session.terminate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_exec_and_write_stdin() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let out = mgr
            .exec_command("echo hello-rust-pty", None, None, false, false, Some(5000), None)
            .await
            .unwrap();
        assert!(out.contains("Process exited with code 0"));
        assert!(out.contains("hello-rust-pty"));
        assert_eq!(mgr.active_session_count().await, 0);
        mgr.close_all().await;
    }
}
