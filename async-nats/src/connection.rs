use std::str::FromStr;
use subslice::SubsliceExt;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::io::{AsyncReadExt, AsyncWrite};

use bytes::{Buf, BytesMut};
use tokio::io;

use crate::header::{HeaderMap, HeaderName, HeaderValue};
use crate::ClientOp;
use crate::ServerError;
use crate::ServerOp;

/// Supertrait enabling trait object for containing both TLS and non TLS `TcpStream` connection.
pub(crate) trait AsyncReadWrite: AsyncWrite + AsyncRead + Send + Unpin {}

/// Blanked implementation that applies to both TLS and non-TLS `TcpStream`.
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// A framed connection
pub(crate) struct Connection {
    pub(crate) stream: Box<dyn AsyncReadWrite>,
    pub(crate) buffer: BytesMut,
}

/// Internal representation of the connection.
/// Helds connection with NATS Server and communicates with `Client` via channels.
impl Connection {
    pub(crate) fn try_read_op(&mut self) -> Result<Option<ServerOp>, io::Error> {
        if self.buffer.starts_with(b"+OK\r\n") {
            self.buffer.advance(5);
            return Ok(Some(ServerOp::Ok));
        }

        if self.buffer.starts_with(b"PING\r\n") {
            self.buffer.advance(6);

            return Ok(Some(ServerOp::Ping));
        }

        if self.buffer.starts_with(b"PONG\r\n") {
            self.buffer.advance(6);

            return Ok(Some(ServerOp::Pong));
        }

        if self.buffer.starts_with(b"-ERR") {
            if let Some(len) = self.buffer.find(b"\r\n") {
                let line = std::str::from_utf8(&self.buffer[5..len])
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
                let error_message = line.trim_matches('\'').to_string();
                self.buffer.advance(len + 2);

                return Ok(Some(ServerOp::Error(ServerError::new(error_message))));
            }
        }

        if self.buffer.starts_with(b"INFO ") {
            if let Some(len) = self.buffer.find(b"\r\n") {
                let line = std::str::from_utf8(&self.buffer[5..len])
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

                let server_info = serde_json::from_str(line)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

                self.buffer.advance(len + 2);

                return Ok(Some(ServerOp::Info(Box::new(server_info))));
            }

            return Ok(None);
        }

        if self.buffer.starts_with(b"MSG ") {
            if let Some(len) = self.buffer.find(b"\r\n") {
                let line = std::str::from_utf8(&self.buffer[4..len]).unwrap();
                let args = line.split(' ').filter(|s| !s.is_empty());
                // TODO(caspervonb) we can drop this alloc
                let args = args.collect::<Vec<_>>();

                // Parse the operation syntax: MSG <subject> <sid> [reply-to] <#bytes>
                let (subject, sid, reply_to, payload_len) = match args[..] {
                    [subject, sid, payload_len] => (subject, sid, None, payload_len),
                    [subject, sid, reply_to, payload_len] => {
                        (subject, sid, Some(reply_to), payload_len)
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "invalid number of arguments after MSG",
                        ));
                    }
                };

                let sid = u64::from_str(sid)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

                // Parse the number of payload bytes.
                let payload_len = usize::from_str(payload_len)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

                // Only advance if there is enough data for the entire operation and payload remaining.
                if len + payload_len + 4 <= self.buffer.remaining() {
                    let subject = subject.to_owned();
                    let reply_to = reply_to.map(String::from);

                    self.buffer.advance(len + 2);
                    let payload = self.buffer.split_to(payload_len).freeze();
                    self.buffer.advance(2);

                    return Ok(Some(ServerOp::Message {
                        sid,
                        reply: reply_to,
                        headers: None,
                        subject,
                        payload,
                    }));
                }
            }

