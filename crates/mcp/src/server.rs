//! The MCP server: streamable HTTP at `/mcp`, eight tools, no WAHA credential.
//!
//! The pool is opened before the listener binds and the shipped migrations are asserted
//! present — never applied. This process runs as a role with no DDL rights, and a
//! schema it cannot create is a schema it must not pretend to have.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use sqlx::postgres::PgPool;
use whatsapp_core::config::McpSettings;
use whatsapp_core::db::{DbError, assert_migrations, create_pool, ping};

use crate::tools::{self, ToolError};

/// The Python server's instructions, with its closing paragraph rewritten.
///
/// The original ends "Message ids are shown in brackets and are what `message`, `react`
/// and `reply_to` take. Sending, reactions and session tools act on the live account."
/// Those tools reach WAHA, this process cannot, and instructions that advertise a tool
/// the server does not serve are worse than none. Everything above that paragraph is
/// byte-identical, hard wrap included.
pub const INSTRUCTIONS: &str = "\
WhatsApp, from a permanent archive in PostgreSQL. Timestamps are ISO 8601 UTC.

History, search and catch-up read the archive, which reaches further back than
WhatsApp's own rolling cache. Voice notes are transcribed and their text is searchable
alongside typed messages.

People are matched on alias, address-book name, pushname or phone number. Many
contacts have no address-book name at all, so call `people` first when a name is
ambiguous, and `set-alias` to record the name someone is actually called.

Message ids are shown in brackets and are what `message` and `revisions` take. This
server reads the archive and writes nothing but aliases; it cannot send a message or
ask WhatsApp anything the ingester has not already stored.
";

/// Tool names are hyphenated and declared explicitly: MCPHub joins them to the server
/// name with `-`, so these read `whatsapp-set-alias` rather than
/// `whatsapp-whatsapp_set_alias`, which would stutter and mix both separators. The Rust
/// method names below are incidental.
#[derive(Clone)]
pub struct WhatsAppMcp {
    pool: PgPool,
}

/// Turn a tool's answer into a protocol response.
///
/// A usage error is the caller's and goes back as `invalid_params` with its own text —
/// where the Python SDK renders `Error executing tool messages` and drops the reason. A
/// SQL error is ours: it is logged in full and the caller is told only that the read
/// failed, because a Postgres error string can carry the DSN and the data.
fn finish(result: std::result::Result<String, ToolError>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(body) => Ok(CallToolResult::success(vec![ContentBlock::text(body)])),
        Err(ToolError::Usage(message)) => Err(ErrorData::invalid_params(message, None)),
        Err(ToolError::Sql(error)) => {
            tracing::error!(%error, "the archive could not be read");
            Err(ErrorData::internal_error(
                "the archive could not be read".to_string(),
                None,
            ))
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ChatsArgs {
    pub limit: Option<i64>,
    pub since: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessagesArgs {
    pub chat_id: String,
    pub limit: Option<i64>,
    pub after: Option<String>,
    pub before: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessageArgs {
    pub chat_id: String,
    pub message_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    pub person: Option<String>,
    pub chat_id: Option<String>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PeopleArgs {
    pub name: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetAliasArgs {
    pub alias: String,
    pub participant_ids: Option<Vec<String>>,
    pub note: Option<String>,
}

// The annotations are declared so a client can tell a read from a write without parsing
// a description. A tool with no hints is assumed able to change things, so an
// unannotated read looks no safer than a write — and ChatGPT blocked read calls before
// they left.
#[tool_router]
impl WhatsAppMcp {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    #[tool(
        name = "chats",
        description = "List chats with message counts and last activity, newest first. \
                       `since` (ISO 8601 or a Unix epoch) narrows to chats active since then \
                       and counts only what arrived after it - the way to catch up.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn chats(
        &self,
        Parameters(args): Parameters<ChatsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::chats(&self.pool, args.limit, args.since.as_deref()).await)
    }

    #[tool(
        name = "messages",
        description = "Read one chat's archived messages, oldest first. With `after` you get the \
                       oldest `limit` messages after it, so you read a period forward from its \
                       start; without `after` you get the newest `limit`, and `before` walks back \
                       from there. `after`/`before` accept ISO 8601 or a Unix epoch. When the \
                       period holds more than `limit`, the reply ends with how many more there \
                       are and the exact `after` (or `before`) to pass for the next page.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn messages(
        &self,
        Parameters(args): Parameters<MessagesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::messages(
                &self.pool,
                &args.chat_id,
                args.limit,
                args.after.as_deref(),
                args.before.as_deref(),
            )
            .await,
        )
    }

    #[tool(
        name = "message",
        description = "Read a single archived message by id.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn message(
        &self,
        Parameters(args): Parameters<MessageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::message(&self.pool, &args.chat_id, &args.message_id).await)
    }

    #[tool(
        name = "revisions",
        description = "The edit history of one message, oldest first, and the deleted text if it \
                       was deleted. Other tools mark a message `(edited)` or `[deleted]` and \
                       withhold the earlier and deleted text; this is where to read it. \
                       Revision 1 is the earliest text this archive saw, which is not \
                       necessarily what was originally sent.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn revisions(
        &self,
        Parameters(args): Parameters<MessageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::revisions(&self.pool, &args.chat_id, &args.message_id).await)
    }

    #[tool(
        name = "search",
        description = "Full-text search across message bodies and voice-note transcripts, best \
                       match first. `person` narrows by sender and matches an alias, \
                       address-book name, pushname or phone number. `after`/`before` (ISO 8601 \
                       or a Unix epoch) narrow it to a period, which is what to use when the \
                       question is about a particular month rather than about all time. Deleted \
                       messages are not searchable; their text is available through `revisions`.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::search(
                &self.pool,
                &args.query,
                args.person.as_deref(),
                args.chat_id.as_deref(),
                args.after.as_deref(),
                args.before.as_deref(),
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "people",
        description = "Resolve a name fragment to people, showing every name form held for each \
                       so an ambiguous match is visible rather than silently picked. Each result \
                       carries the chat id to pass to `messages` where a direct chat exists.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn people(
        &self,
        Parameters(args): Parameters<PeopleArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::people(&self.pool, &args.name).await)
    }

    #[tool(
        name = "status",
        description = "Ingester health: last run, last error, corpus size, transcript backlog.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn status(&self, Parameters(_): Parameters<NoArgs>) -> Result<CallToolResult, ErrorData> {
        finish(tools::status(&self.pool).await)
    }

    #[tool(
        name = "set-alias",
        description = "Record the name a person is actually called. An alias beats their \
                       address-book name everywhere. Omit `participant_ids` to delete it.",
        annotations(
            read_only_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_alias(
        &self,
        Parameters(args): Parameters<SetAliasArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::set_alias(
                &self.pool,
                &args.alias,
                &args.participant_ids.unwrap_or_default(),
                args.note.as_deref(),
            )
            .await,
        )
    }
}

