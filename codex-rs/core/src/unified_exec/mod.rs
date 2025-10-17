//! Unified Exec: interactive PTY execution orchestrated with approvals + sandboxing.
//!
//! Responsibilities
//! - Manages interactive PTY sessions (create, reuse, buffer output with caps).
//! - Uses the shared ToolOrchestrator to handle approval, sandbox selection, and
//!   retry semantics in a single, descriptive flow.
//! - Spawns the PTY from a sandbox‑transformed `ExecEnv`; on sandbox denial,
//!   retries without sandbox when policy allows (no re‑prompt thanks to caching).
//! - Uses the shared `is_likely_sandbox_denied` heuristic to keep denial messages
//!   consistent with other exec paths.
//!
//! Flow at a glance (open session)
//! 1) Build a small request `{ command, cwd }`.
//! 2) Orchestrator: approval (bypass/cache/prompt) → select sandbox → run.
//! 3) Runtime: transform `CommandSpec` → `ExecEnv` → spawn PTY.
//! 4) If denial, orchestrator retries with `SandboxType::None`.
//! 5) Session is returned with streaming output + metadata.
//!
//! This keeps policy logic and user interaction centralized while the PTY/session
//! concerns remain isolated here.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio::time::Instant;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::exec::ExecToolCallOutput;
use crate::exec::SandboxType;
use crate::exec::StreamOutput;
use crate::exec::is_likely_sandbox_denied;
use codex_utils_pty::ExecCommandSession;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::sandboxing::{
    Approvable,
    ApprovalCtx,
    ApprovalDecision,
    SandboxAttempt,
    Sandboxable,
    SandboxablePreference,
    ToolCtx,
    ToolError,
    ToolRuntime,
};
use codex_utils_pty::SpawnedPty;
use crate::sandboxing::ExecEnv;
use crate::truncate::truncate_middle;

mod errors;

pub(crate) use errors::UnifiedExecError;

const DEFAULT_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 128 * 1024; // 128 KiB

pub(crate) struct UnifiedExecContext<'a> {
    pub session: &'a Session,
    pub turn: &'a TurnContext,
    pub sub_id: &'a str,
    pub call_id: &'a str,
    pub session_id: Option<i32>,
}

#[derive(Debug)]
pub(crate) struct UnifiedExecRequest<'a> {
    pub input_chunks: &'a [String],
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UnifiedExecResult {
    pub session_id: Option<i32>,
    pub output: String,
}

#[derive(Debug, Default)]
pub(crate) struct UnifiedExecSessionManager {
    next_session_id: AtomicI32,
    sessions: Mutex<HashMap<i32, UnifiedExecSession>>,
}

#[derive(Debug)]
/// Wraps a PTY session with buffered output and sandbox metadata for unified exec.
struct UnifiedExecSession {
    session: ExecCommandSession,
    output_buffer: OutputBuffer,
    /// Notifies waiters whenever new output has been appended to
    /// `output_buffer`, allowing clients to poll for fresh data.
    output_notify: Arc<Notify>,
    output_task: JoinHandle<()>,
    sandbox_type: SandboxType,
}

#[derive(Debug, Default)]
struct OutputBufferState {
    chunks: VecDeque<Vec<u8>>,
    total_bytes: usize,
}

impl OutputBufferState {
    fn push_chunk(&mut self, chunk: Vec<u8>) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());
        self.chunks.push_back(chunk);

        let mut excess = self
            .total_bytes
            .saturating_sub(UNIFIED_EXEC_OUTPUT_MAX_BYTES);

        while excess > 0 {
            match self.chunks.front_mut() {
                Some(front) if excess >= front.len() => {
                    excess -= front.len();
                    self.total_bytes = self.total_bytes.saturating_sub(front.len());
                    self.chunks.pop_front();
                }
                Some(front) => {
                    front.drain(..excess);
                    self.total_bytes = self.total_bytes.saturating_sub(excess);
                    break;
                }
                None => break,
            }
        }
    }

    fn drain(&mut self) -> Vec<Vec<u8>> {
        let drained: Vec<Vec<u8>> = self.chunks.drain(..).collect();
        self.total_bytes = 0;
        drained
    }

    fn snapshot(&self) -> Vec<Vec<u8>> {
        self.chunks.iter().cloned().collect()
    }
}

