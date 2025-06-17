use crate::proxy_handler::ProxyHandler;
/**
 * Create a local SSE server that proxies requests to a stdio MCP server.
 */
use rmcp::{
    model::{ClientCapabilities, ClientInfo},
    transport::sse_server::{SseServer, SseServerConfig},
    ServiceExt,
};
use std::collections::VecDeque;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::{
    collections::HashMap, error::Error as StdError, net::SocketAddr, sync::Arc, time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

#[cfg(unix)]
use crate::utils::signal_number_to_name;

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
    pub log_url: Option<String>,
    pub access_token: Option<String>,
}

use reqwest::Client;
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStreamType {
    Stdout,
    Stderr,
    Stdin,
    Command,
}

impl LogStreamType {
    pub fn as_log_type(&self) -> &'static str {
        match self {
            LogStreamType::Stdout => "mcp_stdout",
            LogStreamType::Stderr => "mcp_stderr",
            LogStreamType::Stdin => "mcp_stdin",
            LogStreamType::Command => "cmd_log",
        }
    }

    pub fn as_display(&self) -> &'static str {
        match self {
            LogStreamType::Stdout => "[STDOUT]",
            LogStreamType::Stderr => "[STDERR]",
            LogStreamType::Stdin => "[STDIN]",
            LogStreamType::Command => "[CMD]",
        }
    }
}

#[derive(Clone)]
pub struct Logger {
    config: Option<Arc<LogConfig>>,
}

struct LogConfig {
    client: Client,
    url: String,
    access_token: String,
}

impl Logger {
    pub fn new(log_url: Option<String>, access_token: Option<String>) -> Self {
        let config = if let (Some(url), Some(token)) = (log_url, access_token) {
            Some(Arc::new(LogConfig {
                client: Client::new(),
                url,
                access_token: token,
            }))
        } else {
            None
        };

        Logger { config }
    }

    pub async fn log(&self, line: &str, stream_type: LogStreamType) {
        println!("{} - {}", stream_type.as_display(), line);

        if let Some(config) = &self.config {
            self.send_to_remote(line, stream_type, config).await;
        }
    }

    /// Send log to remote URL
    async fn send_to_remote(&self, message: &str, stream_type: LogStreamType, config: &LogConfig) {
        let log_type = stream_type.as_log_type();

        let payload = json!({
            "logType": log_type,
            "level": "info",
            "message": message,
            "timestamp": chrono::Utc::now().to_rfc3339()
        });

        let result = config
            .client
            .post(&config.url)
            .header("x-api-key", &config.access_token)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await;

        match result {
            Ok(response) => {
                if !response.status().is_success() {
                    eprintln!(
                        "Failed to send log to remote URL: HTTP {}",
                        response.status()
                    );
                }
            }
            Err(e) => {
                eprintln!("Error sending log to remote URL: {}", e);
            }
        }
    }
}

pub struct SseServerManager {
    logger: Logger,
}

impl SseServerManager {
    pub fn new(log_url: Option<String>, access_token: Option<String>) -> Self {
        Self {
            logger: Logger::new(log_url, access_token),
        }
    }

    async fn log(&self, line: &str, stream_type: LogStreamType) {
        self.logger.log(line, stream_type).await;
    }

    /// Run the SSE server with a stdio client
    pub async fn run_sse_server(
        &self,
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
        let logging_process = self
            .create_child_process_with_logging(
                &stdio_params.command,
                &stdio_params.args,
                &stdio_params.env,
            )
            .await?;

        let client_info = ClientInfo {
            protocol_version: Default::default(),
            capabilities: ClientCapabilities::builder().enable_sampling().build(),
            ..Default::default()
        };

        let client = client_info.serve(logging_process).await?;

        let server_info = client.peer_info();
        info!("Connected to server: {}", server_info.server_info.name);

        let proxy_handler = ProxyHandler::new(client);

        // Start the SSE server
        let sse_server = SseServer::serve_with_config(config.clone()).await?;

        let ct = sse_server.with_service(move || proxy_handler.clone());

        // Wait for shutdown signals (SIGINT/SIGTERM)
        let signal_name = {
            #[cfg(unix)]
            {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = async {
                        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                        term.recv().await
                    } => "SIGTERM",
                }
            }
            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await?;
                "SIGINT"
            }
        };

        // Log signal receipt as structured JSON
        let signal_log = serde_json::json!({
            "event": "signal_received",
            "signal": signal_name,
            "action": "shutting_down",
            "timestamp": chrono::Utc::now().to_rfc3339()
        });

        // Log to console and remote logger if configured
        println!("{} - {}", LogStreamType::Command.as_display(), signal_log);
        self.log(&signal_log.to_string(), LogStreamType::Command)
            .await;

        ct.cancel();

        Ok(())
    }

    /// Create a LoggingChildProcess that captures stdout/stderr
    async fn create_child_process_with_logging(
        &self,
        command_name: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<LoggingChildProcess, Box<dyn StdError>> {
        // Log the command being started as structured JSON
        let cmd_log = serde_json::json!({
            "event": "process_start",
            "command": command_name,
            "args": args,
            "timestamp": chrono::Utc::now().to_rfc3339()
        });
        self.log(&cmd_log.to_string(), LogStreamType::Command).await;

        // Create the command
        let mut command = Command::new(command_name);
        command.args(args);

        for (key, value) in env {
            command.env(key, value);
        }

        // Create our custom logging child process
        Ok(LoggingChildProcess::new(
            &mut command,
            Some(self.logger.clone()),
        )?)
    }
}

