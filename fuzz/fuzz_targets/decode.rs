#![no_main]
use libfuzzer_sys::fuzz_target;
use futures::prelude::*;

fuzz_target!(|data: &[u8]| {
    futures::executor::block_on(async {
        let events = sse_codec::decode_stream(data);
        events.for_each(|_| async {}).await;
    });
});
