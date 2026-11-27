use std::io;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::OwnedMutexGuard;

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

        tracing::trace!(bytes = json.len(), "Received LSP frame");

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

/// What the client sees of a server taking its input: whether a frame is
/// waiting for the server to take it, and when the server last took any.
/// A pipe takes what fits in its buffer, so "taken" is what the client can
/// see: the pipe accepting bytes because the server made room for them.
#[derive(Clone, Default)]
pub struct Intake(Arc<std::sync::Mutex<IntakeState>>);

#[derive(Default)]
struct IntakeState {
    waiting: bool,
    last_taken: Option<Instant>,
}

impl Intake {
    /// How long the server has taken none of its input, where a frame is
    /// waiting for it and it has taken none since `since`, the start of the
    /// work asking: a server that took nothing for all of that work, with
    /// input in front of it, has stopped reading as far as the client can
    /// tell. One that took some, or had nothing waiting, has not been seen
    /// to.
    pub fn untaken_since(&self, since: Instant) -> Option<std::time::Duration> {
        let state = self.0.lock().expect("intake lock poisoned");
        let last = state.last_taken;
        (state.waiting && last.is_none_or(|taken| taken <= since))
            .then(|| last.unwrap_or(since).elapsed())
    }

    fn set_waiting(&self, waiting: bool) {
        self.0.lock().expect("intake lock poisoned").waiting = waiting;
    }

    fn took(&self) {
        self.0.lock().expect("intake lock poisoned").last_taken = Some(Instant::now());
    }
}

/// Write an LSP request to stdin
pub async fn write_request(
    stdin: OwnedMutexGuard<Option<ChildStdin>>,
    intake: &Intake,
    request: &Request,
) -> io::Result<()> {
    let json = serde_json::to_string(request)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    write_message(stdin, intake, json).await
}

/// Write an LSP notification to stdin
pub async fn write_notification(
    stdin: OwnedMutexGuard<Option<ChildStdin>>,
    intake: &Intake,
    notification: &Notification,
) -> io::Result<()> {
    let json = serde_json::to_string(notification)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    write_message(stdin, intake, json).await
}

pub async fn write_notifications(
    stdin: OwnedMutexGuard<Option<ChildStdin>>,
    intake: &Intake,
    notifications: &[Notification],
) -> io::Result<()> {
    let mut frames = String::new();
    for notification in notifications {
        let json = serde_json::to_string(notification).map_err(io::Error::other)?;
        frames.push_str(&format!("Content-Length: {}\r\n\r\n{}", json.len(), json));
    }
    write_frames(stdin, intake, frames).await
}

/// Write an LSP response to stdin (for server-initiated requests)
pub async fn write_response(
    stdin: OwnedMutexGuard<Option<ChildStdin>>,
    intake: &Intake,
    response: &super::protocol::Response,
) -> io::Result<()> {
    let json = serde_json::to_string(response)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    write_message(stdin, intake, json).await
}

/// Write a raw JSON message with LSP framing
async fn write_message<W>(
    stdin: OwnedMutexGuard<Option<W>>,
    intake: &Intake,
    json: String,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let message = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
    write_frames(stdin, intake, message).await
}

/// Write whole frames, holding the stream until the server has taken them.
///
/// The write is a task of its own, so a caller that stops waiting — its
/// budget spent, its future dropped — leaves the frames to finish rather
/// than half sent. Nothing bounds how long the server takes to read them: a
/// server busy with one long request reads nothing meanwhile, and a server
/// slow to read has not exited. Only a failed write ends the stream, since a
/// frame cut short there leaves nothing a later frame could follow. What the
/// server takes, and when, is told to `intake` as the write goes.
async fn write_frames<W>(
    mut stdin: OwnedMutexGuard<Option<W>>,
    intake: &Intake,
    message: String,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tracing::trace!(bytes = message.len(), "Writing LSP frames");
    let intake = intake.clone();
    tokio::spawn(async move {
        let stream = stdin.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Language server stdin is closed",
            )
        })?;
        intake.set_waiting(true);
        let mut result = Ok(());
        let mut rest = message.as_bytes();
        while !rest.is_empty() {
            match stream.write(rest).await {
                Ok(0) => {
                    result = Err(io::ErrorKind::WriteZero.into());
                    break;
                }
                Ok(taken) => {
                    rest = &rest[taken..];
                    intake.took();
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            result = stream.flush().await;
        }
        intake.set_waiting(false);
        if result.is_err() {
            stdin.take();
        }
        result
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn cancelling_a_writer_keeps_frames_whole_and_ordered() {
        let (stream, mut reader) = tokio::io::duplex(8);
        let writer = Arc::new(Mutex::new(Some(stream)));
        let body = "x".repeat(1024);
        let first = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let send = {
            let writer = Arc::clone(&writer);
            tokio::spawn(async move {
                write_message(writer.lock_owned().await, &Intake::default(), body).await
            })
        };
        let mut received = vec![0; first.len()];
        reader.read_exact(&mut received[..1]).await.unwrap();
        send.abort();
        assert!(send.await.unwrap_err().is_cancelled());
        let next = tokio::spawn(async move {
            write_message(
                writer.lock_owned().await,
                &Intake::default(),
                "{}".to_string(),
            )
            .await
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            reader.read_exact(&mut received[1..]),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(received, first.as_bytes());
        let mut second = vec![0; "Content-Length: 2\r\n\r\n{}".len()];
        tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut second))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, b"Content-Length: 2\r\n\r\n{}");
        next.await.unwrap().unwrap();
    }

    /// A server that takes its input late — busy with one long request — is
    /// still sent the whole frame, and the stream stays open for the next.
    /// Meanwhile the frame is seen waiting: untaken since the work began
    /// before it, not since work that began after the server last took some.
    #[tokio::test]
    async fn a_frame_read_late_is_finished_after_its_caller_stops_waiting() {
        let (stream, mut reader) = tokio::io::duplex(1);
        let writer = Arc::new(Mutex::new(Some(stream)));
        let intake = Intake::default();
        let frame = b"Content-Length: 2\r\n\r\n{}";
        let began = Instant::now();
        let waited = tokio::time::timeout(
            Duration::from_millis(50),
            write_message(
                Arc::clone(&writer).lock_owned().await,
                &intake,
                "{}".to_string(),
            ),
        )
        .await;
        assert!(waited.is_err(), "a one-byte pipe holds no whole frame");
        assert!(intake.untaken_since(began).is_none(), "it took a byte");
        let after = Instant::now();
        assert!(intake.untaken_since(after).is_some(), "it takes no more");

        let mut received = vec![0; frame.len()];
        tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, frame);
        let stream = tokio::time::timeout(Duration::from_secs(5), writer.lock())
            .await
            .unwrap();
        assert!(stream.is_some(), "the stream outlives a slow read");
        assert!(intake.untaken_since(after).is_none(), "nothing waits");
    }

    #[tokio::test]
    async fn a_failed_write_closes_the_stream() {
        let (stream, reader) = tokio::io::duplex(1);
        drop(reader);
        let writer = Arc::new(Mutex::new(Some(stream)));
        let intake = Intake::default();
        let error = write_message(
            Arc::clone(&writer).lock_owned().await,
            &intake,
            "{}".to_string(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(writer.lock().await.is_none());
    }

    #[test]
    fn test_message_framing() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
        assert!(framed.starts_with("Content-Length: 40"));
        assert!(framed.contains("\r\n\r\n"));
    }
}
