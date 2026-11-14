//! Local IPC between the CLI/MCP clients and the background worker.
//!
//! Transport is a Windows named pipe named after the current user, with an
//! explicit DACL that grants access only to that user (and SYSTEM). No TCP
//! port is ever opened. The protocol is one JSON object per line in each
//! direction; it never carries Telegram session data or credentials.

use std::ffi::{c_void, OsStr};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, PipeMode, ServerOptions};
use tokio::sync::watch;

use crate::error::{Error, Result};

/// Maximum accepted request size (one JSON line).
const MAX_LINE: usize = 1024 * 1024;
/// How long a client waits for the worker to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a client waits for a response.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);

/// One request sent by the CLI or MCP server.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    LoginStatus,
    ResolveLink {
        url: String,
    },
    ScanChat {
        chat_id: i64,
        limit: Option<usize>,
    },
    SearchFiles {
        chat_id: i64,
        query: String,
        limit: Option<usize>,
    },
    Stop,
}

/// Structured error returned by the worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteError {
    pub kind: String,
    pub message: String,
}

/// One response sent by the worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok { data: Value },
    Error { error: RemoteError },
}

impl Response {
    pub fn ok<T: Serialize>(value: T) -> Self {
        match serde_json::to_value(value) {
            Ok(data) => Self::Ok { data },
            Err(error) => Self::Error {
                error: RemoteError {
                    kind: "serialization".into(),
                    message: error.to_string(),
                },
            },
        }
    }

    pub fn error(error: &Error) -> Self {
        Self::Error {
            error: RemoteError {
                kind: error.kind().into(),
                message: error.to_string(),
            },
        }
    }

    fn into_result(self) -> Result<Value> {
        match self {
            Self::Ok { data } => Ok(data),
            Self::Error { error } => Err(Error::from_remote(&error.kind, &error.message)),
        }
    }
}

/// Server-side request dispatcher.
#[async_trait]
pub trait RequestHandler: Send + Sync + 'static {
    async fn handle(&self, request: Request) -> Response;
}

/// Serve the named pipe until `shutdown` changes.
///
/// Each connection carries exactly one request and one response. Connections
/// are handled concurrently, so a slow scan cannot block `status` or `stop`.
pub async fn serve(
    pipe_name: String,
    handler: Arc<dyn RequestHandler>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut first = true;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }

        let server = match create_pipe(&pipe_name, first) {
            Ok(server) => server,
            Err(error) => {
                // A stale instance from a crashed worker disappears once the
                // process exits; retry briefly instead of killing the worker.
                tracing::warn!(%error, "cannot create IPC pipe instance; retrying");
                tokio::select! {
                    _ = shutdown.changed() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                }
            }
        };
        first = false;

        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            connected = server.connect() => {
                if let Err(error) = connected {
                    tracing::warn!(%error, "IPC client failed to connect");
                    continue;
                }
            }
        }

        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(Duration::from_secs(300), serve_connection(server, handler))
                    .await;
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "IPC connection failed"),
                Err(_) => tracing::warn!("IPC connection timed out"),
            }
        });
    }
}

async fn serve_connection(
    server: NamedPipeServer,
    handler: Arc<dyn RequestHandler>,
) -> io::Result<()> {
    let (read_half, write_half) = tokio::io::split(server);
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);

    let Some(line) = read_line_bounded(&mut reader, MAX_LINE).await? else {
        return Ok(());
    };
    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            let response = Response::Error {
                error: RemoteError {
                    kind: "invalid_input".into(),
                    message: format!("invalid IPC request: {error}"),
                },
            };
            write_response(&mut writer, &response).await?;
            return Ok(());
        }
    };

    let response = handler.handle(request).await;
    write_response(&mut writer, &response).await
}

async fn write_response(
    writer: &mut BufWriter<tokio::io::WriteHalf<NamedPipeServer>>,
    response: &Response,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// Send one request to the worker and deserialize its response.
pub async fn request<T: serde::de::DeserializeOwned>(
    pipe_name: &str,
    request: &Request,
) -> Result<T> {
    let value = request_value(pipe_name, request).await?;
    serde_json::from_value(value)
        .map_err(|error| Error::Ipc(format!("worker returned an unexpected response: {error}")))
}

/// Send one request and return the raw JSON value.
pub async fn request_value(pipe_name: &str, request: &Request) -> Result<Value> {
    let mut payload = serde_json::to_vec(request)?;
    payload.push(b'\n');

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let client = loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => break client,
            Err(error) if is_retryable_connect(&error) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) if is_retryable_connect(&error) => {
                return Err(Error::WorkerUnavailable);
            }
            Err(error) => {
                return Err(Error::Ipc(format!("cannot connect to the worker: {error}")));
            }
        }
    };

    let (read_half, write_half) = tokio::io::split(client);
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);

    tokio::time::timeout(RESPONSE_TIMEOUT, async {
        writer.write_all(&payload).await?;
        writer.flush().await?;
        let line = read_line_bounded(&mut reader, MAX_LINE)
            .await?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "worker closed the pipe")
            })?;
        let response: Response = serde_json::from_str(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok::<_, io::Error>(response)
    })
    .await
    .map_err(|_| Error::Ipc("worker did not respond in time".into()))?
    .map_err(Error::Io)?
    .into_result()
}