type OutputBuffer = Arc<Mutex<OutputBufferState>>;
type OutputHandles = (OutputBuffer, Arc<Notify>);

impl UnifiedExecSession {
    fn new(
        session: ExecCommandSession,
        initial_output_rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
        sandbox_type: SandboxType,
    ) -> Self {
        let output_buffer = Arc::new(Mutex::new(OutputBufferState::default()));
        let output_notify = Arc::new(Notify::new());
        let mut receiver = initial_output_rx;
        let buffer_clone = Arc::clone(&output_buffer);
        let notify_clone = Arc::clone(&output_notify);
        let output_task = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(chunk) => {
                        let mut guard = buffer_clone.lock().await;
                        guard.push_chunk(chunk);
                        drop(guard);
                        notify_clone.notify_waiters();
                    }
                    // If we lag behind the broadcast buffer, skip missed
                    // messages but keep the task alive to continue streaming.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    // When the sender closes, exit the task.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Self {
            session,
            output_buffer,
            output_notify,
            output_task,
            sandbox_type,
        }
    }

    fn writer_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.session.writer_sender()
    }

    fn output_handles(&self) -> OutputHandles {
        (
            Arc::clone(&self.output_buffer),
            Arc::clone(&self.output_notify),
        )
    }

    fn has_exited(&self) -> bool {
        self.session.has_exited()
    }

    fn exit_code(&self) -> Option<i32> {
        self.session.exit_code()
    }

    async fn snapshot_output(&self) -> Vec<Vec<u8>> {
        let guard = self.output_buffer.lock().await;
        guard.snapshot()
    }

    fn sandbox_type(&self) -> SandboxType {
        self.sandbox_type
    }

    async fn check_for_sandbox_denial(&self) -> Result<(), UnifiedExecError> {
        if self.sandbox_type() == SandboxType::None || !self.has_exited() {
            return Ok(());
        }

        // Give the reader task a brief moment to flush any final PTY bytes after exit.
        let _ =
            tokio::time::timeout(Duration::from_millis(20), self.output_notify.notified()).await;

        let collected_chunks = self.snapshot_output().await;
        let mut aggregated: Vec<u8> = Vec::new();
        for chunk in collected_chunks {
            aggregated.extend_from_slice(&chunk);
        }
        let aggregated_text = String::from_utf8_lossy(&aggregated).to_string();
        let exit_code = self.exit_code().unwrap_or(-1);

        let exec_output = ExecToolCallOutput {
            exit_code,
            stdout: StreamOutput::new(aggregated_text.clone()),
            stderr: StreamOutput::new(String::new()),
            aggregated_output: StreamOutput::new(aggregated_text.clone()),
            duration: Duration::ZERO,
            timed_out: false,
        };

        if is_likely_sandbox_denied(self.sandbox_type(), &exec_output) {
            let (snippet, _) = truncate_middle(&aggregated_text, UNIFIED_EXEC_OUTPUT_MAX_BYTES);
            let message = if snippet.is_empty() {
                format!("exit code {exit_code}")
            } else {
                snippet
            };
            return Err(UnifiedExecError::sandbox_denied(message));
        }

        Ok(())
    }

    async fn from_spawned(
        spawned: SpawnedPty,
        sandbox_type: SandboxType,
    ) -> Result<Self, UnifiedExecError> {
        let SpawnedPty {
            session,
            output_rx,
            mut exit_rx,
        } = spawned;
        let managed = Self::new(session, output_rx, sandbox_type);

        let exit_ready = match exit_rx.try_recv() {
            Ok(_) | Err(TryRecvError::Closed) => true,
            Err(TryRecvError::Empty) => false,
        };

        if exit_ready {
            managed.check_for_sandbox_denial().await?;
        }

        Ok(managed)
    }
}

impl Drop for UnifiedExecSession {
    fn drop(&mut self) {
        self.output_task.abort();
    }
}