#[tool_handler]
impl ServerHandler for WhatsAppMcp {
    /// Written out rather than left to `#[tool_handler(instructions = "…")]`: that
    /// argument takes a literal, and a literal folded to fit this file would drop the
    /// hard wrap the Python server sends.
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("whatsapp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

/// Own the pool and refuse an unmigrated database.
///
/// Separate from [`run`] so a test can call it without a listener.
pub async fn open_pool(settings: &McpSettings) -> Result<PgPool, DbError> {
    let pool = create_pool(settings.dsn.expose()).await?;
    if let Err(error) = assert_migrations(&pool).await {
        // This process runs as a role with no DDL rights. A schema it cannot create is
        // one it must not pretend to have.
        pool.close().await;
        return Err(error);
    }
    Ok(pool)
}

/// `/mcp` plus `/healthz`.
///
/// `/healthz` is a plain route on this router and only `/mcp` is nested onto the MCP
/// service, so the `Host` allowlist does not apply to the probe and `--health-check`
/// needs no entry of its own.
/// `tests/archive/server.rs::healthz_answers_a_host_the_mcp_endpoint_rejects` holds
/// that: if it ever stops being true the healthcheck starts failing for a reason
/// nothing about the container would explain.
pub fn build_router(pool: PgPool, allowed_hosts: Vec<String>) -> Router {
    let probe = pool.clone();
    let service = StreamableHttpService::new(
        move || Ok(WhatsAppMcp::new(pool.clone())),
        Arc::new(NeverSessionManager::default()),
        stateless_config(allowed_hosts),
    );
    Router::new()
        .route("/healthz", get(move || healthz(probe.clone())))
        .nest_service("/mcp", service)
}

/// One round trip to Postgres, not merely "the process is up".
///
/// A pool whose database has gone away still exists, and the container carries
/// `autoheal=true`: a probe that cannot tell the difference would leave a server that
/// answers every tool call with an error looking healthy.
async fn healthz(pool: PgPool) -> Response {
    match ping(&pool).await {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(error) => {
            let mut detail = error.to_string();
            detail.truncate(200);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "status": "database unreachable",
                    "detail": detail,
                })),
            )
                .into_response()
        }
    }
}

