use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use async_trait::async_trait;

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

        let arguments = arguments.trim();
        if !arguments.is_empty() && arguments != "{}" {
            return Err(FunctionCallError::RespondToModel(
                "reload_mcp_servers does not accept any arguments".to_string(),
            ));
        }

        let configured_server_count = session.reload_mcp_servers_from_config(turn.as_ref()).await;
        Ok(FunctionToolOutput::from_text(
            format!("reloaded {configured_server_count} MCP server(s)"),
            Some(true),
        ))
    }
}
