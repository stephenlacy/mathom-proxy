use crate::proxy_handler::ProxyHandler;
/**
 * Create a local SSE server that proxies requests to a stdio MCP server.
 */
use rmcp::{
    model::{ClientCapabilities, ClientInfo},
    transport::sse_server::{SseServer, SseServerConfig},
    ServiceExt,
};
use std::{collections::HashMap, error::Error as StdError, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Settings for the SSE server
pub struct SseServerSettings {
    pub bind_addr: SocketAddr,
    pub keep_alive: Option<Duration>,
}

/// StdioServerParameters holds parameters for the stdio client.
pub struct StdioServerParameters {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// TODO: pipe to logger service
async fn handle_log_output(line: &str, kind: &str) {
    println!("{} - {}", kind, line);
}

use std::collections::VecDeque;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// A custom transport that replicates TokioChildProcess but captures stdout/stderr
pub struct LoggingChildProcess {
    _child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    stdout_buffer: VecDeque<u8>,
    _stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl LoggingChildProcess {
    pub fn new(command: &mut Command) -> Result<Self, std::io::Error> {
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn()?;

        // Split/fork the input/output
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (stdout_sender, stdout_receiver) = mpsc::unbounded_channel();

        if let Some(mut stdout) = stdout {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buffer = [0u8; 4096];

                loop {
                    match stdout.read(&mut buffer).await {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            let data = buffer[..n].to_vec();

                            let content = String::from_utf8_lossy(&data).trim().to_string();
                            if !content.is_empty() {
                                handle_log_output(&content, "[STDOUT]").await;
                            }

                            // Forward
                            if stdout_sender.send(data).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Capture stderr
        let stderr_task = stderr.map(|stderr| {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        handle_log_output(&line, "[STDERR]").await;
                    }
                }
            })
        });

        Ok(Self {
            _child: child,
            stdin,
            stdout_receiver,
            stdout_buffer: VecDeque::new(),
            _stderr_task: stderr_task,
        })
    }
}

impl AsyncRead for LoggingChildProcess {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let available = buf.remaining();
        let mut bytes_read = 0;

        while bytes_read < available && !self.stdout_buffer.is_empty() {
            if let Some(byte) = self.stdout_buffer.pop_front() {
                buf.put_slice(&[byte]);
                bytes_read += 1;
            }
        }

        if bytes_read > 0 {
            return Poll::Ready(Ok(()));
        }

        match self.stdout_receiver.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                for byte in data {
                    self.stdout_buffer.push_back(byte);
                }

                let mut bytes_read = 0;
                while bytes_read < available && !self.stdout_buffer.is_empty() {
                    if let Some(byte) = self.stdout_buffer.pop_front() {
                        buf.put_slice(&[byte]);
                        bytes_read += 1;
                    }
                }

                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => {
                // Channel closed, EOF
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                // No data yet
                Poll::Pending
            }
        }
    }
}

impl AsyncWrite for LoggingChildProcess {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let content = String::from_utf8_lossy(buf);
        let content_trimmed = content.trim().to_string();
        if !content_trimmed.is_empty() {
            tokio::spawn(async move {
                handle_log_output(&content_trimmed, "[STDIN]").await;
            });
        }

        // Pass through
        if let Some(ref mut stdin) = self.stdin {
            let stdin_pin = Pin::new(stdin);
            stdin_pin.poll_write(cx, buf)
        } else {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdin not available",
            )))
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        if let Some(ref mut stdin) = self.stdin {
            let stdin_pin = Pin::new(stdin);
            stdin_pin.poll_flush(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        if let Some(ref mut stdin) = self.stdin {
            let stdin_pin = Pin::new(stdin);
            stdin_pin.poll_shutdown(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

/// Create a LoggingChildProcess that captures stdout/stderr
async fn create_child_process_with_logging(
    command_name: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<LoggingChildProcess, Box<dyn StdError>> {
    // Log the command being started
    let args_str = args.join(" ");
    let log_line = format!("Starting command: {} {}", command_name, args_str);
    handle_log_output(&log_line, "").await;

    // Create the command
    let mut command = Command::new(command_name);
    command.args(args);

    for (key, value) in env {
        command.env(key, value);
    }

    // Create our custom logging child process
    Ok(LoggingChildProcess::new(&mut command)?)
}

/// Run the SSE server with a stdio client
///
/// This function connects to a stdio server and exposes it as an SSE server.
pub async fn run_sse_server(
    stdio_params: StdioServerParameters,
    sse_settings: SseServerSettings,
) -> Result<(), Box<dyn StdError>> {
    info!(
        "Running SSE server on {:?} with command: {}",
        sse_settings.bind_addr, stdio_params.command,
    );

    // Configure SSE server
    let config = SseServerConfig {
        bind: sse_settings.bind_addr,
        sse_path: "/sse".to_string(),
        post_path: "/message".to_string(),
        ct: CancellationToken::new(),
        sse_keep_alive: sse_settings.keep_alive,
    };

    // Create child process with stdout/stderr logging
    let logging_process = create_child_process_with_logging(
        &stdio_params.command,
        &stdio_params.args,
        &stdio_params.env,
    )
    .await?;

    let client_info = ClientInfo {
        protocol_version: Default::default(),
        capabilities: ClientCapabilities::builder()
            // Use minimal capabilities to avoid validation errors
            .enable_sampling()
            .build(),
        ..Default::default()
    };

    // Create client service
    let client = client_info.serve(logging_process).await?;

    // Get server info
    let server_info = client.peer_info();
    info!("Connected to server: {}", server_info.server_info.name);

    // Create proxy handler
    let proxy_handler = ProxyHandler::new(client);

    // Start the SSE server
    let sse_server = SseServer::serve_with_config(config.clone()).await?;

    // Register the proxy handler with the SSE server
    let ct = sse_server.with_service(move || proxy_handler.clone());

    // Wait for Ctrl+C to shut down
    tokio::signal::ctrl_c().await?;
    ct.cancel();

    Ok(())
}
