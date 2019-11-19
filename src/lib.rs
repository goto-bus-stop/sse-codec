//! A [`futures_codec`](https://crates.io/crates/futures_codec) that encodes and decodes Server-Sent Event/Event Sourcing streams.
use bytes::BytesMut;
use futures_codec::{Decoder, Encoder, FramedRead, FramedWrite};
use futures_io::{AsyncRead, AsyncWrite};
use memchr::memchr2;
use std::fmt::Write as _;
use std::{fmt, str::FromStr};

///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEvent {
    ///
    pub id: Option<String>,
    /// The event type. Defaults to "message" if no event name is provided.
    pub event: String,
    /// The data for this event.
    pub data: String,
}

impl Default for MessageEvent {
    fn default() -> Self {
        Self {
            id: None,
            event: "message".to_string(),
            data: Default::default(),
        }
    }
}

///
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Message(MessageEvent),
    Retry { retry: u64 },
}

impl Event {
    /// Create a server-sent event message.
    pub fn message(id: Option<&str>, event: &str, data: &str) -> Self {
        Event::Message(MessageEvent {
            id: id.map(ToString::to_string),
            event: event.to_string(),
            data: data.to_string(),
        })
    }

    /// Create a message that configures the retry timeout.
    pub fn retry(time: u64) -> Self {
        Event::Retry { retry: time }
    }
}

/// Errors that may occur while encoding or decoding server-sent event messages.
#[derive(Debug)]
pub enum Error {
    /// An I/O error occurred while reading or writing a stream.
    IoError(std::io::Error),
    /// Incoming data is not valid utf-8.
    Utf8Error(std::str::Utf8Error),
    /// An error occurred while writing an event message.
    FmtError(std::fmt::Error),
    /// Tried to read an incomplete frame.
    IncompleteFrame,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::IoError(inner) => inner.fmt(f),
            Error::Utf8Error(inner) => inner.fmt(f),
            Error::FmtError(inner) => inner.fmt(f),
            Error::IncompleteFrame => write!(f, "incomplete frame"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(err)
    }
}

impl From<std::fmt::Error> for Error {
    fn from(err: std::fmt::Error) -> Self {
        Self::FmtError(err)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(err: std::str::Utf8Error) -> Self {
        Self::Utf8Error(err)
    }
}

impl Event {
    fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let s = std::str::from_utf8(bytes)?;
        s.parse()
    }
}

/// Chop off a leading space (code point 0x20) from a string slice.
fn strip_leading_space(input: &str) -> &str {
    if input.starts_with(' ') {
        &input[1..]
    } else {
        input
    }
}

impl FromStr for Event {
    type Err = Error;

    /// Parse an event message from a string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut id = None;
        let mut event = "message";
        let mut data = String::new();
        for line in s.lines() {
            let mut parts = line.splitn(2, ":");
            match (parts.next(), parts.next()) {
                // Set the event type buffer to field value.
                (Some("event"), Some(event_name)) => {
                    event = strip_leading_space(event_name);
                }
                // Append the field value to the data buffer, then append a single U+000A LINE FEED (LF) character to the data buffer.
                (Some("data"), Some(data_line)) => {
                    data += strip_leading_space(data_line);
                    data.push('\n');
                }
                (Some("data"), None) => data.push('\n'),
                (Some("id"), Some(id_str)) => {
                    id = Some(strip_leading_space(id_str).to_string());
                }
                // Comment
                (Some(""), Some(_)) => (),
                // End of frame
                (Some(""), None) => {
                    data.pop();
                    return Ok(Event::Message(MessageEvent {
                        id,
                        event: event.to_string(),
                        data,
                    }));
                }
                _ => (),
            }
        }

        Err(Error::IncompleteFrame)
    }
}

impl fmt::Display for MessageEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.event != "message" {
            write!(f, "event: {}\n", &self.event)?;
        }

        for line in self.data.lines() {
            write!(f, "data: {}\n", line)?;
        }

        if let Some("") = self.id.as_ref().map(String::as_str) {
            write!(f, "id\n")
        } else if let Some(id) = self.id.as_ref() {
            write!(f, "id: {}\n", id)
        } else {
            Ok(())
        }
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Message(m) => m.fmt(f),
            Event::Retry { retry } => write!(f, "retry: {}\n", retry),
        }
    }
}

/// Encoder/decoder for server-sent event streams.
#[derive(Debug, Clone)]
pub struct SSECodec {}

impl Decoder for SSECodec {
    type Item = Event;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match memchr2(b'\n', b'\n', src) {
            Some(pos) => {
                let frame = src.split_to(pos + 2);
                Ok(Some(Event::from_bytes(&frame)?))
            }
            None => Ok(None),
        }
    }
}

impl Encoder for SSECodec {
    type Item = Event;
    type Error = Error;

    fn encode(&mut self, item: Self::Item, dest: &mut BytesMut) -> Result<(), Self::Error> {
        write!(dest, "{}\n", item).map_err(Into::into)
    }
}

/// Parse messages from an `AsyncRead`, returning a stream of `Event`s.
pub fn decode_stream<R: AsyncRead>(input: R) -> FramedRead<R, SSECodec> {
    FramedRead::new(input, SSECodec {})
}

/// Encode `Event`s into an `AsyncWrite`.
pub fn encode_stream<W: AsyncWrite>(output: W) -> FramedWrite<W, SSECodec> {
    FramedWrite::new(output, SSECodec {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let event: Event = "event: add\ndata: test\ndata: test2\n\n".parse().unwrap();
        assert_eq!(
            event,
            Event::Message(MessageEvent {
                id: None,
                event: "add".to_string(),
                data: "test\ntest2".to_string(),
            })
        );
    }
}
