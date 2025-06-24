use crate::proxy_handler::ProxyHandler;
use axum::{
    extract::State,
    http::{HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing, Router,
};
/**
 * Create a local HTTP+streamable server that proxies requests to a stdio MCP server.
 */
use rmcp::{
    model::{ClientCapabilities, ClientInfo},
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServiceExt,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::{
    collections::HashMap, error::Error as StdError, net::SocketAddr, sync::Arc, time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

#[cfg(unix)]
use crate::utils::signal_number_to_name;

/// Settings for the HTTP server (with SSE fallback)
pub struct HttpServerSettings {
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

    /// Run the HTTP server with StreamableHttpService and SSE fallback
    pub async fn run_http_server(
        &self,
        stdio_params: StdioServerParameters,
        http_settings: HttpServerSettings,
    ) -> Result<(), Box<dyn StdError>> {
        info!(
            "Starting HTTP server with StreamableHttpService on {:?} with command: {}",
            http_settings.bind_addr, stdio_params.command,
        );

        let cancellation_token = CancellationToken::new();

        // Create notification broadcast channel for SSE notifications
        let (notification_tx, _) = broadcast::channel(100);
        let notification_tx = Arc::new(notification_tx);

        // Create child process with stdout/stderr logging
        let logging_process = self
            .create_child_process_with_logging(
                &stdio_params.command,
                &stdio_params.args,
                &stdio_params.env,
            )
            .await?;

        // Wrap the transport to capture notifications
        let notification_capturing_transport =
            NotificationCapturingTransport::new(logging_process, notification_tx.clone());

        let client_info = ClientInfo {
            protocol_version: Default::default(),
            capabilities: ClientCapabilities::builder().enable_sampling().build(),
            ..Default::default()
        };

        let client = client_info.serve(notification_capturing_transport).await?;

        let server_info = client.peer_info();
        info!(
            "Connected to server: {}",
            server_info.unwrap().server_info.name
        );

        let proxy_handler = ProxyHandler::new_with_notifications(client, notification_tx.clone());

        // Create StreamableHttpService for HTTP streaming (primary transport)
        // Enable stateful mode for proper SSE session management
        let http_config = StreamableHttpServerConfig {
            sse_keep_alive: http_settings.keep_alive,
            stateful_mode: true,
        };

        let session_manager = Arc::new(LocalSessionManager::default());
        let proxy_clone_http = proxy_handler.clone();
        let http_service = StreamableHttpService::new(
            move || Ok(proxy_clone_http.clone()),
            session_manager.clone(),
            http_config,
        );

        // Middleware for MCP protocol compliance (headers, session validation)
        async fn mcp_protocol_middleware(
            mut request: Request<axum::body::Body>,
            next: Next,
        ) -> Result<Response, StatusCode> {
            let headers = request.headers_mut();

            // Check if Accept header exists and needs fixing
            let current_accept = headers
                .get("accept")
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string());

            match &current_accept {
                Some(accept)
                    if !accept.contains("application/json")
                        && !accept.contains("text/event-stream") =>
                {
                    headers.insert(
                        "accept",
                        HeaderValue::from_static("application/json, text/event-stream"),
                    );
                }
                Some(accept) if !accept.contains("application/json") => {
                    // Add application/json to existing accept header
                    let new_accept = format!("{}, application/json", accept);
                    if let Ok(header_value) = HeaderValue::from_str(&new_accept) {
                        headers.insert("accept", header_value);
                    }
                }
                Some(accept) if !accept.contains("text/event-stream") => {
                    // Add text/event-stream to existing accept header
                    let new_accept = format!("{}, text/event-stream", accept);
                    if let Ok(header_value) = HeaderValue::from_str(&new_accept) {
                        headers.insert("accept", header_value);
                    }
                }
                None => {
                    // No Accept header, add both
                    headers.insert(
                        "accept",
                        HeaderValue::from_static("application/json, text/event-stream"),
                    );
                }
                _ => {
                    // Accept header already contains both, no change needed
                    if let Some(accept) = &current_accept {
                        tracing::debug!("Accept header already valid: {}", accept);
                    }
                }
            }

            let mut response = next.run(request).await;

            // Add MCP protocol headers to all responses per 2025-06-18 spec
            let headers = response.headers_mut();
            headers.insert(
                "mcp-protocol-version",
                HeaderValue::from_static("2025-06-18"),
            );

            Ok(response)
        }

        let cors_layer = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
            .allow_credentials(false);

        let sse_proxy = proxy_handler.clone();
        let notification_tx_clone = notification_tx.clone();

        // Create SSE handler with notification support
        let sse_handler = move |State(_proxy): State<ProxyHandler>| {
            let notification_rx = notification_tx_clone.subscribe();
            async move {
                use futures::stream::{self, StreamExt};
                use std::convert::Infallible;
                use tokio_stream::wrappers::{BroadcastStream, IntervalStream};

                tracing::debug!("SSE connection established");

                let endpoint_event = stream::once(async {
                    Ok::<_, Infallible>(
                        axum::response::sse::Event::default()
                            .event("endpoint")
                            .data("/"),
                    )
                });

                let notification_stream =
                    BroadcastStream::new(notification_rx).filter_map(|result| async {
                        match result {
                            Ok(notification) => Some(Ok::<_, Infallible>(
                                axum::response::sse::Event::default()
                                    .event("notification")
                                    .data(notification),
                            )),
                            Err(_) => None, // Skip broadcast errors
                        }
                    });

                let keepalive_stream =
                    IntervalStream::new(tokio::time::interval(std::time::Duration::from_secs(30)))
                        .map(|_| {
                            Ok::<_, Infallible>(
                                axum::response::sse::Event::default()
                                    .event("ping")
                                    .data("keepalive"),
                            )
                        });

                let combined_stream = endpoint_event
                    .chain(notification_stream)
                    .chain(keepalive_stream);

                let mut response = axum::response::Sse::new(combined_stream)
                    .keep_alive(
                        axum::response::sse::KeepAlive::new()
                            .interval(std::time::Duration::from_secs(15))
                            .text("keepalive"),
                    )
                    .into_response();

                let headers = response.headers_mut();
                headers.insert(
                    "mcp-protocol-version",
                    HeaderValue::from_static("2025-06-18"),
                );
                headers.insert(
                    "content-type",
                    HeaderValue::from_static("text/event-stream"),
                );
                headers.insert("cache-control", HeaderValue::from_static("no-cache"));
                headers.insert("connection", HeaderValue::from_static("keep-alive"));

                Ok::<_, StatusCode>(response)
            }
        };

        // Create method router that handles both GET (SSE) and POST (HTTP) at root
        let root_methods = routing::method_routing::MethodRouter::new()
            .get(sse_handler)
            .fallback_service(http_service);

        let app = Router::new()
            .route("/", root_methods)
            .with_state(sse_proxy)
            .layer(
                ServiceBuilder::new()
                    .layer(middleware::from_fn(mcp_protocol_middleware))
                    .layer(cors_layer),
            );

        // Start the server
        info!(
            "HTTP Streaming server running on {} (GET / for SSE, POST / for HTTP)",
            http_settings.bind_addr
        );

        let listener = tokio::net::TcpListener::bind(http_settings.bind_addr).await?;
        let server = axum::serve(listener, app);

        tokio::select! {
            result = server => {
                if let Err(e) = result {
                    tracing::error!("HTTP server error: {}", e);
                }
            }
            _ = cancellation_token.cancelled() => {
                info!("Received shutdown signal");
            }
        }

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

/// A transport wrapper that captures notifications and forwards them to SSE
pub struct NotificationCapturingTransport<T> {
    inner: T,
    notification_tx: Arc<broadcast::Sender<String>>,
}

impl<T> NotificationCapturingTransport<T> {
    pub fn new(inner: T, notification_tx: Arc<broadcast::Sender<String>>) -> Self {
        Self {
            inner,
            notification_tx,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for NotificationCapturingTransport<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let inner = Pin::new(&mut self.inner);
        let result = inner.poll_read(cx, buf);

        // check if the msg contains notifications
        if let Poll::Ready(Ok(())) = result {
            let data = buf.filled();
            if !data.is_empty() {
                let data_str = String::from_utf8_lossy(data);

                for line in data_str.lines() {
                    if let Ok(json_value) = serde_json::from_str::<Value>(line) {
                        if let Some(method) = json_value.get("method").and_then(|m| m.as_str()) {
                            if method.starts_with("notifications/") {
                                let _ = self.notification_tx.send(line.to_string());
                            }
                        }
                    }
                }
            }
        }

        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for NotificationCapturingTransport<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let inner = Pin::new(&mut self.inner);
        inner.poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let inner = Pin::new(&mut self.inner);
        inner.poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let inner = Pin::new(&mut self.inner);
        inner.poll_shutdown(cx)
    }
}

/// Run the SSE server with a stdio client
///
/// This function connects to a stdio server and exposes it as an SSE server.
pub async fn run_sse_server(
    stdio_params: StdioServerParameters,
    http_settings: HttpServerSettings,
) -> Result<(), Box<dyn StdError>> {
    let server_manager = SseServerManager::new(
        stdio_params.log_url.clone(),
        stdio_params.access_token.clone(),
    );

    server_manager
        .run_http_server(stdio_params, http_settings)
        .await
}