/// A custom transport that replicates TokioChildProcess but captures stdout/stderr
pub struct LoggingChildProcess {
    stdin: Option<tokio::process::ChildStdin>,
    stdout_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    stdout_buffer: VecDeque<u8>,
    _stderr_task: Option<tokio::task::JoinHandle<()>>,
    logger: Option<Logger>,
    _exit_task: Option<tokio::task::JoinHandle<()>>,
}

impl LoggingChildProcess {
    pub fn new(command: &mut Command, logger: Option<Logger>) -> Result<Self, std::io::Error> {
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn()?;

        // Split/fork the input/output
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (stdout_sender, stdout_receiver) = mpsc::unbounded_channel();

        // Clone logger for async tasks
        let logger_for_stdout = logger.clone();
        let logger_for_stderr = logger.clone();

        // Wrap child in Arc<Mutex<_>> for sharing between tasks
        let child_arc = Arc::new(tokio::sync::Mutex::new(child));

        if let Some(stdout) = stdout {
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        // Log to console
                        println!("{} - {}", LogStreamType::Stdout.as_display(), line);

                        // Send to remote logger if configured
                        if let Some(logger) = &logger_for_stdout {
                            if let Some(config) = &logger.config {
                                logger
                                    .send_to_remote(&line, LogStreamType::Stdout, config)
                                    .await;
                            }
                        }
                    }

                    // Forward the line with newline
                    let mut line_with_newline = line;
                    line_with_newline.push('\n');
                    let data = line_with_newline.into_bytes();
                    if stdout_sender.send(data).is_err() {
                        break;
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
                        // Log to console
                        println!("{} - {}", LogStreamType::Stderr.as_display(), line);

                        // Send to remote logger if configured
                        if let Some(logger) = &logger_for_stderr {
                            if let Some(config) = &logger.config {
                                logger
                                    .send_to_remote(&line, LogStreamType::Stderr, config)
                                    .await;
                            }
                        }
                    }
                }
            })
        });

        let logger_for_exit = logger.clone();
        let child_for_exit = child_arc.clone();
        let exit_task = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                let mut child_guard = child_for_exit.lock().await;

                match child_guard.try_wait() {
                    Ok(Some(exit_status)) => {
                        // Process has exited - create structured JSON log
                        let exit_log = if let Some(code) = exit_status.code() {
                            serde_json::json!({
                                "event": "process_exit",
                                "exit_type": "normal",
                                "exit_code": code,
                                "timestamp": chrono::Utc::now().to_rfc3339()
                            })
                        } else {
                            // Process was terminated by a signal
                            #[cfg(unix)]
                            {
                                use std::os::unix::process::ExitStatusExt;
                                if let Some(signal) = exit_status.signal() {
                                    let signal_name = signal_number_to_name(signal);

                                    serde_json::json!({
                                        "event": "process_exit",
                                        "exit_type": "signal",
                                        "signal": signal_name,
                                        "signal_number": signal,
                                        "timestamp": chrono::Utc::now().to_rfc3339()
                                    })
                                } else {
                                    serde_json::json!({
                                        "event": "process_exit",
                                        "exit_type": "signal",
                                        "signal": "UNKNOWN",
                                        "timestamp": chrono::Utc::now().to_rfc3339()
                                    })
                                }
                            }
                            #[cfg(not(unix))]
                            {
                                serde_json::json!({
                                    "event": "process_exit",
                                    "exit_type": "signal",
                                    "timestamp": chrono::Utc::now().to_rfc3339()
                                })
                            }
                        };

                        let exit_msg = exit_log.to_string();

                        // Log to console
                        println!("{} - {}", LogStreamType::Command.as_display(), exit_msg);

                        // Send to remote logger if configured
                        if let Some(logger) = &logger_for_exit {
                            logger.log(&exit_msg, LogStreamType::Command).await;
                        }
                        break;
                    }
                    Ok(None) => {
                        // Process is still running, continue checking
                        continue;
                    }
                    Err(_) => {
                        // Error checking process status, break out
                        break;
                    }
                }
            }
        }));

        Ok(Self {
            stdin,
            stdout_receiver,
            stdout_buffer: VecDeque::new(),
            _stderr_task: stderr_task,
            logger,
            _exit_task: exit_task,
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
            let logger = self.logger.clone();
            tokio::spawn(async move {
                // Log to console
                println!(
                    "{} - {}",
                    LogStreamType::Stdin.as_display(),
                    content_trimmed
                );

                // Send to remote logger if configured
                if let Some(logger) = &logger {
                    if let Some(config) = &logger.config {
                        logger
                            .send_to_remote(&content_trimmed, LogStreamType::Stdin, config)
                            .await;
                    }
                }
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

/// Run the SSE server with a stdio client
///
/// This function connects to a stdio server and exposes it as an SSE server.
pub async fn run_sse_server(
    stdio_params: StdioServerParameters,
    sse_settings: SseServerSettings,
) -> Result<(), Box<dyn StdError>> {
    let server_manager = SseServerManager::new(
        stdio_params.log_url.clone(),
        stdio_params.access_token.clone(),
    );

    server_manager
        .run_sse_server(stdio_params, sse_settings)
        .await
}