impl UnifiedExecSessionManager {
    async fn open_session_with_exec_env(
        &self,
        env: &ExecEnv,
    ) -> Result<UnifiedExecSession, UnifiedExecError> {
        let (program, args) = env
            .command
            .split_first()
            .ok_or(UnifiedExecError::MissingCommandLine)?;
        let spawned = codex_utils_pty::spawn_pty_process(program, args, &env.env)
            .await
            .map_err(|err| UnifiedExecError::create_session(err.to_string()))?;
        UnifiedExecSession::from_spawned(spawned, env.sandbox).await
    }

    async fn open_session_with_sandbox(
        &self,
        command: Vec<String>,
        context: &UnifiedExecContext<'_>,
    ) -> Result<UnifiedExecSession, UnifiedExecError> {
        // Orchestrated open: approval + sandbox selection + retry
        struct OpenShellRuntime<'a> {
            manager: &'a UnifiedExecSessionManager,
        }
        #[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
        struct ApprovalKey {
            command: Vec<String>,
            cwd: std::path::PathBuf,
        }
        struct OpenShellReq {
            command: Vec<String>,
            cwd: std::path::PathBuf,
        }
        impl<'a> OpenShellRuntime<'a> {
            fn build_command_spec(
                req: &OpenShellReq,
            ) -> Result<crate::sandboxing::CommandSpec, ToolError> {
                let env = HashMap::new();
                match crate::runtime::command_spec::build_command_spec(
                    &req.command,
                    &req.cwd,
                    &env,
                    None,
                    None,
                    None,
                ) {
                    Ok(spec) => Ok(spec),
                    Err(_) => Err(ToolError::Rejected(
                        "missing command line for PTY".to_string(),
                    )),
                }
            }
        }
        impl Sandboxable for OpenShellRuntime<'_> {
            fn sandbox_preference(&self) -> SandboxablePreference {
                SandboxablePreference::Auto
            }
            fn escalate_on_failure(&self) -> bool {
                true
            }
        }
        impl Approvable<OpenShellReq> for OpenShellRuntime<'_> {
            type ApprovalKey = ApprovalKey;
            fn approval_key(&self, req: &OpenShellReq) -> ApprovalKey {
                ApprovalKey {
                    command: req.command.clone(),
                    cwd: req.cwd.clone(),
                }
            }
            fn reset_cache(&mut self) {}
            fn start_approval_async<'b>(
                &'b mut self,
                req: &'b OpenShellReq,
                ctx: ApprovalCtx<'b>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalDecision> + Send + 'b>>
            {
                let reason = ctx.retry_reason.clone();
                Box::pin(async move {
                    ctx.session
                        .request_command_approval(
                            ctx.sub_id.to_string(),
                            ctx.call_id.to_string(),
                            req.command.clone(),
                            req.cwd.clone(),
                            reason,
                        )
                        .await
                        .into()
                })
            }
        }
        impl<'a> ToolRuntime<OpenShellReq, UnifiedExecSession> for OpenShellRuntime<'a> {
            async fn run(
                &mut self,
                req: &OpenShellReq,
                attempt: &SandboxAttempt<'_>,
                _ctx: &ToolCtx<'_>,
            ) -> Result<UnifiedExecSession, ToolError> {
                let spec = Self::build_command_spec(req)?;
                let env = attempt
                    .env_for(&spec)
                    .map_err(|err| ToolError::Codex(err.into()))?;
                self.manager
                    .open_session_with_exec_env(&env)
                    .await
                    .map_err(|e| ToolError::Rejected(e.to_string()))
            }
        }

        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime = OpenShellRuntime { manager: self };
        let req = OpenShellReq {
            command,
            cwd: context.turn.cwd.clone(),
        };
        let tool_ctx = ToolCtx {
            session: context.session,
            sub_id: context.sub_id.to_string(),
            call_id: context.call_id.to_string(),
        };
        orchestrator
            .run(
                &mut runtime,
                &req,
                &tool_ctx,
                context.turn,
                context.turn.approval_policy,
            )
            .await
            .map_err(|e| UnifiedExecError::create_session(format!("{e:?}")))
    }

    pub async fn handle_request(
        &self,
        request: UnifiedExecRequest<'_>,
        context: UnifiedExecContext<'_>,
    ) -> Result<UnifiedExecResult, UnifiedExecError> {
        let (timeout_ms, timeout_warning) = match request.timeout_ms {
            Some(requested) if requested > MAX_TIMEOUT_MS => (
                MAX_TIMEOUT_MS,
                Some(format!(
                    "Warning: requested timeout {requested}ms exceeds maximum of {MAX_TIMEOUT_MS}ms; clamping to {MAX_TIMEOUT_MS}ms.\n"
                )),
            ),
            Some(requested) => (requested, None),
            None => (DEFAULT_TIMEOUT_MS, None),
        };

        let mut new_session: Option<UnifiedExecSession> = None;
        let session_id;
        let writer_tx;
        let output_buffer;
        let output_notify;

        if let Some(existing_id) = context.session_id {
            let mut sessions = self.sessions.lock().await;
            match sessions.get(&existing_id) {
                Some(session) => {
                    if session.has_exited() {
                        sessions.remove(&existing_id);
                        return Err(UnifiedExecError::UnknownSessionId {
                            session_id: existing_id,
                        });
                    }
                    let (buffer, notify) = session.output_handles();
                    session_id = existing_id;
                    writer_tx = session.writer_sender();
                    output_buffer = buffer;
                    output_notify = notify;
                }
                None => {
                    return Err(UnifiedExecError::UnknownSessionId {
                        session_id: existing_id,
                    });
                }
            }
            drop(sessions);
        } else {
            let command = request.input_chunks.to_vec();
            let new_id = self.next_session_id.fetch_add(1, Ordering::SeqCst);
            let managed_session = self.open_session_with_sandbox(command, &context).await?;
            let (buffer, notify) = managed_session.output_handles();
            writer_tx = managed_session.writer_sender();
            output_buffer = buffer;
            output_notify = notify;
            session_id = new_id;
            new_session = Some(managed_session);
        };

        if context.session_id.is_some() {
            let mut trailing_whitespace = true;
            for chunk in request.input_chunks {
                if chunk.is_empty() {
                    continue;
                }

                let leading_whitespace = chunk
                    .chars()
                    .next()
                    .map(char::is_whitespace)
                    .unwrap_or(true);

                if !trailing_whitespace
                    && !leading_whitespace
                    && writer_tx.send(vec![b' ']).await.is_err()
                {
                    return Err(UnifiedExecError::WriteToStdin);
                }

                if writer_tx.send(chunk.as_bytes().to_vec()).await.is_err() {
                    return Err(UnifiedExecError::WriteToStdin);
                }

                trailing_whitespace = chunk
                    .chars()
                    .next_back()
                    .map(char::is_whitespace)
                    .unwrap_or(trailing_whitespace);
            }
        }

        let mut collected: Vec<u8> = Vec::with_capacity(4096);
        let start = Instant::now();
        let deadline = start + Duration::from_millis(timeout_ms);

        loop {
            let drained_chunks;
            let mut wait_for_output = None;
            {
                let mut guard = output_buffer.lock().await;
                drained_chunks = guard.drain();
                if drained_chunks.is_empty() {
                    wait_for_output = Some(output_notify.notified());
                }
            }

            if drained_chunks.is_empty() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining == Duration::ZERO {
                    break;
                }

                let notified = wait_for_output.unwrap_or_else(|| output_notify.notified());
                tokio::pin!(notified);
                tokio::select! {
                    _ = &mut notified => {}
                    _ = tokio::time::sleep(remaining) => break,
                }
                continue;
            }

            for chunk in drained_chunks {
                collected.extend_from_slice(&chunk);
            }

            if Instant::now() >= deadline {
                break;
            }
        }

        let (output, _maybe_tokens) = truncate_middle(
            &String::from_utf8_lossy(&collected),
            UNIFIED_EXEC_OUTPUT_MAX_BYTES,
        );
        let output = if let Some(warning) = timeout_warning {
            format!("{warning}{output}")
        } else {
            output
        };

        let should_store_session = if let Some(session) = new_session.as_ref() {
            !session.has_exited()
        } else if context.session_id.is_some() {
            let mut sessions = self.sessions.lock().await;
            if let Some(existing) = sessions.get(&session_id) {
                if existing.has_exited() {
                    sessions.remove(&session_id);
                    false
                } else {
                    true
                }
            } else {
                false
            }
        } else {
            true
        };

        if should_store_session {
            if let Some(session) = new_session {
                self.sessions.lock().await.insert(session_id, session);
            }
            Ok(UnifiedExecResult {
                session_id: Some(session_id),
                output,
            })
        } else {
            Ok(UnifiedExecResult {
                session_id: None,
                output,
            })
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    use crate::codex::Session;
    use crate::codex::TurnContext;
    use crate::codex::make_session_and_context;
    use crate::protocol::AskForApproval;
    use crate::protocol::SandboxPolicy;
    use core_test_support::skip_if_sandbox;
    use std::sync::Arc;

    fn test_session_and_turn() -> (Arc<Session>, Arc<TurnContext>) {
        let (session, mut turn) = make_session_and_context();
        turn.approval_policy = AskForApproval::Never;
        turn.sandbox_policy = SandboxPolicy::DangerFullAccess;
        (Arc::new(session), Arc::new(turn))
    }

    async fn run_unified_exec_request(
        session: &Arc<Session>,
        turn: &Arc<TurnContext>,
        session_id: Option<i32>,
        input: Vec<String>,
        timeout_ms: Option<u64>,
    ) -> Result<UnifiedExecResult, UnifiedExecError> {
        let request_input = input;
        let request = UnifiedExecRequest {
            input_chunks: &request_input,
            timeout_ms,
        };

        session
            .services
            .unified_exec_manager
            .handle_request(
                request,
                UnifiedExecContext {
                    session,
                    turn: turn.as_ref(),
                    sub_id: "sub",
                    call_id: "call",
                    session_id,
                },
            )
            .await
    }

    #[test]
    fn push_chunk_trims_only_excess_bytes() {
        let mut buffer = OutputBufferState::default();
        buffer.push_chunk(vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES]);
        buffer.push_chunk(vec![b'b']);
        buffer.push_chunk(vec![b'c']);

        assert_eq!(buffer.total_bytes, UNIFIED_EXEC_OUTPUT_MAX_BYTES);
        assert_eq!(buffer.chunks.len(), 3);
        assert_eq!(
            buffer.chunks.front().unwrap().len(),
            UNIFIED_EXEC_OUTPUT_MAX_BYTES - 2
        );
        assert_eq!(buffer.chunks.pop_back().unwrap(), vec![b'c']);
        assert_eq!(buffer.chunks.pop_back().unwrap(), vec![b'b']);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unified_exec_persists_across_requests_jif() -> Result<(), UnifiedExecError> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let open_shell = run_unified_exec_request(
            &session,
            &turn,
            None,
            vec!["bash".to_string(), "-i".to_string()],
            Some(2_500),
        )
        .await?;
        let session_id = open_shell.session_id.expect("expected session_id");

        run_unified_exec_request(
            &session,
            &turn,
            Some(session_id),
            vec![
                "export".to_string(),
                "CODEX_INTERACTIVE_SHELL_VAR=codex\n".to_string(),
            ],
            Some(2_500),
        )
        .await?;

        let out_2 = run_unified_exec_request(
            &session,
            &turn,
            Some(session_id),
            vec!["echo $CODEX_INTERACTIVE_SHELL_VAR\n".to_string()],
            Some(2_500),
        )
        .await?;
        assert!(out_2.output.contains("codex"));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_unified_exec_sessions() -> Result<(), UnifiedExecError> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let shell_a = run_unified_exec_request(
            &session,
            &turn,
            None,
            vec!["/bin/bash".to_string(), "-i".to_string()],
            Some(2_500),
        )
        .await?;
        let session_a = shell_a.session_id.expect("expected session id");

        run_unified_exec_request(
            &session,
            &turn,
            Some(session_a),
            vec!["export CODEX_INTERACTIVE_SHELL_VAR=codex\n".to_string()],
            Some(2_500),
        )
        .await?;

        let out_2 = run_unified_exec_request(
            &session,
            &turn,
            None,
            vec![
                "echo".to_string(),
                "$CODEX_INTERACTIVE_SHELL_VAR\n".to_string(),
            ],
            Some(2_500),
        )
        .await?;
        assert!(!out_2.output.contains("codex"));

        let out_3 = run_unified_exec_request(
            &session,
            &turn,
            Some(session_a),
            vec!["echo $CODEX_INTERACTIVE_SHELL_VAR\n".to_string()],
            Some(2_500),
        )
        .await?;
        assert!(out_3.output.contains("codex"));

        Ok(())
    }

    #[tokio::test]
    async fn unified_exec_timeouts() -> Result<(), UnifiedExecError> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let open_shell = run_unified_exec_request(
            &session,
            &turn,
            None,
            vec!["bash".to_string(), "-i".to_string()],
            Some(2_500),
        )
        .await?;
        let session_id = open_shell.session_id.expect("expected session id");

        run_unified_exec_request(
            &session,
            &turn,
            Some(session_id),
            vec![
                "export".to_string(),
                "CODEX_INTERACTIVE_SHELL_VAR=codex\n".to_string(),
            ],
            Some(2_500),
        )
        .await?;

        let out_2 = run_unified_exec_request(
            &session,
            &turn,
            Some(session_id),
            vec!["sleep 5 && echo $CODEX_INTERACTIVE_SHELL_VAR\n".to_string()],
            Some(10),
        )
        .await?;
        assert!(!out_2.output.contains("codex"));

        tokio::time::sleep(Duration::from_secs(7)).await;

        let out_3 =
            run_unified_exec_request(&session, &turn, Some(session_id), Vec::new(), Some(100))
                .await?;

        assert!(out_3.output.contains("codex"));

        Ok(())
    }

    #[tokio::test]
    #[ignore] // Ignored while we have a better way to test this.
    async fn requests_with_large_timeout_are_capped() -> Result<(), UnifiedExecError> {
        let (session, turn) = test_session_and_turn();

        let result = run_unified_exec_request(
            &session,
            &turn,
            None,
            vec!["echo".to_string(), "codex".to_string()],
            Some(120_000),
        )
        .await?;

        assert!(result.output.starts_with(
            "Warning: requested timeout 120000ms exceeds maximum of 60000ms; clamping to 60000ms.\n"
        ));
        assert!(result.output.contains("codex"));

        Ok(())
    }

    #[tokio::test]
    #[ignore] // Ignored while we have a better way to test this.
    async fn completed_commands_do_not_persist_sessions() -> Result<(), UnifiedExecError> {
        let (session, turn) = test_session_and_turn();
        let result = run_unified_exec_request(
            &session,
            &turn,
            None,
            vec!["/bin/echo".to_string(), "codex".to_string()],
            Some(2_500),
        )
        .await?;

        assert!(result.session_id.is_none());
        assert!(result.output.contains("codex"));

        assert!(
            session
                .services
                .unified_exec_manager
                .sessions
                .lock()
                .await
                .is_empty()
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reusing_completed_session_returns_unknown_session() -> Result<(), UnifiedExecError> {
        skip_if_sandbox!(Ok(()));

        let (session, turn) = test_session_and_turn();

        let open_shell = run_unified_exec_request(
            &session,
            &turn,
            None,
            vec!["/bin/bash".to_string(), "-i".to_string()],
            Some(2_500),
        )
        .await?;
        let session_id = open_shell.session_id.expect("expected session id");

        run_unified_exec_request(
            &session,
            &turn,
            Some(session_id),
            vec!["exit\n".to_string()],
            Some(2_500),
        )
        .await?;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let err =
            run_unified_exec_request(&session, &turn, Some(session_id), Vec::new(), Some(100))
                .await
                .expect_err("expected unknown session error");

        match err {
            UnifiedExecError::UnknownSessionId { session_id: err_id } => {
                assert_eq!(err_id, session_id);
            }
            other => panic!("expected UnknownSessionId, got {other:?}"),
        }

        assert!(
            !session
                .services
                .unified_exec_manager
                .sessions
                .lock()
                .await
                .contains_key(&session_id)
        );

        Ok(())
    }
}
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::SandboxablePreference;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
