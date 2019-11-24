//! Talk to a [Signalhub](https://github.com/mafintosh/signalhub) signaling server.
// seems kinda bad that this is needed?
#![type_length_limit = "1059008"]

use futures::{future, TryStreamExt};
use sse_codec::{decode_stream, Event};
use std::time::Duration;

/// Listen for messages from the server-sent event source.
async fn listen() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response = surf::get("https://signalhub-jccqtwhdwc.now.sh/v1/sse-codec/example").await?;
    let mut events = decode_stream(response);

    while let Some(line) = events.try_next().await? {
        println!("incoming: {:?}", line);

        if let Event::Message { data, .. } = line {
            if data == "stop" {
                break;
            }
        }
    }

    Ok(())
}

/// Send a message to the server for broadcast.
async fn publish() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    async_std::task::sleep(Duration::from_secs(1)).await;

    surf::post("https://signalhub-jccqtwhdwc.now.sh/v1/sse-codec/example")
        .body_bytes(br#"{"event": "sample"}"#)
        .await?;

    surf::post("https://signalhub-jccqtwhdwc.now.sh/v1/sse-codec/example")
        .body_bytes(b"stop")
        .await?;

    Ok(())
}

/// "async main"
async fn amain() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (a, b) = future::join(listen(), publish()).await;
    // check the results…
    let _ = a?;
    let _ = b?;
    Ok(())
}

fn main() {
    async_std::task::block_on(async {
        amain().await.unwrap();
    });
}
