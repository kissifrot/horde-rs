use super::Error;
use crate::BoundedLatency;
use hdrhistogram::{
    Histogram,
    serialization::{Deserializer, Serializer, V2Serializer},
};
use std::{io::Cursor, time::Duration};

const SLOTS: usize = 56_320;
const ENVELOPE: usize = 44;
const V2_HEADER: usize = 40;
/// Largest possible version-1 envelope, independently of receiver limits.
pub const MAX_ENVELOPE_BYTES: usize = ENVELOPE + V2_HEADER + 9 * SLOTS;

/// Limits are supplied by the receiver, never by the encoded histogram.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    pub max_envelope_bytes: usize,
    pub max_samples: u64,
    pub assigned_requests: Option<u64>,
}

/// An approximate histogram whose metadata and counts have been checked.
#[derive(Clone, Debug)]
pub struct DistributedHistogram(BoundedLatency);

impl DistributedHistogram {
    pub fn count(&self) -> u64 {
        self.0.len()
    }
    pub fn min(&self) -> Option<Duration> {
        (self.count() > 0).then(|| self.0.min())
    }
    pub fn max(&self) -> Option<Duration> {
        (self.count() > 0).then(|| self.0.max())
    }
    pub fn mean_ms(&self) -> Option<f64> {
        (self.count() > 0).then(|| self.0.total_seconds() / self.count() as f64 * 1000.0)
    }
    /// Nearest-rank percentile; the returned HDR value is an upper bound.
    pub fn percentile(&self, percent: u8) -> Option<Duration> {
        if self.count() == 0 || !(1..=100).contains(&percent) {
            return None;
        }
        let rank = ((u128::from(self.count()) * u128::from(percent)).div_ceil(100)) as u64;
        Some(self.0.rank(rank))
    }
    /// Check all sums before merging; never average percentile values.
    pub fn merge(&mut self, other: &Self) -> Result<(), Error> {
        self.0.merge_checked(&other.0).map_err(Error)
    }
}

/// Serialize a local bounded collector. `None` represents an empty result.
pub fn encode_histogram(
    histogram: Option<&BoundedLatency>,
    completed: u64,
) -> Result<Vec<u8>, Error> {
    let empty;
    let histogram = match histogram {
        Some(h) => h,
        None => {
            empty = BoundedLatency::new();
            &empty
        }
    };
    if histogram.len() != completed {
        return Err(Error("completed count does not match histogram"));
    }
    let (min, max, sum) = if completed == 0 {
        (0, 0, 0)
    } else {
        let min =
            u64::try_from(histogram.min().as_nanos()).map_err(|_| Error("minimum out of range"))?;
        let max =
            u64::try_from(histogram.max().as_nanos()).map_err(|_| Error("maximum out of range"))?;
        let sum = histogram.total_seconds();
        if !sum.is_finite() || sum < 0.0 {
            return Err(Error("invalid latency sum"));
        }
        (min, max, sum.to_bits())
    };
    let mut v2 = Vec::new();
    V2Serializer::new()
        .serialize(histogram.histogram(), &mut v2)
        .map_err(|_| Error("histogram encoding failed"))?;
    if v2.len() > MAX_ENVELOPE_BYTES - ENVELOPE {
        return Err(Error("histogram too large"));
    }
    let mut out = Vec::with_capacity(ENVELOPE + v2.len());
    out.extend_from_slice(b"HLDH");
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&u16::from(completed > 0).to_be_bytes());
    out.extend_from_slice(&completed.to_be_bytes());
    out.extend_from_slice(&min.to_be_bytes());
    out.extend_from_slice(&max.to_be_bytes());
    out.extend_from_slice(&sum.to_be_bytes());
    out.extend_from_slice(&(v2.len() as u32).to_be_bytes());
    out.extend_from_slice(&v2);
    Ok(out)
}

fn u16_at(data: &[u8], i: usize) -> u16 {
    u16::from_be_bytes(data[i..i + 2].try_into().expect("checked header"))
}
fn u32_at(data: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(data[i..i + 4].try_into().expect("checked header"))
}
fn u64_at(data: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(data[i..i + 8].try_into().expect("checked header"))
}

