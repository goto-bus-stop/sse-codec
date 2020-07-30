#![no_main]
use libfuzzer_sys::fuzz_target;
use futures::prelude::*;

fuzz_target!(|data: Vec<sse_codec::Event>| {
    futures::executor::block_on(async {
        let mut encoder = sse_codec::encode_stream(vec![]);
        for event in data {
            encoder.send(event).await.unwrap();
        }
    });
});
