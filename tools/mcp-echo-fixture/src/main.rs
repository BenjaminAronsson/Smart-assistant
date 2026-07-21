#![deny(unsafe_code)]
//! A trivial MCP tool server used as a **test fixture** for the Jarvis MCP host
//! (F2.7, docs/06 §5). It speaks MCP over stdio and exports three tools:
//!
//! * `echo` — returns its `message` argument as text (annotated read-only).
//! * `read` — returns a fixed canned document (annotated **destructive**, a
//!   deliberate lie: the host must classify it by its own policy, not this hint).
//! * `danger` — returns a marker string, annotated **read-only / non-destructive**
//!   (also a lie). The host has no policy entry for it, so it must be **dropped**
//!   on import — a server cannot introduce a tool by claiming it is safe.
//!
//! The mismatched annotations are the point: they let the host adapter's tests
//! prove that host-owned policy — not the server's self-declared safety — decides
//! which tools exist and how risky each one is (invariant #1).
//!
//! This is not a shipping tool and performs no real side effects.

use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};

const CANNED_DOCUMENT: &str = "fixture document contents";

#[derive(Clone)]
struct EchoFixture;

impl EchoFixture {
    fn tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "echo",
                "Echo the message argument back",
                object_schema("message"),
            )
            .with_annotations(ToolAnnotations::new().read_only(true)),
            // Claims destructive; the host reclassifies it as an R0 read.
            Tool::new("read", "Return a canned document", object_schema("path"))
                .with_annotations(ToolAnnotations::new().read_only(false).destructive(true)),
            // Claims perfectly safe; the host drops it (no policy entry).
            Tool::new("danger", "Unsanctioned tool", empty_schema())
                .with_annotations(ToolAnnotations::new().read_only(true).destructive(false)),
        ]
    }
}

impl ServerHandler for EchoFixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("echo/read test fixture")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(Self::tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "echo" => {
                let message = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("message"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ErrorData::invalid_params("echo requires a string `message`", None)
                    })?;
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    message.to_owned(),
                )]))
            }
            "read" => Ok(CallToolResult::success(vec![ContentBlock::text(
                CANNED_DOCUMENT,
            )])),
            "danger" => Ok(CallToolResult::success(vec![ContentBlock::text(
                "danger executed",
            )])),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool `{other}`"),
                None,
            )),
        }
    }
}

/// A minimal `{ "type": "object", "properties": { <field>: { "type": "string" } } }`
/// JSON Schema for a single optional string field.
fn object_schema(field: &str) -> Arc<JsonObject> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { field: { "type": "string" } },
    });
    Arc::new(schema.as_object().expect("schema is an object").clone())
}

fn empty_schema() -> Arc<JsonObject> {
    let schema = serde_json::json!({ "type": "object", "properties": {} });
    Arc::new(schema.as_object().expect("schema is an object").clone())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Serve MCP over this process's stdio; the Jarvis host is the client.
    let service = EchoFixture
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}
