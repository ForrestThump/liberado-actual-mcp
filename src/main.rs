mod actual;
mod db;
mod models;
mod server;

use server::ActualServer;
use turbomcp::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let server = ActualServer::new()?;
    if let Ok(addr) = std::env::var("BIND_ADDR") {
        tracing::info!("actual-budget-mcp listening on {addr}");
        server
            .builder()
            .transport(Transport::http(&addr))
            .serve()
            .await?;
    } else {
        server.builder().transport(Transport::stdio()).serve().await?;
    }
    Ok(())
}
