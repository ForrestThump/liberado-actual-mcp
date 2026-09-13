mod actual;
mod budget_api;
mod db;
mod models;
mod pattern;
mod server;

use server::ActualServer;
use turbomcp::prelude::*;

/// Origin policy for the MCP surface.
///
/// turbomcp validates the `Origin` header and answers 403 to anything it does not recognise unless
/// the peer is loopback. That check exists to stop DNS rebinding, where a malicious page in a
/// **browser** aims XHR at an MCP server bound to localhost.
///
/// It cannot do that job here and it breaks the only client we have: Liberado is a server-side HTTP
/// client reaching this container across a private Docker bridge — not loopback, and it sends no
/// `Origin` at all, so it is refused every time. Browsers always send `Origin` on cross-origin
/// requests, so an Origin-less request is definitionally not the attack being defended against.
/// Without this the server 403s every call (exactly what happened on liberado-search-orchestrator-mcp).
///
/// Default is permissive because this server is consumed by MCP clients on a private network and is
/// not published to the internet. Set `MCP_ALLOWED_ORIGINS` to a comma-separated list to switch to
/// strict allow-listing if it is ever exposed to a browser.
fn origin_policy() -> ServerConfig {
    let builder = ServerConfig::builder();

    match std::env::var("MCP_ALLOWED_ORIGINS") {
        Ok(raw) if !raw.trim().is_empty() => {
            let origins: Vec<String> = raw
                .split(',')
                .map(|o| o.trim().to_string())
                .filter(|o| !o.is_empty())
                .collect();
            tracing::info!(?origins, "MCP origin validation: allow-listed");
            builder
                .allow_origins(origins)
                .allow_any_origin(false)
                .build()
        }
        _ => {
            tracing::info!(
                "MCP origin validation: disabled (private-network default). \
                 Set MCP_ALLOWED_ORIGINS to enforce an allow-list."
            );
            builder.allow_any_origin(true).build()
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Log to stderr: in STDIO transport mode stdout carries the MCP JSON-RPC
    // stream, so any log line on stdout would corrupt the protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let server = ActualServer::new()?;
    // An unset *or empty* BIND_ADDR means STDIO transport.
    match std::env::var("BIND_ADDR").ok().filter(|s| !s.is_empty()) {
        Some(addr) => {
            tracing::info!("liberado-actual-mcp listening on {addr}");
            // origin_policy() is load-bearing on the HTTP transport: without it turbomcp 403s every
            // request from Liberado. STDIO below needs no policy — there is no Origin header there.
            server
                .builder()
                .with_config(origin_policy())
                .transport(Transport::http(&addr))
                .serve()
                .await?;
        }
        None => {
            server
                .builder()
                .transport(Transport::stdio())
                .serve()
                .await?;
        }
    }
    Ok(())
}
