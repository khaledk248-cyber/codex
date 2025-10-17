/*
Runtime: unified exec

Handles approval + sandbox orchestration for unified exec requests, delegating to
the session manager to spawn PTYs once an ExecEnv is prepared.
*/
use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::CodexErr;
use crate::error::SandboxErr;
use crate::tools::runtimes::build_command_spec;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ApprovalDecision;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::SandboxablePreference;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecSession;
use crate::unified_exec::UnifiedExecSessionManager;

#[derive(Clone, Debug)]
pub struct UnifiedExecRequest {
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnifiedExecApprovalKey {
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

pub struct UnifiedExecRuntime<'a> {
    manager: &'a UnifiedExecSessionManager,
}

impl UnifiedExecRequest {
    pub fn new(command: Vec<String>, cwd: PathBuf) -> Self {
        Self { command, cwd }
    }
}

impl<'a> UnifiedExecRuntime<'a> {
    pub fn new(manager: &'a UnifiedExecSessionManager) -> Self {
        Self { manager }
    }
}

impl Sandboxable for UnifiedExecRuntime<'_> {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<UnifiedExecRequest> for UnifiedExecRuntime<'_> {
    type ApprovalKey = UnifiedExecApprovalKey;

    fn approval_key(&self, req: &UnifiedExecRequest) -> Self::ApprovalKey {
        UnifiedExecApprovalKey {
            command: req.command.clone(),
            cwd: req.cwd.clone(),
        }
    }

    fn start_approval_async<'b>(
        &'b mut self,
        req: &'b UnifiedExecRequest,
        ctx: ApprovalCtx<'b>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalDecision> + Send + 'b>> {
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

impl<'a> ToolRuntime<UnifiedExecRequest, UnifiedExecSession> for UnifiedExecRuntime<'a> {
    async fn run(
        &mut self,
        req: &UnifiedExecRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx<'_>,
    ) -> Result<UnifiedExecSession, ToolError> {
        let empty_env = HashMap::new();
        let spec = build_command_spec(&req.command, &req.cwd, &empty_env, None, None, None)
            .map_err(|_| ToolError::Rejected("missing command line for PTY".to_string()))?;
        let exec_env = attempt
            .env_for(&spec)
            .map_err(|err| ToolError::Codex(err.into()))?;
        self.manager
            .open_session_with_exec_env(&exec_env)
            .await
            .map_err(|err| match err {
                UnifiedExecError::SandboxDenied { output, .. } => {
                    ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                        output: Box::new(output),
                    }))
                }
                other => ToolError::Rejected(other.to_string()),
            })
    }
}
