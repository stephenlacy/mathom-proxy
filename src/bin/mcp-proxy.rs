/**
 * The entry point for the mcp-proxy application.
 * It sets up logging and runs the main function.
 */
use clap::{ArgAction, Parser};
use rmcp_proxy::{
    config::get_config_dir,
    run_sse_client, run_sse_server,
    sse_client::LocalSseClientConfig,
    sse_server::{HttpServerSettings, StdioServerParameters},
};
use std::{collections::HashMap, env, error::Error, net::SocketAddr, process, time::Duration};
use tracing::{debug, error};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// MCP Proxy CLI arguments
#[derive(Parser)]
#[command(
    name = "mcp-proxy",
    version = env!("CARGO_PKG_VERSION"),
    about = concat!("MCP Proxy v",env!("CARGO_PKG_VERSION"),". Start the MCP proxy in one of two possible modes: as an HTTP/SSE client or HTTP server."),
    long_about = None,
    after_help = "Examples:\n  \
        Expose a local stdio server as an HTTP server:\n  \
        mcp-proxy your-command --port 8080 -e KEY VALUE -e ANOTHER_KEY ANOTHER_VALUE\n  \
        mcp-proxy --port 8080 -- your-command --arg1 value1 --arg2 value2\n  \
        mcp-proxy --port 8080 -- python mcp_server.py\n  \
        mcp-proxy --port 8080 --host 0.0.0.0 -- npx -y @modelcontextprotocol/server-everything
",
)]
struct Cli {
    /// Command or URL to connect to. When a URL, will run an HTTP/SSE client,
    /// otherwise will run the given command and expose it as an HTTP server.
    #[arg(env = "SSE_URL")]
    command_or_url: Option<String>,

    /// Headers to pass to the SSE server. Can be used multiple times.
    #[arg(short = 'H', long = "headers", value_names = ["KEY", "VALUE"], number_of_values = 2)]
    headers: Vec<String>,

    /// Any extra arguments to the command to spawn the server
    #[arg(last = true, allow_hyphen_values = true)]
    args: Vec<String>,

    /// Environment variables used when spawning the server. Can be used multiple times.
    #[arg(short = 'e', long = "env", value_names = ["KEY", "VALUE"], number_of_values = 2)]
    env_vars: Vec<String>,

    /// Pass through all environment variables when spawning the server.
    #[arg(long = "pass-environment", action = ArgAction::SetTrue)]
    pass_environment: bool,

    /// Port to expose an HTTP server on. Default is a random port
    #[arg(long = "port", default_value = "0")]
    port: u16,

    /// Host to expose an HTTP server on. Default is 127.0.0.1
    #[arg(long = "host", default_value = "127.0.0.1")]
    host: String,

    /// URL to send logs to (optional)
    #[arg(long = "log-url", env = "LOG_URL")]
    log_url: Option<String>,

    /// Access token for logging service
    #[arg(long = "mathom-access-token", env = "MATHOM_ACCESS_TOKEN")]
    mathom_access_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let mut cli = Cli::parse();

    // Check if we have a command or URL, or use the first of the pased args
    let command_or_url = match cli.command_or_url {
        Some(value) => value,
        None => match cli.args.len() {
            0 => {
                eprintln!("Error: command or URL is required");
                std::process::exit(1);
            }
            _ => cli.args.remove(0),
        },
    };

    // Check if it's a URL (SSE client mode) or a command (stdio client mode)
    if command_or_url.starts_with("http://") || command_or_url.starts_with("https://") {
        // Start a client connected to the SSE server, and expose as a stdio server
        debug!("Starting SSE client and stdio server");

        // Convert headers from Vec<String> to HashMap<String, String>
        let mut headers = HashMap::new();
        for i in (0..cli.headers.len()).step_by(2) {
            if i + 1 < cli.headers.len() {
                headers.insert(cli.headers[i].clone(), cli.headers[i + 1].clone());
            }
        }

        // Create SSE client config
        let config = LocalSseClientConfig {
            url: command_or_url,
            headers,
        };

        // Run SSE client
        run_sse_client(config).await?;
    } else if command_or_url == "reset" {
        let config_dir = get_config_dir();

        println!("Deleting auth config at {:?}", config_dir);
        if let Err(e) = std::fs::remove_dir_all(&config_dir) {
            if e.kind() == std::io::ErrorKind::NotFound {
                println!("Auth config not found at {:?}", config_dir);
                return Ok(());
            }
            // Handle the error without using ?
            error!("Failed to delete auth config: {}", e);
            process::exit(1);
        }
        debug!("Auth config deleted");
    } else {
        // Start a client connected to the given command, and expose as an SSE server
        debug!("Starting stdio client and SSE server");

        // The environment variables passed to the server process
        let mut env_map = HashMap::new();

        // Pass through current environment variables if configured
        if cli.pass_environment {
            for (key, value) in env::vars() {
                env_map.insert(key, value);
            }
        }

        // Pass in and override any environment variables with those passed on the command line
        for i in (0..cli.env_vars.len()).step_by(2) {
            if i + 1 < cli.env_vars.len() {
                env_map.insert(cli.env_vars[i].clone(), cli.env_vars[i + 1].clone());
            }
        }

        // Create stdio parameters
        let stdio_params = StdioServerParameters {
            command: command_or_url,
            args: cli.args,
            env: env_map,
            log_url: cli.log_url,
            access_token: cli.mathom_access_token,
        };

        // Create HTTP server settings
        let http_settings = HttpServerSettings {
            bind_addr: format!("{}:{}", cli.host, cli.port).parse::<SocketAddr>()?,
            keep_alive: Some(Duration::from_secs(15)),
        };

        // Run HTTP server
        run_sse_server(stdio_params, http_settings).await?;
    }

    Ok(())
}
