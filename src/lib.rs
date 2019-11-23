//! A [`futures_codec`](https://crates.io/crates/futures_codec) that encodes and decodes Server-Sent Event/Event Sourcing streams.
//!
//! It emits or serializes full messages, and the meta-messages `retry:` and `id:`.
//!
//! # Examples
//! ```rust,no_run
//! # async fn amain() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//! use sse_codec::{decode_stream, Event};
//! use futures::stream::TryStreamExt; // for try_next()
//!
//! let response = surf::get("https://some-site.com/events").await?;
//! let mut events = decode_stream(response);
//!
//! while let Some(event) = events.try_next().await? {
//!     println!("incoming: {:?}", event);
//!
//!     match event {
//!         Event::LastEventId { id } => {
//!             // change the last event ID
//!         },
//!         Event::Retry { retry } => {
//!             // change a retry timer value or something
//!         }
//!         Event::Message { event, .. } if event == "stop" => {
//!             break;
//!         }
//!         _ => (),
//!     }
//! }
//! # Ok(()) }
//! ```
use bytes::BytesMut;
use futures_codec::{Decoder, Encoder, FramedRead, FramedWrite};
use futures_io::{AsyncRead, AsyncWrite};
use memchr::memchr2;
use std::fmt::Write as _;
use std::{fmt, str::FromStr};

/// An "event", either an incoming message or some meta-action that needs to be applied to the
/// stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// An incoming message.
    Message {
        /// The event type. Defaults to "message" if no event name is provided.
        event: String,
        /// The data for this event.
        data: String,
    },
    /// Set the _last event ID string_.
    ///
    /// See also the [Server-Sent Events spec](https://html.spec.whatwg.org/multipage/server-sent-events.html#concept-event-stream-last-event-id).
    LastEventId {
        /// The value to be set as the event source's last event ID.
        id: String,
    },
    /// Set the _reconnection time_.
    ///
    /// See also the [Server-Sent Events spec](https://html.spec.whatwg.org/multipage/server-sent-events.html#concept-event-stream-reconnection-time).
    Retry {
        /// The new reconnection time in milliseconds.
        retry: u64,
    },
}

impl Event {
    /// Create a server-sent event message.
    pub fn message(event: &str, data: &str) -> Self {
        Event::Message {
            event: event.to_string(),
            data: data.to_string(),
        }
    }

    /// Create a message that sets the last event ID, without emitting an event message.
    pub fn id(id: &str) -> Self {
        Event::LastEventId { id: id.to_string() }
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
        let mut codec = SSECodec::default();
        for line in s.lines() {
            if let Some(message @ Event::Message { .. }) = codec.parse_line(line) {
                return Ok(message);
            }
        }
        Err(Error::IncompleteFrame)
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Message { event, data } => {
                if event != "message" {
                    write!(f, "event: {}\n", &event)?;
                }

                for line in data.lines() {
                    write!(f, "data: {}\n", line)?;
                }
                Ok(())
            },
            Event::Retry { retry } => write!(f, "retry: {}\n", retry),
            Event::LastEventId { id } => {
                if id.is_empty() {
                    write!(f, "id\n")
                } else {
                    write!(f, "id: {}\n", id)
                }
            }
        }
    }
}

/// Encoder/decoder for server-sent event streams.
#[derive(Debug, Default, Clone)]
pub struct SSECodec {
    /// The _last event ID_ buffer.
    id: Option<String>,
    /// The _event type_ buffer.
    event: Option<String>,
    /// The _data_ buffer.
    data: String,
}

impl SSECodec {
    fn take_message(&mut self) -> Option<Event> {
        fn default_event_name() -> String {
            "message".to_string()
        }

        if let Some(id) = self.id.take() {
            // Set the last event ID string of the event source to the value of the last event ID buffer.
            //
            // NOTE: In the spec, the last event ID state is maintained and this update happens for
            // every message. However sse-codec does not maintain last event ID state, so instead
            // it emits a LastEventId event whenever it is updated, always separately from the
            // messages themselves.
            Some(Event::LastEventId { id })
        } else if self.data.is_empty() {
            // If the data buffer is an empty string, set the data buffer and the event type buffer to the empty string [and return.]
            self.event.take();
            None
        } else {
            Some(Event::Message {
                event: self.event.take().unwrap_or_else(default_event_name),
                data: std::mem::replace(&mut self.data, String::new()),
            })
        }
    }

    fn parse_line(&mut self, line: &str) -> Option<Event> {
        let mut parts = line.splitn(2, ":");
        match (parts.next(), parts.next()) {
            // If the field name is "retry":
            (Some("retry"), Some(value)) if value.chars().all(|c| c.is_ascii_digit()) => {
                // If the field value consists of only ASCII digits, then interpret the field value
                // as an integer in base ten, and set the event stream's reconnection time to that
                // integer. Otherwise, ignore the field.
                if let Ok(time) = value.parse::<u64>() {
                    return Some(Event::Retry { retry: time });
                }
            }
            // If the field name is "event":
            (Some("event"), Some(value)) => {
                // Set the event type buffer to field value.
                self.event = Some(strip_leading_space(value).to_string());
            }
            // If the field name is "data":
            (Some("data"), value) => {
                // Append the field value to the data buffer, then append a single U+000A LINE FEED (LF) character to the data buffer.
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                if let Some(value) = value {
                    self.data += strip_leading_space(value);
                }
            }
            // If the field name is "id":
            (Some("id"), Some(id_str)) if !id_str.contains(char::from(0)) => {
                // If the field value does not contain U+0000 NULL, then set the last event ID buffer to the field value.
                // Otherwise, ignore the field.
                self.id = Some(strip_leading_space(id_str).to_string());
            }
            // Comment
            (Some(""), Some(_)) => (),
            // End of frame
            (Some(""), None) => {
                return self.take_message();
            }
            _ => (),
        }
        None
    }
}

impl Decoder for SSECodec {
    type Item = Event;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match memchr2(b'\n', b'\n', src) {
            Some(pos) => {
                let line = src.split_to(pos + 2);
                Ok(self.parse_line(std::str::from_utf8(&line)?))
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
    FramedRead::new(input, SSECodec::default())
}

/// Encode `Event`s into an `AsyncWrite`.
pub fn encode_stream<W: AsyncWrite>(output: W) -> FramedWrite<W, SSECodec> {
    FramedWrite::new(output, SSECodec::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let mut codec = SSECodec::default();
        let mut event = None;
        let s = "event: add\ndata: test\ndata: test2\n\n";
        for line in s.lines() {
            if let Some(message @ Event::Message { .. }) = codec.parse_line(line) {
                event = Some(message);
                break;
            }
        }
        assert_eq!(
            event,
            Some(Event::Message {
                event: "add".to_string(),
                data: "test\ntest2".to_string(),
            })
        );
    }
}
