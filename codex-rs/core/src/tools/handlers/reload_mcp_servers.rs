use async_trait::async_trait;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct ReloadMcpServersHandler;

#[async_trait]
impl ToolHandler for ReloadMcpServersHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "reload_mcp_servers handler received unsupported payload".to_string(),
                ));
            }
        };

        if !arguments.trim().is_empty() {
            let _: serde_json::Value = parse_arguments(&arguments)?;
        }

        let configured_server_count = session.reload_mcp_servers_from_config(turn.as_ref()).await;
        let body = serde_json::json!({
            "status": "ok",
            "configured_server_count": configured_server_count,
            "message": "Reloaded MCP servers from config.toml and reconnected active MCP sessions.",
        })
        .to_string();

        Ok(FunctionToolOutput::from_text(body, Some(true)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::make_session_and_context;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn invocation(
        session: Arc<crate::codex::Session>,
        turn: Arc<crate::codex::TurnContext>,
        payload: ToolPayload,
    ) -> ToolInvocation {
        ToolInvocation {
            session,
            turn,
            tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            call_id: "call-1".to_string(),
            tool_name: "reload_mcp_servers".to_string(),
            tool_namespace: None,
            payload,
        }
    }

    #[tokio::test]
    async fn handler_rejects_non_function_payload() {
        let (session, turn) = make_session_and_context().await;
        let invocation = invocation(
            Arc::new(session),
            Arc::new(turn),
            ToolPayload::Custom {
                input: "reload".to_string(),
            },
        );
        let Err(err) = ReloadMcpServersHandler.handle(invocation).await else {
            panic!("payload should be rejected");
        };
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "reload_mcp_servers handler received unsupported payload".to_string()
            )
        );
    }

    #[tokio::test]
    async fn handler_reloads_mcp_servers_from_config_immediately() {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let invocation = invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );

        let output = ReloadMcpServersHandler
            .handle(invocation)
            .await
            .expect("reload handler should succeed");

        assert_eq!(output.success, Some(true));
        let parsed: serde_json::Value =
            serde_json::from_str(&output.into_text()).expect("reload tool should return JSON text");
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["configured_server_count"], 0);
    }
}
