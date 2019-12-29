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
                    writeln!(f, "event: {}", &event)?;
                }

                for line in data.lines() {
                    writeln!(f, "data: {}", line)?;
                }
                Ok(())
            }
            Event::Retry { retry } => writeln!(f, "retry: {}", retry),
            Event::LastEventId { id } => {
                if id.is_empty() {
                    writeln!(f, "id")
                } else {
                    writeln!(f, "id: {}", id)
                }
            }
        }
    }
}

/// Encoder/decoder for server-sent event streams.
#[derive(Debug, Default, Clone)]
pub struct SSECodec {
    /// Have we processed the optional Byte Order Marker on the first line?
    processed_bom: bool,
    /// Was the last character of the previous line a \r?
    last_was_cr: bool,
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
            if self.data.ends_with('\n') {
                self.data.pop();
            }
            Some(Event::Message {
                event: self.event.take().unwrap_or_else(default_event_name),
                data: std::mem::replace(&mut self.data, String::new()),
            })
        }
    }

    fn parse_line(&mut self, line: &str) -> Option<Event> {
        let mut parts = line.splitn(2, ':');
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
                // Append the field value to the data buffer,
                if let Some(value) = value {
                    self.data += strip_leading_space(value);
                }
                // then append a single U+000A LINE FEED (LF) character to the data buffer.
                self.data.push('\n');
            }
            // If the field name is "id":
            (Some("id"), Some(id_str)) if !id_str.contains(char::from(0)) => {
                // If the field value does not contain U+0000 NULL, then set the last event ID buffer to the field value.
                // Otherwise, ignore the field.
                self.id = Some(strip_leading_space(id_str).to_string());
                return self.take_message();
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
        while let Some(pos) = memchr2(b'\r', b'\n', src) {
            let line = src.split_to(pos + 1);

            // treat \r\n as one newline
            if pos == 0 && line == "\n" && self.last_was_cr {
                self.last_was_cr = false;
                continue;
            }
            self.last_was_cr = line.last() == Some(&b'\r');

            // get rid of the '\n' at the end
            let line = std::str::from_utf8(&line[..pos])?;
            // get rid of the BOM at the start
            let line = if line.starts_with("\u{feff}") && !self.processed_bom {
                self.processed_bom = true;
                &line[3..]
            } else {
                line
            };
            if let Some(event) = self.parse_line(line) {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

impl Encoder for SSECodec {
    type Item = Event;
    type Error = Error;

    fn encode(&mut self, item: Self::Item, dest: &mut BytesMut) -> Result<(), Self::Error> {
        writeln!(dest, "{}", item).map_err(Into::into)
    }
}

/// Type of a decoding stream, returned from `decode_stream()`.
pub type DecodeStream<R> = FramedRead<R, SSECodec>;

/// Type of an encoding stream, returned from `encode_stream()`.
pub type EncodeStream<W> = FramedWrite<W, SSECodec>;

/// Parse messages from an `AsyncRead`, returning a stream of `Event`s.
pub fn decode_stream<R: AsyncRead>(input: R) -> DecodeStream<R> {
    FramedRead::new(input, SSECodec::default())
}

/// Encode `Event`s into an `AsyncWrite`.
pub fn encode_stream<W: AsyncWrite>(output: W) -> EncodeStream<W> {
    FramedWrite::new(output, SSECodec::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_event() {
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

#[cfg(test)]
mod wpt {
    //! EventSource tests from the web-platform-tests suite. See https://github.com/web-platform-tests/wpt/tree/master/eventsource

    use super::*;
    use futures::stream::StreamExt;

    struct DecodeIter<'a> {
        inner: FramedRead<&'a [u8], SSECodec>,
    }
    impl Iterator for DecodeIter<'_> {
        type Item = Result<Event, Error>;
        fn next(&mut self) -> Option<Self::Item> {
            let mut result = None;
            async_std::task::block_on(async {
                result = self.inner.next().await;
            });
            result
        }
    }

    fn decode(input: &[u8]) -> DecodeIter<'_> {
        DecodeIter {
            inner: decode_stream(input),
        }
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/event-data.html
    #[test]
    fn data() {
        let input = concat!(
            "data:msg\n",
            "data:msg\n",
            "\n",
            ":\n",
            "falsefield:msg\n",
            "\n",
            "falsefield:msg\n",
            "Data:data\n",
            "\n",
            "data\n",
            "\n",
            "data:end\n",
            "\n",
        );
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "msg\nmsg".into()
            })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "".into()
            })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "end".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-bom.htm
    /// The byte order marker should only be stripped at the very start.
    #[test]
    fn bom() {
        let mut input = vec![];
        input.extend(b"\xEF\xBB\xBF");
        input.extend(b"data:1\n");
        input.extend(b"\n");
        input.extend(b"\xEF\xBB\xBF");
        input.extend(b"data:2\n");
        input.extend(b"\n");
        input.extend(b"data:3\n");
        input.extend(b"\n");
        let mut messages = decode(&input);
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "1".into()
            })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "3".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-bom-2.htm
    /// Only _one_ byte order marker should be stripped. This has two, which means one will remain
    /// in the first line, therefore making the first `data:1` invalid.
    #[test]
    fn bom2() {
        let mut input = vec![];
        input.extend(b"\xEF\xBB\xBF");
        input.extend(b"\xEF\xBB\xBF");
        input.extend(b"data:1\n");
        input.extend(b"\n");
        input.extend(b"data:2\n");
        input.extend(b"\n");
        input.extend(b"data:3\n");
        input.extend(b"\n");
        let mut messages = decode(&input);
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "2".into()
            })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "3".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-comments.htm
    #[test]
    fn comments() {
        let longstring = "x".repeat(2049);
        let mut input = concat!("data:1\r", ":\0\n", ":\r\n", "data:2\n", ":").to_string();
        input.push_str(&longstring);
        input.push_str("\r");
        input.push_str("data:3\n");
        input.push_str(":data:fail\r");
        input.push_str(":");
        input.push_str(&longstring);
        input.push_str("\n");
        input.push_str("data:4\n\n");
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "1\n2\n3\n4".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-data-before-final-empty-line.htm
    #[test]
    fn data_before_final_empty_line() {
        let input = "retry:1000\ndata:test1\n\nid:test\ndata:test2";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Retry { retry: 1000 })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "test1".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-data.htm
    #[test]
    fn field_data() {
        let input = "data:\n\ndata\ndata\n\ndata:test\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "".into()
            })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "\n".into()
            })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "test".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-event-empty.htm
    #[test]
    fn field_event_empty() {
        let input = "event: \ndata:data\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "".into(),
                data: "data".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-event.htm
    #[test]
    fn field_event() {
        let input = "event:test\ndata:x\n\ndata:x\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "test".into(),
                data: "x".into()
            })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "x".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-id.htm
    #[test]
    #[ignore]
    fn field_id() {
        unimplemented!()
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-id-2.htm
    #[test]
    #[ignore]
    fn field_id_2() {
        unimplemented!()
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-parsing.htm
    #[test]
    fn field_parsing() {
        let input = "data:\0\ndata:  2\rData:1\ndata\0:2\ndata:1\r\0data:4\nda-ta:3\rdata_5\ndata:3\rdata:\r\n data:32\ndata:4\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "\0\n 2\n1\n3\n\n4".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-retry-bogus.htm
    #[test]
    fn field_retry_bogus() {
        let input = "retry:3000\nretry:1000x\ndata:x\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Retry { retry: 3000 })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "x".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-retry-empty.htm
    #[test]
    fn field_retry_empty() {
        let input = "retry\ndata:test\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "test".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-retry.htm
    #[test]
    fn field_retry() {
        let input = "retry:03000\ndata:x\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Retry { retry: 3000 })
        );
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "x".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-field-unknown.htm
    #[test]
    fn field_unknown() {
        let input =
            "data:test\n data\ndata\nfoobar:xxx\njustsometext\n:thisisacommentyay\ndata:test\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "test\n\ntest".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-leading-space.htm
    #[test]
    fn leading_space() {
        let input = "data:\ttest\rdata: \ndata:test\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "\ttest\n\ntest".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-newlines.htm
    #[test]
    fn newlines() {
        let input = "data:test\r\ndata\ndata:test\r\n\r";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "test\n\ntest".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-null-character.html
    #[test]
    fn null_character() {
        let input = "data:\0\n\n\n\n";
        let mut messages = decode(input.as_bytes());
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "\0".into()
            })
        );
        assert!(messages.next().is_none());
    }

    /// https://github.com/web-platform-tests/wpt/blob/master/eventsource/format-utf-8.htm
    #[test]
    fn utf_8() {
        let input = b"data:ok\xE2\x80\xA6\n\n";
        let mut messages = decode(input);
        assert_eq!(
            messages.next().map(Result::unwrap),
            Some(Event::Message {
                event: "message".into(),
                data: "ok…".into()
            })
        );
        assert!(messages.next().is_none());
    }

    #[test]
    fn decode_stream_when_fed_by_line() {
        use futures::stream::{self, StreamExt, TryStreamExt};

        let input: Vec<&str> = vec![":ok", "", "event:message", "id:id1", "data:data1", ""];

        let body_stream = stream::iter(input).map(|i| Ok(i.to_owned() + "\n"));

        let messages = decode_stream(body_stream.into_async_read());

        let mut result = None;
        async_std::task::block_on(async {
            result = Some(messages.map(|i| i.unwrap()).collect::<Vec<_>>().await);
        });

        let results = result.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results.get(0).unwrap(), &Event::id("id1"));
        assert_eq!(results.get(1).unwrap(), &Event::message("message", "data1"));
    }
}
