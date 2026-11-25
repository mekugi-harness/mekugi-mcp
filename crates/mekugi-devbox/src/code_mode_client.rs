use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tonic::transport::{Channel, Endpoint};
use tracing::{info, warn};
use uuid::Uuid;

use crate::apply_patch::apply_patch;
use crate::pragma::parse_exec_source;
use crate::pty_manager::ProcessManager;

#[allow(clippy::result_large_err)]
pub mod cm_pb {
    tonic::include_proto!("codex.code_mode.v1");
}

use cm_pb::code_mode_host_client::CodeModeHostClient as GrpcCodeModeClient;

pub const DEFAULT_CELL_YIELD_TIME_MS: u32 = 10_000;
pub const MAX_CELL_YIELD_TIME_MS: u32 = 45_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 10_000;
const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormattedOutcome {
    pub cell_id: String,
    pub status: String,
    pub text: String,
    pub more_output: bool,
    pub error_text: Option<String>,
}

pub fn format_execution_outcome(
    outcome: &cm_pb::ExecutionOutcome,
    missing: bool,
) -> FormattedOutcome {
    let mut text_parts = Vec::new();
    let mut more_output = false;
    let mut continuation = false;

    for item in &outcome.content_items {
        match &item.item {
            Some(cm_pb::content_item::Item::Text(t)) => text_parts.push(t.text.clone()),
            Some(cm_pb::content_item::Item::Image(img)) => {
                let prefix: String = img.image_url.chars().take(64).collect();
                text_parts.push(format!("[image: {prefix}...]"));
            }
            Some(cm_pb::content_item::Item::Audio(aud)) => {
                let prefix: String = aud.audio_url.chars().take(64).collect();
                text_parts.push(format!("[audio: {prefix}...]"));
            }
            Some(cm_pb::content_item::Item::PageBoundary(pb)) => {
                more_output = more_output || pb.more_output;
                continuation = continuation || pb.continuation;
            }
            None => {}
        }
    }

    let body = text_parts.join("
");
    let elapsed_s = (outcome.code_mode_host_duration_ns as f64) / 1_000_000_000.0;

    let (status, header, error_text) = if missing {
        (
            "missing".to_owned(),
            format!(
                "Cell {} is not running or has already finished.",
                outcome.cell_id
            ),
            None,
        )
    } else {
        match &outcome.outcome {
            Some(cm_pb::execution_outcome::Outcome::Yielded(_)) => (
                "yielded".to_owned(),
                format!(
                    "Wall time: {elapsed_s:.4} seconds
Script running with cell ID {}",
                    outcome.cell_id
                ),
                None,
            ),
            Some(cm_pb::execution_outcome::Outcome::Terminated(_)) => (
                "terminated".to_owned(),
                format!(
                    "Wall time: {elapsed_s:.4} seconds
Script terminated (cell ID {})",
                    outcome.cell_id
                ),
                None,
            ),
            Some(cm_pb::execution_outcome::Outcome::Completed(c)) => {
                if let Some(err) = &c.error_text {
                    (
                        "completed".to_owned(),
                        format!("Wall time: {elapsed_s:.4} seconds
Script failed:
{err}"),
                        Some(err.clone()),
                    )
                } else {
                    (
                        "completed".to_owned(),
                        format!("Wall time: {elapsed_s:.4} seconds
Script completed"),
                        None,
                    )
                }
            }
            None => (
                "completed".to_owned(),
                format!("Wall time: {elapsed_s:.4} seconds
Script completed"),
                None,
            ),
        }
    };

    let mut lines = vec![header];
    if !body.is_empty() {
        let label = if continuation {
            "Output (continued):"
        } else {
            "Output:"
        };
        lines.push(format!("{label}
{body}"));
    }
    if more_output {
        lines.push(format!(
            "More output available. Call wait with cell_id='{}'.",
            outcome.cell_id
        ));
    }

    FormattedOutcome {
        cell_id: outcome.cell_id.clone(),
        status,
        text: lines.join("
"),
        more_output,
        error_text,
    }
}

fn build_nested_tool_definitions() -> Vec<cm_pb::ToolDefinition> {
    let exec_schema = json!({
        "type": "object",
        "properties": {
            "cmd": {"type": "string", "description": "Shell command to execute inside the devbox."},
            "workdir": {"type": "string", "description": "Optional working directory (defaults to /workspace)."},
            "shell": {"type": "string", "description": "Optional shell executable (defaults to /bin/bash)."},
            "login": {"type": "boolean", "description": "Run shell with -l login semantics (default true)."},
            "tty": {"type": "boolean", "description": "Allocate a PTY for interactive sessions (default false)."},
            "yield_time_ms": {"type": "number", "description": "How long to wait in ms before returning a running session ID (default 10000)."},
            "max_output_tokens": {"type": "number", "description": "Maximum output tokens to return (default 10000)."}
        },
        "required": ["cmd"]
    });
    let write_stdin_schema = json!({
        "type": "object",
        "properties": {
            "session_id": {"type": "number", "description": "Identifier of the running shell session."},
            "chars": {"type": "string", "description": "Bytes/characters to write to stdin (empty string polls output)."},
            "yield_time_ms": {"type": "number", "description": "How long to wait in ms for output (default 250)."},
            "max_output_tokens": {"type": "number", "description": "Maximum output tokens to return (default 10000)."}
        },
        "required": ["session_id"]
    });

    vec![
        cm_pb::ToolDefinition {
            name: "exec_command".into(),
            tool_name: Some(cm_pb::ToolName {
                name: "exec_command".into(),
                namespace: None,
            }),
            description: "Runs a command in a shell or PTY inside the devbox, returning output or a session ID.".into(),
            kind: cm_pb::ToolKind::Function as i32,
            input_schema_json: Some(serde_json::to_vec(&exec_schema).unwrap_or_default()),
            output_schema_json: None,
            pauses_cell_deadline: false,
        },
        cm_pb::ToolDefinition {
            name: "write_stdin".into(),
            tool_name: Some(cm_pb::ToolName {
                name: "write_stdin".into(),
                namespace: None,
            }),
            description: "Writes characters to an existing exec_command session and returns recent output.".into(),
            kind: cm_pb::ToolKind::Function as i32,
            input_schema_json: Some(serde_json::to_vec(&write_stdin_schema).unwrap_or_default()),
            output_schema_json: None,
            pauses_cell_deadline: false,
        },
        cm_pb::ToolDefinition {
            name: "apply_patch".into(),
            tool_name: Some(cm_pb::ToolName {
                name: "apply_patch".into(),
                namespace: None,
            }),
            description: "Applies a Codex patch (*** Begin Patch ... *** End Patch) to files in the devbox.".into(),
            kind: cm_pb::ToolKind::Freeform as i32,
            input_schema_json: None,
            output_schema_json: None,
            pauses_cell_deadline: false,
        },
    ]
}

pub struct HostSupervisor {
    binary_path: Option<PathBuf>,
    listen_addr: String,
    proc: Mutex<Option<Child>>,
}

impl HostSupervisor {
    pub fn new(binary_path: Option<PathBuf>, listen_addr: String) -> Self {
        Self {
            binary_path,
            listen_addr,
            proc: Mutex::new(None),
        }
    }

    pub async fn is_running(&self) -> bool {
        if self.binary_path.is_none() {
            return true;
        }
        let mut guard = self.proc.lock().await;
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub async fn ensure_running(&self) -> Result<(), String> {
        let Some(bin) = &self.binary_path else {
            return Ok(());
        };
        let mut guard = self.proc.lock().await;
        if let Some(child) = guard.as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            return Ok(());
        }
        if !bin.exists() {
            return Err(format!("CODE_MODE_HOST_BIN not found: {}", bin.display()));
        }

        let listen_url = format!("grpc://{}", self.listen_addr);
        info!(
            "Starting codex-code-mode-host: {} --listen {}",
            bin.display(),
            listen_url
        );
        let child = Command::new(bin)
            .arg("--listen")
            .arg(&listen_url)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to spawn codex-code-mode-host: {e}"))?;
        *guard = Some(child);

        for _ in 0..100 {
            if let Some(child) = guard.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                *guard = None;
                return Err(format!(
                    "codex-code-mode-host exited early with status {status}"
                ));
            }
            if TcpStream::connect(&self.listen_addr).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if let Some(mut child) = guard.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        Err(format!(
            "codex-code-mode-host did not listen on {} in time",
            self.listen_addr
        ))
    }

    pub async fn stop(&self) {
        let mut guard = self.proc.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
    }
}

struct SessionState {
    channel: Option<Channel>,
    session_id: Option<String>,
    supports_catalog_revision: bool,
    catalog_installed: bool,
    session_task: Option<JoinHandle<()>>,
    subscribe_task: Option<JoinHandle<()>>,
}

type SessionOpenedResult = Result<(String, bool), String>;

pub struct CodeModeHostClient {
    target: String,
    workdir: PathBuf,
    max_yield_time_ms: u32,
    supervisor: Arc<HostSupervisor>,
    process_manager: Arc<ProcessManager>,
    nested_tools: Vec<cm_pb::ToolDefinition>,
    state: Arc<Mutex<SessionState>>,
    active_cells: Arc<Mutex<HashSet<String>>>,
    inflight_calls: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    cancelled_invocations: Arc<Mutex<HashSet<String>>>,
    session_lock: Mutex<()>,
}

impl CodeModeHostClient {
    pub fn new(
        target: String,
        workdir: PathBuf,
        default_shell: String,
        max_yield_time_ms: u32,
        supervisor: Arc<HostSupervisor>,
    ) -> Self {
        let process_manager = Arc::new(ProcessManager::new(workdir.clone(), default_shell));
        Self {
            target,
            workdir,
            max_yield_time_ms,
            supervisor,
            process_manager,
            nested_tools: build_nested_tool_definitions(),
            state: Arc::new(Mutex::new(SessionState {
                channel: None,
                session_id: None,
                supports_catalog_revision: false,
                catalog_installed: false,
                session_task: None,
                subscribe_task: None,
            })),
            active_cells: Arc::new(Mutex::new(HashSet::new())),
            inflight_calls: Arc::new(Mutex::new(HashMap::new())),
            cancelled_invocations: Arc::new(Mutex::new(HashSet::new())),
            session_lock: Mutex::new(()),
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub async fn session_id(&self) -> Option<String> {
        self.state.lock().await.session_id.clone()
    }

    pub async fn active_cell_count(&self) -> u32 {
        self.active_cells.lock().await.len() as u32
    }

    pub async fn active_shell_count(&self) -> u32 {
        self.process_manager.active_session_count().await
    }

    pub async fn is_host_running(&self) -> bool {
        self.supervisor.is_running().await
    }

    pub async fn start(&self) -> Result<(), String> {
        let _guard = self.session_lock.lock().await;
        if self.state.lock().await.session_id.is_some() {
            return Ok(());
        }
        self.open_session_locked().await
    }

    async fn open_session_locked(&self) -> Result<(), String> {
        self.close_streams_locked().await;
        self.supervisor.ensure_running().await?;

        let uri = if self.target.contains("://") {
            self.target.clone()
        } else {
            format!("http://{}", self.target)
        };
        let channel = Endpoint::from_shared(uri)
            .map_err(|e| format!("Invalid code-mode host target '{}': {e}", self.target))?
            .connect_timeout(Duration::from_secs(10))
            .tcp_nodelay(true)
            .connect()
            .await
            .map_err(|e| format!("Failed to connect to codex-code-mode-host: {e}"))?;

        let mut open_client = GrpcCodeModeClient::new(channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);

        let mut stream = open_client
            .open_session(cm_pb::OpenSessionRequest {
                cell_execution_limits: None,
                output_truncation: Some(cm_pb::OutputTruncation::Server as i32),
                max_pending_delegate_calls: None,
                observation_liveness: false,
                heartbeat_interval_s: 15,
            })
            .await
            .map_err(|e| format!("OpenSession failed: {e}"))?
            .into_inner();

        let opened_notify = Arc::new(Notify::new());
        let opened_result: Arc<Mutex<Option<SessionOpenedResult>>> =
            Arc::new(Mutex::new(None));

        let state_for_session = self.state.clone();
        let cells_for_session = self.active_cells.clone();
        let inflight_for_session = self.inflight_calls.clone();
        let cancelled_for_session = self.cancelled_invocations.clone();
        let channel_for_ack = channel.clone();
        let notify_clone = opened_notify.clone();
        let res_clone = opened_result.clone();

        let session_task = tokio::spawn(async move {
            let mut current_session_id: Option<String> = None;
            loop {
                match stream.message().await {
                    Ok(Some(event)) => match event.event {
                        Some(cm_pb::session_event::Event::Opened(opened)) => {
                            current_session_id = Some(opened.session_id.clone());
                            *res_clone.lock().await =
                                Some(Ok((opened.session_id, opened.supports_catalog_revision)));
                            notify_clone.notify_one();
                        }
                        Some(cm_pb::session_event::Event::ToolCallCancelled(c)) => {
                            if let Some(handle) =
                                inflight_for_session.lock().await.remove(&c.invocation_id)
                            {
                                handle.abort();
                            } else {
                                let mut cancelled = cancelled_for_session.lock().await;
                                if cancelled.len() >= 1024 {
                                    cancelled.clear();
                                }
                                cancelled.insert(c.invocation_id);
                            }
                        }
                        Some(cm_pb::session_event::Event::Notification(n)) => {
                            if let Some(sid) = current_session_id.clone() {
                                let ch = channel_for_ack.clone();
                                tokio::spawn(async move {
                                    let mut c = GrpcCodeModeClient::new(ch);
                                    let _ = c
                                        .acknowledge_notification(
                                            cm_pb::AcknowledgeNotificationRequest {
                                                session_id: sid,
                                                notification_id: n.notification_id,
                                            },
                                        )
                                        .await;
                                });
                            }
                        }
                        Some(cm_pb::session_event::Event::CellClosed(closed)) => {
                            cells_for_session.lock().await.remove(&closed.cell_id);
                        }
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(err) => {
                        warn!("codex-code-mode-host session stream ended: {err}");
                        let mut guard = res_clone.lock().await;
                        if guard.is_none() {
                            *guard = Some(Err(err.to_string()));
                            notify_clone.notify_one();
                        }
                        break;
                    }
                }
            }
            {
                let mut guard = res_clone.lock().await;
                if guard.is_none() {
                    *guard = Some(Err(
                        "codex-code-mode-host closed OpenSession before SessionOpened".into(),
                    ));
                    notify_clone.notify_one();
                }
            }
            if let Some(sid) = current_session_id {
                let mut st = state_for_session.lock().await;
                if st.session_id.as_deref() == Some(sid.as_str()) {
                    st.session_id = None;
                    cells_for_session.lock().await.clear();
                }
            }
        });

        if tokio::time::timeout(Duration::from_secs(15), opened_notify.notified())
            .await
            .is_err()
        {
            session_task.abort();
            return Err("Timed out waiting for SessionOpened".to_owned());
        }

        let (session_id, supports_catalog_revision) = match opened_result.lock().await.take() {
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                session_task.abort();
                return Err(e);
            }
            None => {
                session_task.abort();
                return Err("Missing SessionOpened result".to_owned());
            }
        };

        let mut sub_client = GrpcCodeModeClient::new(channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        let mut tool_stream = match sub_client
            .subscribe_to_tool_calls(cm_pb::SubscribeToToolCallsRequest {
                session_id: session_id.clone(),
                tool_names: vec![],
                heartbeat_interval_s: 15,
            })
            .await
        {
            Ok(resp) => resp.into_inner(),
            Err(e) => {
                session_task.abort();
                return Err(format!("SubscribeToToolCalls failed: {e}"));
            }
        };

        let pm = self.process_manager.clone();
        let workdir = self.workdir.clone();
        let ch_for_tools = channel.clone();
        let inflight_map = self.inflight_calls.clone();
        let cancelled_map = self.cancelled_invocations.clone();
        let state_for_sub = self.state.clone();
        let cells_for_sub = self.active_cells.clone();
        let sub_session_id = session_id.clone();

        let subscribe_task = tokio::spawn(async move {
            while let Ok(Some(call)) = tool_stream.message().await {
                if call.heartbeat.is_some() {
                    continue;
                }
                let inv_id = call.invocation_id.clone();
                if cancelled_map.lock().await.remove(&inv_id) {
                    continue;
                }
                let pm_c = pm.clone();
                let wd_c = workdir.clone();
                let ch_c = ch_for_tools.clone();
                let inflight_c = inflight_map.clone();
                let inv_id_done = inv_id.clone();
                let mut guard = inflight_map.lock().await;
                let handle = tokio::spawn(async move {
                    handle_nested_tool_call(call, pm_c, wd_c, ch_c).await;
                    inflight_c.lock().await.remove(&inv_id_done);
                });
                guard.insert(inv_id, handle);
            }
            warn!("SubscribeToToolCalls stream ended for session {sub_session_id}");
            let mut st = state_for_sub.lock().await;
            if st.session_id.as_deref() == Some(sub_session_id.as_str()) {
                st.session_id = None;
                cells_for_sub.lock().await.clear();
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut st = self.state.lock().await;
        st.channel = Some(channel);
        st.session_id = Some(session_id);
        st.supports_catalog_revision = supports_catalog_revision;
        st.catalog_installed = false;
        st.session_task = Some(session_task);
        st.subscribe_task = Some(subscribe_task);
        Ok(())
    }

    async fn ensure_session(&self) -> Result<(Channel, String, bool, bool), String> {
        {
            let st = self.state.lock().await;
            if let (Some(ch), Some(sid)) = (&st.channel, &st.session_id) {
                return Ok((
                    ch.clone(),
                    sid.clone(),
                    st.supports_catalog_revision,
                    st.catalog_installed,
                ));
            }
        }
        self.start().await?;
        let st = self.state.lock().await;
        match (&st.channel, &st.session_id) {
            (Some(ch), Some(sid)) => Ok((
                ch.clone(),
                sid.clone(),
                st.supports_catalog_revision,
                st.catalog_installed,
            )),
            _ => Err("Failed to initialize code-mode session".into()),
        }
    }

    pub async fn execute(
        &self,
        code: &str,
        yield_time_ms: Option<u32>,
        max_output_tokens: Option<u32>,
    ) -> Result<FormattedOutcome, String> {
        let parsed = parse_exec_source(code).map_err(|e| e.to_string())?;
        let effective_yield_ms = parsed
            .yield_time_ms
            .or(yield_time_ms)
            .unwrap_or(DEFAULT_CELL_YIELD_TIME_MS)
            .min(self.max_yield_time_ms);
        let requested_tokens = parsed
            .max_output_tokens
            .or(max_output_tokens.filter(|&v| v > 0))
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let effective_max_tokens = requested_tokens.max(100);

        for attempt in 0..2 {
            let (channel, session_id, supports_rev, catalog_installed) =
                self.ensure_session().await?;
            let reuse_catalog = supports_rev && catalog_installed;

            let request = cm_pb::ExecuteRequest {
                session_id,
                execution_id: format!("exec-{}", &Uuid::new_v4().simple().to_string()[..12]),
                tool_call_id: format!("call-{}", &Uuid::new_v4().simple().to_string()[..12]),
                source: parsed.code.clone(),
                enabled_tools: if reuse_catalog {
                    Vec::new()
                } else {
                    self.nested_tools.clone()
                },
                yield_time_ms: Some(u64::from(effective_yield_ms)),
                max_output_tokens: Some(u64::from(effective_max_tokens)),
                catalog_revision: supports_rev.then_some(1),
            };

            let mut client = GrpcCodeModeClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES);

            match client.execute(request).await {
                Ok(resp) => {
                    let mut stream = resp.into_inner();
                    let mut outcome: Option<cm_pb::ExecutionOutcome> = None;
                    let mut stream_err: Option<tonic::Status> = None;

                    loop {
                        match stream.message().await {
                            Ok(Some(event)) => match event.event {
                                Some(cm_pb::execute_event::Event::Started(started)) => {
                                    self.state.lock().await.catalog_installed = true;
                                    if !started.cell_id.is_empty() {
                                        self.active_cells.lock().await.insert(started.cell_id);
                                    }
                                }
                                Some(cm_pb::execute_event::Event::Outcome(out)) => {
                                    outcome = Some(out);
                                }
                                None => {}
                            },
                            Ok(None) => break,
                            Err(status) => {
                                stream_err = Some(status);
                                break;
                            }
                        }
                    }

                    if let Some(status) = stream_err {
                        if attempt == 0
                            && matches!(
                                status.code(),
                                tonic::Code::NotFound | tonic::Code::Unavailable
                            )
                        {
                            warn!("Reopening code-mode session after gRPC error: {}", status.code());
                            let _guard = self.session_lock.lock().await;
                            self.state.lock().await.session_id = None;
                            continue;
                        }
                        return Err(format!("Execute failed: {status}"));
                    }

                    let out = outcome.ok_or_else(|| {
                        "codex-code-mode-host ended Execute stream without outcome".to_owned()
                    })?;
                    let formatted = format_execution_outcome(&out, false);
                    if matches!(
                        formatted.status.as_str(),
                        "completed" | "terminated" | "missing"
                    ) {
                        self.active_cells.lock().await.remove(&formatted.cell_id);
                    }
                    return Ok(formatted);
                }
                Err(status) => {
                    if attempt == 0
                        && matches!(
                            status.code(),
                            tonic::Code::NotFound | tonic::Code::Unavailable
                        )
                    {
                        warn!("Reopening code-mode session after gRPC error: {}", status.code());
                        let _guard = self.session_lock.lock().await;
                        self.state.lock().await.session_id = None;
                        continue;
                    }
                    return Err(format!("Execute failed: {status}"));
                }
            }
        }

        Err("Failed to execute code after reconnecting".into())
    }

    pub async fn wait(
        &self,
        cell_id: &str,
        yield_time_ms: Option<u32>,
        max_output_tokens: Option<u32>,
        terminate: bool,
    ) -> Result<FormattedOutcome, String> {
        let (channel, session_id, _, _) = self.ensure_session().await?;
        let effective_max_tokens = max_output_tokens
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
            .max(100);

        let mut client = GrpcCodeModeClient::new(channel)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);

        let rpc_res = if terminate {
            client
                .terminate(cm_pb::TerminateRequest {
                    session_id,
                    cell_id: cell_id.to_owned(),
                    max_output_tokens: Some(u64::from(effective_max_tokens)),
                    discard_output: false,
                })
                .await
        } else {
            let effective_yield_ms = yield_time_ms
                .unwrap_or(DEFAULT_CELL_YIELD_TIME_MS)
                .min(self.max_yield_time_ms);
            client
                .wait(cm_pb::WaitRequest {
                    session_id,
                    cell_id: cell_id.to_owned(),
                    wait_id: format!("wait-{}", &Uuid::new_v4().simple().to_string()[..12]),
                    yield_time_ms: u64::from(effective_yield_ms),
                    max_output_tokens: Some(u64::from(effective_max_tokens)),
                })
                .await
        };

        let resp = match rpc_res {
            Ok(r) => r.into_inner(),
            Err(status) => {
                if matches!(
                    status.code(),
                    tonic::Code::NotFound | tonic::Code::Unavailable
                ) {
                    let _guard = self.session_lock.lock().await;
                    self.state.lock().await.session_id = None;
                    self.active_cells.lock().await.clear();
                }
                return Err(format!("Wait/Terminate failed: {status}"));
            }
        };

        let formatted = match resp.state {
            Some(cm_pb::wait_response::State::MissingCell(out)) => {
                self.active_cells.lock().await.remove(cell_id);
                format_execution_outcome(&out, true)
            }
            Some(cm_pb::wait_response::State::LiveCell(out)) => {
                format_execution_outcome(&out, false)
            }
            None => return Err("WaitResponse contained empty state".into()),
        };

        if matches!(
            formatted.status.as_str(),
            "completed" | "terminated" | "missing"
        ) {
            self.active_cells.lock().await.remove(&formatted.cell_id);
        }
        Ok(formatted)
    }

    async fn close_streams_locked(&self) {
        for (_, h) in self.inflight_calls.lock().await.drain() {
            h.abort();
        }
        self.cancelled_invocations.lock().await.clear();
        let mut st = self.state.lock().await;
        if let Some(h) = st.subscribe_task.take() {
            h.abort();
        }
        if let Some(h) = st.session_task.take() {
            h.abort();
        }
        st.channel = None;
        st.session_id = None;
        st.catalog_installed = false;
        self.active_cells.lock().await.clear();
    }

    pub async fn close(&self) {
        let _guard = self.session_lock.lock().await;
        let maybe_close = {
            let st = self.state.lock().await;
            match (&st.channel, &st.session_id) {
                (Some(ch), Some(sid)) => Some((ch.clone(), sid.clone())),
                _ => None,
            }
        };
        if let Some((ch, sid)) = maybe_close {
            let mut client = GrpcCodeModeClient::new(ch);
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                client.close_session(cm_pb::CloseSessionRequest { session_id: sid }),
            )
            .await;
        }
        self.close_streams_locked().await;
        self.process_manager.close_all().await;
    }
}

async fn handle_nested_tool_call(
    call: cm_pb::ToolCall,
    pm: Arc<ProcessManager>,
    workdir: PathBuf,
    channel: Channel,
) {
    let tool_name = call
        .tool_name
        .as_ref()
        .map(|t| t.name.as_str())
        .unwrap_or("");
    let args: serde_json::Value = match &call.input_json {
        Some(bytes) if !bytes.is_empty() => match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(e) => {
                complete_failed(
                    channel,
                    call.session_id,
                    call.invocation_id,
                    format!("Invalid tool input JSON: {e}"),
                )
                .await;
                return;
            }
        },
        _ => json!({}),
    };

    let result: Result<String, String> = match tool_name {
        "exec_command" => {
            let Some(cmd) = args.get("cmd").and_then(|v| v.as_str()) else {
                complete_failed(
                    channel,
                    call.session_id,
                    call.invocation_id,
                    "exec_command requires an object with 'cmd'".into(),
                )
                .await;
                return;
            };
            pm.exec_command(
                cmd,
                args.get("workdir").and_then(|v| v.as_str()),
                args.get("shell").and_then(|v| v.as_str()),
                args.get("login").and_then(|v| v.as_bool()).unwrap_or(true),
                args.get("tty").and_then(|v| v.as_bool()).unwrap_or(false),
                args.get("yield_time_ms").and_then(|v| v.as_u64()),
                args.get("max_output_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize),
            )
            .await
        }
        "write_stdin" => {
            let Some(sid) = args.get("session_id").and_then(|v| v.as_i64()) else {
                complete_failed(
                    channel,
                    call.session_id,
                    call.invocation_id,
                    "write_stdin requires an object with 'session_id'".into(),
                )
                .await;
                return;
            };
            pm.write_stdin(
                sid,
                args.get("chars").and_then(|v| v.as_str()).unwrap_or(""),
                args.get("yield_time_ms").and_then(|v| v.as_u64()),
                args.get("max_output_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize),
            )
            .await
        }
        "apply_patch" => {
            let patch_str = if let Some(s) = args.as_str() {
                s.to_owned()
            } else {
                args.get("patch")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned()
            };
            match tokio::task::spawn_blocking(move || apply_patch(&patch_str, &workdir)).await {
                Ok(Ok(out)) => Ok(out),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(format!("apply_patch task panicked: {e}")),
            }
        }
        other => Err(format!("Unknown nested tool: {other}")),
    };

    let mut client = GrpcCodeModeClient::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);

    match result {
        Ok(output_str) => {
            let output_json = serde_json::to_vec(&output_str).unwrap_or_default();
            let _ = client
                .complete_tool_call(cm_pb::CompleteToolCallRequest {
                    session_id: call.session_id,
                    invocation_id: call.invocation_id,
                    outcome: Some(cm_pb::complete_tool_call_request::Outcome::Succeeded(
                        cm_pb::ToolCallSucceeded { output_json },
                    )),
                })
                .await;
        }
        Err(msg) => {
            let _ = client
                .complete_tool_call(cm_pb::CompleteToolCallRequest {
                    session_id: call.session_id,
                    invocation_id: call.invocation_id,
                    outcome: Some(cm_pb::complete_tool_call_request::Outcome::Failed(
                        cm_pb::ToolCallFailed { message: msg },
                    )),
                })
                .await;
        }
    }
}

async fn complete_failed(channel: Channel, session_id: String, invocation_id: String, message: String) {
    let mut client = GrpcCodeModeClient::new(channel);
    let _ = client
        .complete_tool_call(cm_pb::CompleteToolCallRequest {
            session_id,
            invocation_id,
            outcome: Some(cm_pb::complete_tool_call_request::Outcome::Failed(
                cm_pb::ToolCallFailed { message },
            )),
        })
        .await;
}
