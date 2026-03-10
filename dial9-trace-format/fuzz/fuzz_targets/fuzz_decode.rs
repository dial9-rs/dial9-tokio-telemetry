#![no_main]
use dial9_trace_format::decoder::Decoder;
use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes to the decoder — it must never panic.
fuzz_target!(|data: &[u8]| {
    if let Some(mut dec) = Decoder::new(data) {
        let _ = dec.decode_all();
    }
});