            return Ok(None);
        }

        if self.buffer.starts_with(b"HMSG ") {
            if let Some(len) = self.buffer.find(b"\r\n") {
                // Extract whitespace-delimited arguments that come after "HMSG".
                let line = std::str::from_utf8(&self.buffer[5..len]).unwrap();
                let args = line.split_whitespace().filter(|s| !s.is_empty());
                let args = args.collect::<Vec<_>>();

                // <subject> <sid> [reply-to] <# header bytes><# total bytes>
                let (subject, sid, reply_to, num_header_bytes, num_bytes) = match args[..] {
                    [subject, sid, num_header_bytes, num_bytes] => {
                        (subject, sid, None, num_header_bytes, num_bytes)
                    }
                    [subject, sid, reply_to, num_header_bytes, num_bytes] => {
                        (subject, sid, Some(reply_to), num_header_bytes, num_bytes)
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "invalid number of arguments after HMSG",
                        ));
                    }
                };

                // Convert the slice into an owned string.
                let subject = subject.to_string();

                // Parse the subject ID.
                let sid = u64::from_str(sid).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot parse sid argument after HMSG",
                    )
                })?;

                // Convert the slice into an owned string.
                let reply_to = reply_to.map(ToString::to_string);

                // Parse the number of payload bytes.
                let num_header_bytes = usize::from_str(num_header_bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot parse the number of header bytes argument after \
                     HMSG",
                    )
                })?;

                // Parse the number of payload bytes.
                let num_bytes = usize::from_str(num_bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot parse the number of bytes argument after HMSG",
                    )
                })?;

                if num_bytes < num_header_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "number of header bytes was greater than or equal to the \
                 total number of bytes after HMSG",
                    ));
                }

                // Only advance if there is enough data for the entire operation and payload remaining.
                if len + num_bytes + 4 <= self.buffer.remaining() {
                    self.buffer.advance(len + 2);
                    let buffer = self.buffer.split_to(num_header_bytes).freeze();
                    let payload = self.buffer.split_to(num_bytes - num_header_bytes).freeze();

                    let mut lines = std::str::from_utf8(&buffer).unwrap().lines().peekable();

                    let version_line = lines.next().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "no header version line found")
                    })?;

                    if !version_line.starts_with("NATS/1.0") {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "header version line does not begin with nats/1.0",
                        ));
                    }

                    let mut headers = HeaderMap::new();
                    while let Some(line) = lines.next() {
                        if line.is_empty() {
                            continue;
                        }

                        let (key, value) = line.split_once(':').ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "no header version line found",
                            )
                        })?;

                        let mut value = String::from_str(value).unwrap();
                        while let Some(v) = lines.next_if(|s| s.starts_with(char::is_whitespace)) {
                            value.push_str(v);
                        }

                        headers.append(
                            HeaderName::from_str(key).unwrap(),
                            HeaderValue::from_str(&value).unwrap(),
                        );
                    }

                    return Ok(Some(ServerOp::Message {
                        sid,
                        reply: reply_to,
                        subject,
                        headers: Some(headers),
                        payload,
                    }));
                }
            }

            return Ok(None);
        }

        Ok(None)
    }

    pub(crate) async fn read_op(&mut self) -> Result<Option<ServerOp>, io::Error> {
        loop {
            if let Some(op) = self.try_read_op()? {
                return Ok(Some(op));
            }

            if 0 == self.stream.read_buf(&mut self.buffer).await? {
                if self.buffer.is_empty() {
                    return Ok(None);
                } else {
                    return Err(io::Error::new(io::ErrorKind::ConnectionReset, ""));
                }
            }
        }
    }

    pub(crate) async fn write_op(&mut self, item: ClientOp) -> Result<(), io::Error> {
        match item {
            ClientOp::Connect(connect_info) => {
                let op = format!(
                    "CONNECT {}\r\n",
                    serde_json::to_string(&connect_info)
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?
                );
                self.stream.write_all(op.as_bytes()).await?;
            }
            ClientOp::Publish {
                subject,
                payload,
                respond,
                headers,
            } => {
                if headers.is_some() {
                    self.stream.write_all(b"HPUB ").await?;
                } else {
                    self.stream.write_all(b"PUB ").await?;
                }

                self.stream.write_all(subject.as_bytes()).await?;
                self.stream.write_all(b" ").await?;

                if let Some(respond) = respond {
                    self.stream.write_all(respond.as_bytes()).await?;
                    self.stream.write_all(b" ").await?;
                }

                if let Some(headers) = headers {
                    let mut header = Vec::new();
                    header.extend_from_slice(b"NATS/1.0\r\n");
                    for (key, value) in headers.iter() {
                        header.extend_from_slice(key.as_ref());
                        header.push(b':');
                        header.extend_from_slice(value.as_ref());
                        header.extend_from_slice(b"\r\n");
                    }

                    header.extend_from_slice(b"\r\n");

                    let mut header_len_buf = itoa::Buffer::new();
                    self.stream
                        .write_all(header_len_buf.format(header.len()).as_bytes())
                        .await?;

                    self.stream.write_all(b" ").await?;

                    let mut total_len_buf = itoa::Buffer::new();
                    self.stream
                        .write_all(
                            total_len_buf
                                .format(header.len() + payload.len())
                                .as_bytes(),
                        )
                        .await?;

                    self.stream.write_all(b"\r\n").await?;
                    self.stream.write_all(&header).await?;
                } else {
                    let mut len_buf = itoa::Buffer::new();
                    self.stream
                        .write_all(len_buf.format(payload.len()).as_bytes())
                        .await?;
                    self.stream.write_all(b"\r\n").await?;
                }

                self.stream.write_all(&payload).await?;
                self.stream.write_all(b"\r\n").await?;
            }

            ClientOp::Subscribe {
                sid,
                subject,
                queue_group,
            } => {
                self.stream.write_all(b"SUB ").await?;
                self.stream.write_all(subject.as_bytes()).await?;
                if let Some(queue_group) = queue_group {
                    self.stream
                        .write_all(format!(" {}", queue_group).as_bytes())
                        .await?;
                }
                self.stream
                    .write_all(format!(" {}\r\n", sid).as_bytes())
                    .await?;
                self.stream.flush().await?;
            }

            ClientOp::Unsubscribe { sid, max } => {
                self.stream.write_all(b"UNSUB ").await?;
                self.stream.write_all(format!("{}", sid).as_bytes()).await?;
                if let Some(max) = max {
                    self.stream
                        .write_all(format!(" {}", max).as_bytes())
                        .await?;
                }
                self.stream.write_all(b"\r\n").await?;
            }
            ClientOp::Ping => {
                self.stream.write_all(b"PING\r\n").await?;
                self.stream.flush().await?;
            }
            ClientOp::Pong => {
                self.stream.write_all(b"PONG\r\n").await?;
                self.stream.flush().await?;
            }
            ClientOp::TryFlush => {
                self.stream.flush().await?;
            }
        }

        Ok(())
    }

    pub(crate) async fn flush(&mut self) -> Result<(), io::Error> {
        self.stream.flush().await
    }
}