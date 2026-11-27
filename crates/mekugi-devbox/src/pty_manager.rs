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

/// Daemon-private configuration and systemd runtime variables that must not
/// leak into agent child shells (e.g. to prevent exposing DEVBOX_INTERNAL_TOKEN
/// or shadowing docker compose .env interpolation on the host).
const SCRUBBED_DAEMON_ENV_VARS: &[&str] = &[
    "DEVBOX_INTERNAL_TOKEN",
    "DEVBOX_GRPC_ADDR",
    "DEVBOX_HOST",
    "DEVBOX_PORT",
    "DEVBOX_WORKDIR",
    "DEVBOX_SHELL",
    "CODE_MODE_HOST_BIN",
    "CODE_MODE_HOST_TARGET",
    "CODE_MODE_DOCS_PATH",
    "MAX_CELL_YIELD_TIME_MS",
    "NOTIFY_SOCKET",
    "WATCHDOG_USEC",
    "WATCHDOG_PID",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecCommandMode {
    Shell {
        cmd: String,
        shell: Option<String>,
        login: bool,
    },
    Argv {
        program: String,
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCommandOptions {
    pub mode: ExecCommandMode,
    pub workdir: Option<String>,
    pub stdin: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub tty: bool,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
}

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

    fn build_env(&self, extra_env: Option<&HashMap<String, String>>) -> HashMap<String, String> {
        Self::build_env_from(std::env::vars(), &self.session_tmp, extra_env)
    }

    fn build_env_from<I>(
        base_vars: I,
        session_tmp: &Path,
        extra_env: Option<&HashMap<String, String>>,
    ) -> HashMap<String, String>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut env: HashMap<String, String> = base_vars.into_iter().collect();
        for key in SCRUBBED_DAEMON_ENV_VARS {
            env.remove(*key);
        }
        env.insert("SESSION_TMP".into(), session_tmp.display().to_string());
        env.entry("TERM".into()).or_insert_with(|| "xterm-256color".into());
        if let Some(extra) = extra_env {
            for (k, v) in extra {
                env.insert(k.clone(), v.clone());
            }
        }
        env
    }

    pub async fn exec_command(&self, options: ExecCommandOptions) -> Result<String, String> {
        if options.tty && options.stdin.is_some() {
            return Err("exec_command 'stdin' is not supported when 'tty' is true".into());
        }
        let cwd = self.resolve_workdir(options.workdir.as_deref());
        let (program, args): (String, Vec<String>) = match &options.mode {
            ExecCommandMode::Shell { cmd, shell, login } => {
                let shell_bin = shell
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&self.default_shell);
                let args = if *login {
                    vec!["-lc".to_owned(), cmd.clone()]
                } else {
                    vec!["-c".to_owned(), cmd.clone()]
                };
                (shell_bin.to_owned(), args)
            }
            ExecCommandMode::Argv { program, args } => {
                let resolved = if !Path::new(program).is_absolute()
                    && (program.contains('/') || program.contains(std::path::MAIN_SEPARATOR))
                {
                    cwd.join(program).display().to_string()
                } else {
                    program.clone()
                };
                (resolved, args.clone())
            }
        };
        let env = self.build_env(options.env.as_ref());
        let spawned = if options.tty {
            spawn_pty_process(
                &program,
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
            spawn_pipe_process(&program, &args, &cwd, &env, &None, &[])
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

        if let Some(stdin_str) = options.stdin {
            if stdin_str.is_empty() {
                managed.handle.close_stdin();
            } else {
                let tx = managed.handle.writer_sender();
                // Drop ProcessHandle's sender copy first so tx is the sole remaining sender
                // and EOF is delivered immediately after the payload is queued.
                managed.handle.close_stdin();
                let bytes = stdin_str.into_bytes();
                tokio::spawn(async move {
                    let _ = tx.send(bytes).await;
                });
            }
        }

        let wait_ms = options
            .yield_time_ms
            .unwrap_or(DEFAULT_SHELL_YIELD_TIME_MS)
            .min(MAX_SHELL_YIELD_TIME_MS);
        let max_tokens = options
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
            .max(100);

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

    fn shell_opts(cmd: &str) -> ExecCommandOptions {
        ExecCommandOptions {
            mode: ExecCommandMode::Shell {
                cmd: cmd.to_owned(),
                shell: None,
                login: false,
            },
            workdir: None,
            stdin: None,
            env: None,
            tty: false,
            yield_time_ms: Some(5000),
            max_output_tokens: None,
        }
    }

    fn argv_opts(program: &str, args: &[&str]) -> ExecCommandOptions {
        ExecCommandOptions {
            mode: ExecCommandMode::Argv {
                program: program.to_owned(),
                args: args.iter().map(|s| (*s).to_owned()).collect(),
            },
            workdir: None,
            stdin: None,
            env: None,
            tty: false,
            yield_time_ms: Some(5000),
            max_output_tokens: None,
        }
    }

    #[tokio::test]
    async fn test_exec_and_write_stdin() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let out = mgr.exec_command(shell_opts("echo hello-rust-pty")).await.unwrap();
        assert!(out.contains("Process exited with code 0"));
        assert!(out.contains("hello-rust-pty"));
        assert_eq!(mgr.active_session_count().await, 0);
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_argv_special_shell_chars_verbatim() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let payload1 = "$(echo PWNED) `uname` $HOME";
        let payload2 = "line1\nline2 'single' \"double\" \\backslash";
        let payload3 = "; echo injected && || | > /dev/null";
        let out = mgr
            .exec_command(argv_opts("printf", &["%s|%s|%s", payload1, payload2, payload3]))
            .await
            .unwrap();
        assert!(out.contains("Process exited with code 0"), "out={out}");
        assert!(
            out.contains(&format!("{payload1}|{payload2}|{payload3}")),
            "expected verbatim payload, got: {out}"
        );
        assert!(!out.contains("PWNED\n"), "shell expansion occurred: {out}");
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_argv_stdin_and_eof() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let payload = "hello\n$HOME `backticks` 日本語 🚀\n";
        let mut opts = argv_opts("cat", &[]);
        opts.stdin = Some(payload.to_owned());
        let out = mgr.exec_command(opts).await.unwrap();
        assert!(out.contains("Process exited with code 0"), "out={out}");
        assert!(out.contains(payload), "expected stdin echoed verbatim, got: {out}");
        assert_eq!(mgr.active_session_count().await, 0);
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_argv_empty_stdin_closes_eof() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let mut opts = argv_opts("cat", &[]);
        opts.stdin = Some(String::new());
        let out = mgr.exec_command(opts).await.unwrap();
        assert!(out.contains("Process exited with code 0"), "out={out}");
        assert_eq!(mgr.active_session_count().await, 0);
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_env_overlay_for_argv_and_cmd() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let env = HashMap::from([
            ("MEKUGI_TEST_A".to_owned(), "alpha-123".to_owned()),
            ("MEKUGI_TEST_B".to_owned(), "beta $raw".to_owned()),
        ]);

        let mut argv = argv_opts("printenv", &["MEKUGI_TEST_A"]);
        argv.env = Some(env.clone());
        let out_argv = mgr.exec_command(argv).await.unwrap();
        assert!(out_argv.contains("Process exited with code 0"), "out={out_argv}");
        assert!(out_argv.contains("alpha-123"), "out={out_argv}");

        let mut shell = shell_opts("printf '%s|%s' \"$MEKUGI_TEST_A\" \"$MEKUGI_TEST_B\"");
        shell.env = Some(env);
        let out_shell = mgr.exec_command(shell).await.unwrap();
        assert!(out_shell.contains("Process exited with code 0"), "out={out_shell}");
        assert!(out_shell.contains("alpha-123|beta $raw"), "out={out_shell}");

        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_argv_nonzero_exit_code() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let out = mgr.exec_command(argv_opts("false", &[])).await.unwrap();
        assert!(out.contains("Process exited with code 1"), "out={out}");
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_argv_workdir() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let mut opts = argv_opts("pwd", &[]);
        opts.workdir = Some("nested/sub".to_owned());
        let out = mgr.exec_command(opts).await.unwrap();
        let expected = std::fs::canonicalize(tmp.path().join("nested/sub")).unwrap();
        assert!(out.contains("Process exited with code 0"), "out={out}");
        assert!(
            out.contains(&expected.display().to_string()),
            "expected {} in {out}",
            expected.display()
        );
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_argv_running_session_write_stdin() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let mut opts = argv_opts("cat", &[]);
        opts.yield_time_ms = Some(100);
        let out = mgr.exec_command(opts).await.unwrap();
        assert!(out.contains("Process running with session ID "), "out={out}");
        let sid_str = out
            .lines()
            .find_map(|l| l.strip_prefix("Process running with session ID "))
            .expect("missing session ID line");
        let sid: i64 = sid_str.trim().parse().unwrap();

        let cont = mgr
            .write_stdin(sid, "ping-from-write-stdin\n", Some(500), None)
            .await
            .unwrap();
        assert!(cont.contains("ping-from-write-stdin"), "cont={cont}");
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_argv_missing_binary_returns_error() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let err = mgr
            .exec_command(argv_opts("__mekugi_nonexistent_bin_99999__", &[]))
            .await
            .unwrap_err();
        assert!(err.contains("Failed to spawn command"), "err={err}");
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_tty_and_stdin_rejected() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let mut opts = argv_opts("cat", &[]);
        opts.tty = true;
        opts.stdin = Some("hello\n".into());
        let err = mgr.exec_command(opts).await.unwrap_err();
        assert!(err.contains("tty"), "err={err}");
        mgr.close_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_exec_argv_relative_path_works_in_pipe_and_tty() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let script = bin_dir.join("hello.sh");
        std::fs::write(&script, "#!/bin/sh\necho rel-tool-ok\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let pipe_out = mgr.exec_command(argv_opts("bin/hello.sh", &[])).await.unwrap();
        assert!(pipe_out.contains("Process exited with code 0"), "out={pipe_out}");
        assert!(pipe_out.contains("rel-tool-ok"), "out={pipe_out}");

        let mut tty_opts = argv_opts("bin/hello.sh", &[]);
        tty_opts.tty = true;
        let tty_out = mgr.exec_command(tty_opts).await.unwrap();
        assert!(tty_out.contains("Process exited with code 0"), "out={tty_out}");
        assert!(tty_out.contains("rel-tool-ok"), "out={tty_out}");
        mgr.close_all().await;
    }

    #[test]
    fn test_build_env_scrubs_daemon_vars_and_allows_explicit_override() {
        let base = vec![
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("DEVBOX_INTERNAL_TOKEN".to_owned(), "super-secret".to_owned()),
            ("DEVBOX_PORT".to_owned(), "8080".to_owned()),
            ("NOTIFY_SOCKET".to_owned(), "/run/systemd/notify".to_owned()),
            ("WATCHDOG_USEC".to_owned(), "30000000".to_owned()),
        ];
        let mut extra = HashMap::new();
        extra.insert("DEVBOX_PORT".to_owned(), "9090".to_owned());
        extra.insert("CUSTOM_VAR".to_owned(), "ok".to_owned());

        let env = ProcessManager::build_env_from(base, Path::new("/tmp/session"), Some(&extra));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(env.get("SESSION_TMP").map(String::as_str), Some("/tmp/session"));
        assert_eq!(env.get("CUSTOM_VAR").map(String::as_str), Some("ok"));
        assert_eq!(env.get("DEVBOX_PORT").map(String::as_str), Some("9090"));
        assert!(!env.contains_key("DEVBOX_INTERNAL_TOKEN"));
        assert!(!env.contains_key("NOTIFY_SOCKET"));
        assert!(!env.contains_key("WATCHDOG_USEC"));
    }

    #[tokio::test]
    async fn test_exec_argv_large_stdin_fast_exit_does_not_block() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let mut payload = String::with_capacity(260 * 1024);
        payload.push_str("first-line-ok\n");
        for _ in 0..4096 {
            payload.push_str("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n");
        }
        let mut opts = argv_opts("head", &["-n", "1"]);
        opts.stdin = Some(payload);
        opts.yield_time_ms = Some(3_000);

        let out = mgr.exec_command(opts).await.unwrap();
        assert!(out.contains("Process exited with code 0"), "out={out}");
        assert!(out.contains("first-line-ok"), "out={out}");
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_exec_argv_large_stdin_unconsumed_respects_yield_time() {
        let tmp = TempDir::new().unwrap();
        let mgr = ProcessManager::new(tmp.path().to_path_buf(), "/bin/bash".into());
        let payload = "x".repeat(256 * 1024);
        let mut opts = argv_opts("sleep", &["5"]);
        opts.stdin = Some(payload);
        opts.yield_time_ms = Some(200);

        let started = Instant::now();
        let out = mgr.exec_command(opts).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "exec_command blocked past yield_time_ms: {:?}",
            started.elapsed()
        );
        assert!(out.contains("Process running with session ID"), "out={out}");
        mgr.close_all().await;
    }
}