fn stateless_config(allowed_hosts: Vec<String>) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default();
    // There is no per-client state to keep, and statelessness takes session affinity off
    // the table behind MCPHub — what `stateless_http=True` bought the Python server.
    config.legacy_session_mode = false;
    // rmcp's guard against DNS rebinding. An empty list would mean "answer any `Host`",
    // so the list handed in here is never empty — see `DEFAULT_ALLOWED_HOSTS`. MCPHub
    // addresses this server by container name, so production sets `whatsapp-mcp`.
    config.allowed_hosts = allowed_hosts;
    config
}

pub async fn run(settings: &McpSettings) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pool = open_pool(settings).await?;
    let address: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    // Logged because the failure mode is a server that boots clean and then refuses
    // every tool call with a 403: one line here turns a baffling outage into an obvious
    // one. `/healthz` is outside this list, so a container can look healthy while every
    // call is rejected.
    tracing::info!(
        allowed_hosts = %settings.allowed_hosts.join(", "),
        "answering /mcp only for these Host headers"
    );
    tracing::info!(%address, "serving the whatsapp archive over MCP");
    axum::serve(listener, build_router(pool, settings.allowed_hosts.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Probe our own `/healthz`, for the container healthcheck.
///
/// `deploy/compose.yaml` currently shells out to `python -c "import urllib.request;
/// ..."`. A Rust image has neither Python nor, in a distroless or scratch base, a shell
/// — so at cutover that healthcheck silently becomes an unhealthy container. This
/// subcommand is what replaces it.
///
/// Hand-rolled over TCP rather than through an HTTP client crate: it reads one status
/// line from a loopback socket, and a dependency for that is a dependency to keep
/// patched for no gain.
pub async fn health_check(host: &str, port: u16) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // The bind address may be the wildcard, which is not a destination.
    let host = match host {
        "0.0.0.0" | "::" | "" => "127.0.0.1",
        host => host,
    };
    let target = format!("{host}:{port}");
    let mut stream = tokio::net::TcpStream::connect(&target)
        .await
        .map_err(|error| format!("could not connect to {target}: {error}"))?;
    stream
        .write_all(
            format!("GET /healthz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .map_err(|error| format!("could not send the probe: {error}"))?;

    let mut status = String::new();
    BufReader::new(stream)
        .read_line(&mut status)
        .await
        .map_err(|error| format!("no answer from {target}: {error}"))?;
    if status.contains(" 200 ") {
        return Ok(());
    }
    Err(format!("/healthz answered {}", status.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names() -> Vec<String> {
        WhatsAppMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    #[test]
    fn the_tool_surface_is_exactly_this() {
        // Deliberately smaller than the archive can serve. `aliases` duplicated what
        // `people` already prints and `catch-up` is `chats` with a `since`. Every tool
        // listed costs context on every call.
        //
        // The eight live WAHA tools the Python server also carries are absent, and
        // cannot be here: this binary does not link the crate that could reach WhatsApp.
        let mut names = tool_names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "chats",
                "message",
                "messages",
                "people",
                "revisions",
                "search",
                "set-alias",
                "status",
            ]
        );
    }

    #[test]
    fn names_are_hyphenated_not_mixed() {
        // MCPHub joins server and tool with `-`; an underscore would read as
        // `whatsapp-set_alias`, mixing both separators in one identifier.
        for name in tool_names() {
            assert!(!name.contains('_'), "{name}");
        }
    }

    #[test]
    fn every_tool_describes_itself() {
        // MCPHub shows these descriptions; an unlabelled tool is an unusable one.
        for tool in WhatsAppMcp::tool_router().list_all() {
            assert!(tool.description.is_some(), "{}", tool.name);
        }
    }

    #[test]
    fn a_client_can_tell_which_tools_only_read() {
        // With no hints every tool looks able to write, so `messages` was no safer on
        // its face than `set-alias` beside it.
        for tool in WhatsAppMcp::tool_router().list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
            assert_eq!(
                annotations.read_only_hint,
                Some(tool.name != "set-alias"),
                "{}",
                tool.name
            );
        }
    }

    #[test]
    fn no_tool_takes_a_session() {
        // The archive tools read PostgreSQL, which has no notion of a WAHA session, and
        // there are no other tools here. Offering the argument would imply the archive
        // could be scoped that way.
        for tool in WhatsAppMcp::tool_router().list_all() {
            let schema = serde_json::to_string(&tool.input_schema).expect("a schema");
            assert!(!schema.contains("session"), "{}: {schema}", tool.name);
        }
    }

    #[tokio::test]
    async fn the_health_check_target_is_never_the_wildcard_address() {
        // Connecting to 0.0.0.0 is not a thing the healthcheck can do; port 1 is
        // closed, so the error names the address it actually tried.
        let error = health_check("0.0.0.0", 1).await.unwrap_err();
        assert!(error.contains("127.0.0.1:1"), "{error}");
    }
}
