use hdrhistogram::{Histogram, serialization::Deserializer};
use horde_rs::distributed::{DecodeLimits, MAX_ENVELOPE_BYTES, decode_histogram};
use std::io::Cursor;

pub fn check_frame(data: &[u8], required: bool) {
    let count = data
        .get(8..16)
        .map(|bytes| u64::from_be_bytes(bytes.try_into().unwrap()))
        .unwrap_or(0);
    let limits = DecodeLimits {
        max_envelope_bytes: MAX_ENVELOPE_BYTES,
        max_samples: u64::MAX,
        assigned_requests: None,
    };
    let decoded = decode_histogram(data, count, limits);
    if required {
        assert!(decoded.is_ok(), "generated frame rejected: {decoded:?}");
    }
    let Ok(decoded) = decoded else { return };
    assert_eq!(decoded.count(), count);
    assert!(decode_histogram(data, count.wrapping_add(1), limits).is_err());
    assert!(
        decode_histogram(
            data,
            count,
            DecodeLimits {
                max_envelope_bytes: data.len() - 1,
                ..limits
            }
        )
        .is_err()
    );
    if count > 0 {
        assert!(
            decode_histogram(
                data,
                count,
                DecodeLimits {
                    max_samples: count - 1,
                    ..limits
                }
            )
            .is_err()
        );
        assert!(
            decode_histogram(
                data,
                count,
                DecodeLimits {
                    assigned_requests: Some(count - 1),
                    ..limits
                }
            )
            .is_err()
        );
    }
    let mut cursor = Cursor::new(&data[44..]);
    let direct: Histogram<u64> = Deserializer::new().deserialize(&mut cursor).unwrap();
    assert_eq!(cursor.position() as usize, data.len() - 44);
    assert_eq!(direct.len(), count);
    if count == 0 {
        assert_eq!(decoded.min(), None);
        assert_eq!(decoded.max(), None);
        assert_eq!(decoded.percentile(50), None);
        return;
    }
    let min = decoded.min().unwrap().as_nanos() as u64;
    let max = decoded.max().unwrap().as_nanos() as u64;
    assert_eq!(
        direct.lowest_equivalent(min),
        direct.lowest_equivalent(direct.min())
    );
    assert_eq!(
        direct.lowest_equivalent(max),
        direct.lowest_equivalent(direct.max())
    );
    assert!(decoded.mean_ms().unwrap().is_finite());
    for percent in [1u8, 50, 90, 95, 99, 100] {
        let rank = (u128::from(count) * u128::from(percent)).div_ceil(100) as u64;
        let mut cumulative = 0u64;
        let bucket = direct
            .iter_recorded()
            .find(|entry| {
                cumulative += entry.count_at_value();
                cumulative >= rank
            })
            .unwrap();
        let expected = if direct.lowest_equivalent(bucket.value_iterated_to())
            == direct.lowest_equivalent(max)
        {
            max
        } else {
            bucket.value_iterated_to().min(max)
        };
        assert_eq!(
            decoded.percentile(percent).unwrap().as_nanos(),
            u128::from(expected)
        );
    }
}
