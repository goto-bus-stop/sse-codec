//! Serialize some Server-Sent Event messages to standard output.
use futures::SinkExt;
use sse_codec::{encode_stream, Event};

async fn amain() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let output = async_std::io::stdout();
    let mut stream = encode_stream(output);

    stream.send(Event::message("message", "test", None)).await?;
    stream
        .send(Event::message(
            "something-else",
            "some-data\non\nmultiple-lines",
            None,
        ))
        .await?;

    Ok(())
}

fn main() {
    async_std::task::block_on(async {
        amain().await.unwrap();
    });
}