// Match HDR V2's nine-byte zigzag encoding without allocating or negating i64::MIN.
fn preflight(data: &[u8], ceiling: u64) -> Result<u64, Error> {
    let (mut pos, mut slot, mut total) = (0usize, 0usize, 0u64);
    while pos < data.len() {
        let mut value = 0u64;
        for i in 0..9 {
            let byte = *data.get(pos).ok_or(Error("truncated histogram integer"))?;
            pos += 1;
            if i == 8 {
                value |= u64::from(byte) << 56;
                break;
            }
            value |= u64::from(byte & 0x7f) << (i * 7);
            if byte & 0x80 == 0 {
                break;
            }
        }
        if value & 1 != 0 {
            if value == u64::MAX {
                return Err(Error("invalid histogram zero run"));
            }
            let run = (value >> 1) + 1;
            if run > SLOTS as u64 {
                return Err(Error("histogram slot limit exceeded"));
            }
            slot = slot
                .checked_add(run as usize)
                .ok_or(Error("histogram slot overflow"))?;
        } else {
            total = total
                .checked_add(value >> 1)
                .ok_or(Error("histogram count overflow"))?;
            if total > ceiling {
                return Err(Error("histogram sample limit exceeded"));
            }
            slot = slot
                .checked_add(1)
                .ok_or(Error("histogram slot overflow"))?;
        }
        if slot > SLOTS {
            return Err(Error("histogram slot limit exceeded"));
        }
    }
    Ok(total)
}

