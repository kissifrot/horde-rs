#![no_main]
use hdrhistogram::{
    Histogram,
    serialization::{Serializer, V2Serializer},
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut histogram = Histogram::<u64>::new_with_bounds(1, u64::MAX, 3).unwrap();
    let mut values = Vec::new();
    for chunk in data.chunks_exact(8).take(128) {
        let value = u64::from_le_bytes(chunk.try_into().unwrap());
        histogram.record(value).unwrap();
        values.push(value);
    }
    let count = values.len() as u64;
    let min = values.iter().copied().min().unwrap_or(0);
    let max = values.iter().copied().max().unwrap_or(0);
    let sum = values
        .iter()
        .map(|v| std::time::Duration::from_nanos(*v).as_secs_f64())
        .fold(0.0_f64, |sum, value| sum + value);
    let mut v2 = Vec::new();
    V2Serializer::new().serialize(&histogram, &mut v2).unwrap();
    let mut frame = Vec::new();
    frame.extend_from_slice(b"HLDH\x00\x01");
    frame.extend_from_slice(&u16::from(count > 0).to_be_bytes());
    frame.extend_from_slice(&count.to_be_bytes());
    frame.extend_from_slice(&min.to_be_bytes());
    frame.extend_from_slice(&max.to_be_bytes());
    frame.extend_from_slice(&sum.to_bits().to_be_bytes());
    frame.extend_from_slice(&(v2.len() as u32).to_be_bytes());
    frame.extend_from_slice(&v2);
    horde_fuzz::check_frame(&frame, true);
});
