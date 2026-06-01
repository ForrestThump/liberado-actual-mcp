mod actual;
mod db;
mod models;
mod server;

use server::ActualServer;
use turbomcp::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8000".to_string());
    tracing::info!("actual-budget-mcp listening on {addr}");
    ActualServer::new()
        .builder()
        .allow_any_origin(true)
        .transport(Transport::http(&addr))
        .serve()
        .await?;
    Ok(())
}
