use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{EnvFilter, fmt};

mod tools;

const LOG_ENV: &str = "REFORGE_LOG";
/// Pre-rebrand name of [`LOG_ENV`]; still honored when the new name is unset.
const LEGACY_LOG_ENV: &str = "FOSSIL_LOG";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Log to stderr so it doesn't interfere with the MCP stdio transport.
    fmt()
        .with_env_filter(
            EnvFilter::try_from_env(LOG_ENV)
                .or_else(|_| EnvFilter::try_from_env(LEGACY_LOG_ENV))
                .unwrap_or_else(|_| EnvFilter::new("reforge_mcp=info,warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    warn_on_legacy_env();
    tracing::info!("reforge-mcp server starting (stdio transport)");

    let service = tools::ReforgeServer::new();
    let server = service.serve(stdio()).await?;
    server.waiting().await?;

    tracing::info!("reforge-mcp server stopped");
    Ok(())
}

fn warn_on_legacy_env() {
    use reforge_core::storage::{CACHE_DIR_ENV, LEGACY_CACHE_DIR_ENV};
    for (legacy, current) in [
        (LEGACY_LOG_ENV, LOG_ENV),
        (LEGACY_CACHE_DIR_ENV, CACHE_DIR_ENV),
    ] {
        if std::env::var_os(legacy).is_some() && std::env::var_os(current).is_none() {
            tracing::warn!("{legacy} is deprecated; set {current} instead");
        }
    }
}
