mod actual;
mod db;
mod models;
mod server;

use server::ActualServer;
use turbomcp::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Log to stderr: in STDIO transport mode stdout carries the MCP JSON-RPC
    // stream, so any log line on stdout would corrupt the protocol.
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let server = ActualServer::new()?;
    // An unset *or empty* BIND_ADDR means STDIO transport.
    match std::env::var("BIND_ADDR").ok().filter(|s| !s.is_empty()) {
        Some(addr) => {
            tracing::info!("actual-budget-mcp listening on {addr}");
            server
                .builder()
                .transport(Transport::http(&addr))
                .serve()
                .await?;
        }
        None => {
            server.builder().transport(Transport::stdio()).serve().await?;
        }
    }
    Ok(())
}