/// Validate an already bounded slice before invoking the library decoder.
pub fn decode_histogram(
    data: &[u8],
    completed: u64,
    limits: DecodeLimits,
) -> Result<DistributedHistogram, Error> {
    if data.len() < ENVELOPE + V2_HEADER
        || data.len() > limits.max_envelope_bytes.min(MAX_ENVELOPE_BYTES)
    {
        return Err(Error("invalid histogram envelope length"));
    }
    if &data[..4] != b"HLDH" || u16_at(data, 4) != 1 {
        return Err(Error("unsupported histogram envelope"));
    }
    let flags = u16_at(data, 6);
    let count = u64_at(data, 8);
    let min = u64_at(data, 16);
    let max = u64_at(data, 24);
    let sum_bits = u64_at(data, 32);
    let sum = f64::from_bits(sum_bits);
    if flags != u16::from(count > 0) || count != completed {
        return Err(Error("inconsistent histogram count or flags"));
    }
    if count == 0 && (min != 0 || max != 0 || sum_bits != 0) {
        return Err(Error("invalid empty histogram metadata"));
    }
    if count > 0
        && (min > max
            || !sum.is_finite()
            || sum < 0.0
            || !(sum / count as f64 * 1000.0).is_finite())
    {
        return Err(Error("invalid histogram metadata"));
    }
    let ceiling = limits
        .assigned_requests
        .unwrap_or(u64::MAX)
        .min(limits.max_samples);
    if count > ceiling {
        return Err(Error("histogram sample limit exceeded"));
    }
    let v2 = &data[ENVELOPE..];
    if u32_at(data, 40) as usize != v2.len() {
        return Err(Error("invalid histogram payload length"));
    }
    if u32_at(v2, 0) != 0x1c849313
        || u32_at(v2, 4) as usize != v2.len() - V2_HEADER
        || u32_at(v2, 8) != 0
        || u32_at(v2, 12) != 3
        || u64_at(v2, 16) != 1
        || u64_at(v2, 24) != u64::MAX
        || u64_at(v2, 32) != 1f64.to_bits()
    {
        return Err(Error("unsupported histogram configuration"));
    }
    if preflight(&v2[V2_HEADER..], ceiling)? != count {
        return Err(Error("histogram count mismatch"));
    }
    let mut cursor = Cursor::new(v2);
    let histogram: Histogram<u64> = Deserializer::new()
        .deserialize(&mut cursor)
        .map_err(|_| Error("histogram decoding failed"))?;
    if cursor.position() != v2.len() as u64 || histogram.len() != count {
        return Err(Error("histogram decoding mismatch"));
    }
    if count > 0
        && (histogram.count_at(min) == 0
            || histogram.count_at(max) == 0
            || histogram.lowest_equivalent(min) != histogram.lowest_equivalent(histogram.min())
            || histogram.lowest_equivalent(max) != histogram.lowest_equivalent(histogram.max()))
    {
        return Err(Error("histogram extrema mismatch"));
    }
    Ok(DistributedHistogram(BoundedLatency::from_validated(
        histogram,
        if count == 0 {
            Duration::MAX
        } else {
            Duration::from_nanos(min)
        },
        Duration::from_nanos(max),
        sum,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> DecodeLimits {
        DecodeLimits {
            max_envelope_bytes: MAX_ENVELOPE_BYTES,
            max_samples: u64::MAX,
            assigned_requests: None,
        }
    }
    fn sample(values: &[u64]) -> (BoundedLatency, Vec<u8>) {
        let mut h = BoundedLatency::new();
        for &n in values {
            h.record(Duration::from_nanos(n)).unwrap();
        }
        let bytes = encode_histogram(Some(&h), values.len() as u64).unwrap();
        (h, bytes)
    }
    fn payload(bytes: &mut Vec<u8>, payload: &[u8]) {
        bytes.truncate(ENVELOPE + V2_HEADER);
        bytes.extend_from_slice(payload);
        bytes[40..44].copy_from_slice(&((V2_HEADER + payload.len()) as u32).to_be_bytes());
        bytes[48..52].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    }
    #[test]
    fn round_trip_empty_extrema_and_merge() {
        for values in [
            &[][..],
            &[0][..],
            &[u64::MAX][..],
            &[0, 1, 500, 700_000, u64::MAX][..],
        ] {
            let (original, bytes) = sample(values);
            assert_eq!(&bytes[..4], b"HLDH");
            let result = decode_histogram(&bytes, values.len() as u64, limits()).unwrap();
            assert_eq!(result.count(), values.len() as u64);
            assert_eq!(
                result.0.total_seconds().to_bits(),
                original.total_seconds().to_bits()
            );
            for &v in values {
                assert_eq!(
                    result.0.histogram().count_at(v),
                    original.histogram().count_at(v)
                );
            }
            if !values.is_empty() {
                assert_eq!(result.min(), Some(original.min()));
                assert_eq!(result.max(), Some(original.max()));
            }
        }
        let (_, a) = sample(&[1, 2, 3]);
        let (_, b) = sample(&[10, 20]);
        let mut merged = decode_histogram(&a, 3, limits()).unwrap();
        merged
            .merge(&decode_histogram(&b, 2, limits()).unwrap())
            .unwrap();
        assert_eq!(merged.count(), 5);
        assert_eq!(merged.percentile(50), Some(Duration::from_nanos(3)));
        assert_eq!(merged.percentile(100), Some(Duration::from_nanos(20)));
    }
    #[test]
    fn reject_bad_headers_lengths_and_metadata() {
        let (_, good) = sample(&[100, 200]);
        let mut infinite_mean = good.clone();
        infinite_mean[32..40].copy_from_slice(&f64::MAX.to_bits().to_be_bytes());
        assert!(decode_histogram(&infinite_mean, 2, limits()).is_err());
        for i in [0, 4, 6, 8, 16, 24, 32, 40, 44, 48, 52, 56, 60, 68, 76] {
            let mut bad = good.clone();
            bad[i] ^= 0x80;
            assert!(decode_histogram(&bad, 2, limits()).is_err(), "offset {i}");
        }
        for len in 0..good.len() {
            assert!(
                decode_histogram(&good[..len], 2, limits()).is_err(),
                "length {len}"
            );
        }
        let mut extra = good.clone();
        extra.push(0);
        assert!(decode_histogram(&extra, 2, limits()).is_err());
        assert!(decode_histogram(&good, 1, limits()).is_err());
        assert!(
            decode_histogram(
                &good,
                2,
                DecodeLimits {
                    max_samples: 1,
                    ..limits()
                }
            )
            .is_err()
        );
        assert!(
            decode_histogram(
                &good,
                2,
                DecodeLimits {
                    assigned_requests: Some(1),
                    ..limits()
                }
            )
            .is_err()
        );
        assert!(
            decode_histogram(
                &good,
                2,
                DecodeLimits {
                    max_envelope_bytes: good.len() - 1,
                    ..limits()
                }
            )
            .is_err()
        );
    }
    #[test]
    fn reject_malicious_counts_before_library() {
        assert!(preflight(&[0x80], u64::MAX).is_err());
        assert!(preflight(&[0xff; 9], u64::MAX).is_err());
        assert!(preflight(&vec![0; SLOTS + 1], u64::MAX).is_err());
        assert_eq!(preflight(&vec![0; SLOTS], u64::MAX).unwrap(), 0);
        assert_eq!(preflight(&[0x80, 0], u64::MAX).unwrap(), 0);
        assert!(preflight(&[2], 0).is_err());
        let (_, mut bytes) = sample(&[]);
        payload(&mut bytes, &[0xff; 9]);
        assert!(decode_histogram(&bytes, 0, limits()).is_err());
    }
    #[test]
    fn accepted_preflights_agree_with_library_for_generated_payloads() {
        let (_, empty) = sample(&[]);
        let mut state = 0x3141_5926_5358_9793u64;
        for _ in 0..2000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state as usize) % 80;
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes.push(state as u8);
            }
            if let Ok(expected) = preflight(&bytes, u64::MAX) {
                let mut frame = empty.clone();
                payload(&mut frame, &bytes);
                let v2 = &frame[ENVELOPE..];
                let mut cursor = Cursor::new(v2);
                let h: Histogram<u64> = Deserializer::new()
                    .deserialize(&mut cursor)
                    .expect("accepted preflight must decode");
                assert_eq!(h.len(), expected);
                assert_eq!(cursor.position(), v2.len() as u64);
            }
        }
    }

    #[test]
    fn all_slots_and_merge_overflow() {
        let mut histogram = Histogram::<u64>::new_with_bounds(1, u64::MAX, 3).unwrap();
        let representatives: Vec<_> = histogram
            .iter_all()
            .map(|entry| entry.value_iterated_to())
            .collect();
        assert_eq!(representatives.len(), SLOTS);
        for value in representatives {
            histogram.record(value).unwrap();
        }
        let original = BoundedLatency::from_validated(
            histogram,
            Duration::ZERO,
            Duration::from_nanos(u64::MAX),
            1.0,
        );
        let bytes = encode_histogram(Some(&original), SLOTS as u64).unwrap();
        let decoded = decode_histogram(&bytes, SLOTS as u64, limits()).unwrap();
        assert_eq!(decoded.count(), SLOTS as u64);

        let mut histogram = Histogram::<u64>::new_with_bounds(1, u64::MAX, 3).unwrap();
        histogram.record_n(1, i64::MAX as u64).unwrap();
        let h = BoundedLatency::from_validated(
            histogram,
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            1.0,
        );
        let encoded = encode_histogram(Some(&h), i64::MAX as u64).unwrap();
        let incoming = decode_histogram(&encoded, i64::MAX as u64, limits()).unwrap();
        let mut aggregate = incoming.clone();
        aggregate.merge(&incoming).unwrap();
        aggregate.merge(&incoming).unwrap_err();
        assert_eq!(aggregate.count(), (i64::MAX as u64) * 2);
    }

    #[test]
    fn differential_bucket_counts_and_percentiles_for_generated_histograms() {
        let mut state = 0x1234_5678_9abc_def0u64;
        for size in [1, 2, 20, 200, 2000] {
            let mut values = Vec::new();
            for _ in 0..size {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                values.push(state % 10_000_000);
            }
            let (original, bytes) = sample(&values);
            let decoded = decode_histogram(&bytes, size, limits()).unwrap();
            for (a, b) in original
                .histogram()
                .iter_all()
                .zip(decoded.0.histogram().iter_all())
            {
                assert_eq!(a.count_at_value(), b.count_at_value());
            }
            values.sort_unstable();
            for p in [1u8, 50, 90, 95, 99, 100] {
                let rank = (values.len() * p as usize).div_ceil(100);
                let result = decoded.percentile(p).unwrap().as_nanos();
                assert!(result >= values[rank - 1] as u128);
                assert_eq!(result, original.rank(rank as u64).as_nanos());
            }
        }
    }

    #[test]
    fn noncanonical_nine_byte_integer_matches_library() {
        let (_, mut bytes) = sample(&[0]);
        payload(
            &mut bytes,
            &[0x82, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0],
        );
        let decoded = decode_histogram(&bytes, 1, limits()).unwrap();
        assert_eq!(decoded.0.histogram().count_at(0), 1);
        assert_eq!(decoded.percentile(95), Some(Duration::ZERO));
    }

    #[test]
    fn golden_single_zero_sample() {
        let (_, bytes) = sample(&[0]);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"HLDH\x00\x01\x00\x01");
        expected.extend_from_slice(&1u64.to_be_bytes());
        expected.extend_from_slice(&[0; 24]);
        expected.extend_from_slice(&41u32.to_be_bytes());
        expected.extend_from_slice(&0x1c849313u32.to_be_bytes());
        expected.extend_from_slice(&1u32.to_be_bytes());
        expected.extend_from_slice(&0u32.to_be_bytes());
        expected.extend_from_slice(&3u32.to_be_bytes());
        expected.extend_from_slice(&1u64.to_be_bytes());
        expected.extend_from_slice(&u64::MAX.to_be_bytes());
        expected.extend_from_slice(&1f64.to_bits().to_be_bytes());
        expected.push(2);
        assert_eq!(bytes, expected);
        assert_eq!(decode_histogram(&expected, 1, limits()).unwrap().count(), 1);
    }

    #[test]
    fn mutated_envelopes_never_panic_and_accepted_buckets_match_library() {
        let (_, seed) = sample(&[0, 1, 1000, 100_000, u64::MAX]);
        let mut state = 0x8627_b1ed_7463_951au64;
        for _ in 0..20_000 {
            let mut bytes = seed.clone();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let changes = (state as usize % 4) + 1;
            for _ in 0..changes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let at = state as usize % bytes.len();
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes[at] ^= (state as u8) | 1;
            }
            if let Ok(decoded) = decode_histogram(&bytes, 5, limits()) {
                let mut cursor = Cursor::new(&bytes[ENVELOPE..]);
                let direct: Histogram<u64> = Deserializer::new().deserialize(&mut cursor).unwrap();
                assert_eq!(direct.len(), decoded.count());
                for (left, right) in direct.iter_all().zip(decoded.0.histogram().iter_all()) {
                    assert_eq!(left.count_at_value(), right.count_at_value());
                }
            }
        }
    }
}