/// Whether the worker answers a ping.
pub async fn ping(pipe_name: &str) -> bool {
    request_value(pipe_name, &Request::Ping).await.is_ok()
}

fn is_retryable_connect(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2) | Some(231))
}

async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> io::Result<Option<String>> {
    let mut buffer = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buffer.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            buffer.extend_from_slice(&available[..=position]);
            reader.consume(position + 1);
            break;
        }
        buffer.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
        if buffer.len() > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC message is too large",
            ));
        }
    }

    let text = String::from_utf8(buffer)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some(text.trim_end_matches(['\r', '\n']).to_string()))
}

fn create_pipe(name: &str, first: bool) -> Result<NamedPipeServer> {
    let sid = current_user_sid()
        .map_err(|error| Error::Ipc(format!("cannot determine the current user SID: {error}")))?;
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})");
    let descriptor = SecurityDescriptor::from_sddl(&sddl)?;

    let mut attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };

    let mut options = ServerOptions::new();
    options.first_pipe_instance(first);
    options.pipe_mode(PipeMode::Byte);

    // Safety: `attributes` points at a live SECURITY_ATTRIBUTES whose
    // descriptor outlives this call; CreateNamedPipeW copies the descriptor.
    let server = unsafe {
        options.create_with_security_attributes_raw(
            OsStr::new(name),
            &mut attributes as *mut _ as *mut c_void,
        )
    }
    .map_err(|error| {
        if error.raw_os_error() == Some(5) {
            Error::Conflict("another process is already serving the tgdlrs pipe".into())
        } else {
            Error::Ipc(format!("cannot create named pipe {name}: {error}"))
        }
    })?;
    drop(descriptor);
    Ok(server)
}

struct SecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self> {
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR =
            std::ptr::null_mut();
        let mut size = 0u32;
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                1,
                &mut descriptor,
                &mut size,
            )
        };
        if ok == 0 {
            return Err(Error::Ipc(format!(
                "cannot build the pipe security descriptor: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(
                    self.0 as windows_sys::Win32::Foundation::HLOCAL,
                );
            }
        }
    }
}

fn current_user_sid() -> io::Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            let error = io::Error::last_os_error();
            CloseHandle(token);
            return Err(error);
        }

        let mut buffer = vec![0u8; needed as usize];
        if GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        ) == 0
        {
            let error = io::Error::last_os_error();
            CloseHandle(token);
            return Err(error);
        }
        CloseHandle(token);

        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut string_sid: windows_sys::core::PWSTR = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut string_sid) == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut length = 0usize;
        while *string_sid.add(length) != 0 {
            length += 1;
        }
        let slice = std::slice::from_raw_parts(string_sid, length);
        let text = String::from_utf16_lossy(slice);
        windows_sys::Win32::Foundation::LocalFree(
            string_sid as windows_sys::Win32::Foundation::HLOCAL,
        );
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl RequestHandler for Echo {
        async fn handle(&self, request: Request) -> Response {
            match request {
                Request::Ping => Response::ok(serde_json::json!({ "pong": true })),
                Request::Stop => Response::ok(serde_json::json!({ "stopping": true })),
                _ => Response::error(&Error::InvalidInput("unsupported".into())),
            }
        }
    }

    #[tokio::test]
    async fn round_trips_requests_over_the_pipe() {
        let name = format!(r"\\.\pipe\tgdlrs-test-{}", std::process::id());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server = tokio::spawn(serve(name.clone(), Arc::new(Echo), shutdown_rx));

        // Wait for the first instance to exist.
        let value = request_value(&name, &Request::Ping).await.unwrap();
        assert_eq!(value["pong"], serde_json::json!(true));

        let error = request_value(&name, &Request::Status).await.unwrap_err();
        assert!(matches!(error, Error::InvalidInput(_)));

        let _ = request_value(&name, &Request::Stop).await.unwrap();
        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }
}
