//! LSP Transport Layer
//!
//! Handles LSP message framing with Content-Length headers.
//! Provides async read/write with proper buffering.

use std::io;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

use super::protocol::{Message, Notification, Request};

/// LSP Transport for reading/writing messages
pub struct Transport {
    reader: BufReader<ChildStdout>,
}

impl Transport {
    /// Create a new transport from stdout
    pub fn new(stdout: ChildStdout) -> Self {
        Self {
            reader: BufReader::new(stdout),
        }
    }

    /// Read the next message from the server
    ///
    /// LSP messages are framed with headers:
    /// ```text
    /// Content-Length: 123\r\n
    /// \r\n
    /// {"jsonrpc":"2.0",...}
    /// ```
    pub async fn read_message(&mut self) -> io::Result<Message> {
        // Read headers
        let content_length = self.read_headers().await?;

        // Read body
        let mut body = vec![0u8; content_length];
        self.reader.read_exact(&mut body).await?;

        let json =
            String::from_utf8(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        tracing::trace!("LSP <- {}", json);

        Message::parse(&json).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Read headers and return Content-Length
    async fn read_headers(&mut self) -> io::Result<usize> {
        let mut content_length: Option<usize> = None;
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = self.reader.read_line(&mut line).await?;

            if bytes_read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Server closed connection",
                ));
            }

            let line = line.trim();

            // Empty line marks end of headers
            if line.is_empty() {
                break;
            }

            // Parse Content-Length header
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = Some(
                    value
                        .trim()
                        .parse()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                );
            }
            // Ignore other headers (Content-Type, etc.)
        }

        content_length
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Missing Content-Length"))
    }
}

/// Write an LSP request to stdin
pub async fn write_request(stdin: &mut ChildStdin, request: &Request) -> io::Result<()> {
    let json = serde_json::to_string(request)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    write_message(stdin, &json).await
}

/// Write an LSP notification to stdin
pub async fn write_notification(
    stdin: &mut ChildStdin,
    notification: &Notification,
) -> io::Result<()> {
    let json = serde_json::to_string(notification)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    write_message(stdin, &json).await
}

/// Write an LSP response to stdin (for server-initiated requests)
pub async fn write_response(
    stdin: &mut ChildStdin,
    response: &super::protocol::Response,
) -> io::Result<()> {
    let json = serde_json::to_string(response)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    write_message(stdin, &json).await
}

/// Write a raw JSON message with LSP framing
async fn write_message(stdin: &mut ChildStdin, json: &str) -> io::Result<()> {
    tracing::trace!("LSP -> {}", json);

    let message = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
    stdin.write_all(message.as_bytes()).await?;
    stdin.flush().await
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_message_framing() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
        assert!(framed.starts_with("Content-Length: 40"));
        assert!(framed.contains("\r\n\r\n"));
    }
}
