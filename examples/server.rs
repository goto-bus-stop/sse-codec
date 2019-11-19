//! Serialize some Server-Sent Event messages to standard output.
use futures::SinkExt;
use sse_codec::{encode_stream, Event};

async fn amain() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let output = async_std::io::stdout();
    let mut stream = encode_stream(output);

    stream.send(Event::message(None, "message", "test")).await?;
    stream
        .send(Event::message(
            None,
            "something-else",
            "some-data\non\nmultiple-lines",
        ))
        .await?;

    Ok(())
}

fn main() {
    async_std::task::block_on(async {
        amain().await.unwrap();
    });
}
