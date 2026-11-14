//! stdio entry point for the download MCP server.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Logs go to stderr so stdout stays a clean MCP channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("tgdlrs_mcp=warn")),
        )
        .with_ansi(false)
        .init();

    tgdlrs_mcp::serve_stdio().await?;
    Ok(())
}
