# sse-codec change log

All notable changes to this project will be documented in this file.

This project adheres to [Semantic Versioning](http://semver.org/).

## 0.3.0
* Make `id` part of the `Event::Message` event, removing the separate non-spec `Event::LastEventId` message.
* Ignore trailing data in the input stream.
* Use `futures_codec` 0.4.0.

## 0.2.0
* Fix messages being dropped when parsing a stream. (@bekh6ex, #1)

## 0.1.0
* Initial release.
