use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

/// Global stream ID counter (monotonically increasing)
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a new unique stream ID.
pub(crate) fn next_stream_id() -> u64 {
    NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed)
}

/// Encode a binary frame: [8-byte stream_id big-endian][chunk_data]
pub(crate) fn encode_binary_frame(stream_id: u64, chunk: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + chunk.len());
    frame.extend_from_slice(&stream_id.to_be_bytes());
    frame.extend_from_slice(chunk);
    frame
}

/// Decode a binary frame: extract stream_id and chunk data.
/// Returns None if data is too short (< 8 bytes).
pub(crate) fn decode_binary_frame(data: &[u8]) -> Option<(u64, &[u8])> {
    if data.len() < 8 {
        return None;
    }
    let stream_id = u64::from_be_bytes(data[..8].try_into().ok()?);
    Some((stream_id, &data[8..]))
}

/// Verify a SHA-256 checksum against data.
/// Expected format: "sha256:<hex>"
pub(crate) fn verify_checksum(data: &[u8], expected: &str) -> bool {
    if let Some(expected_hex) = expected.strip_prefix("sha256:") {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        let computed_hex = hex::encode(result);
        computed_hex == expected_hex
    } else {
        false
    }
}

/// Compute SHA-256 checksum of data.
/// Returns "sha256:<hex>"
pub(crate) fn compute_checksum(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    format!("sha256:{}", hex::encode(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_binary_frame() {
        let stream_id = 42u64;
        let chunk = b"hello world";
        let frame = encode_binary_frame(stream_id, chunk);
        assert_eq!(frame.len(), 8 + chunk.len());

        let (decoded_id, decoded_chunk) = decode_binary_frame(&frame).unwrap();
        assert_eq!(decoded_id, stream_id);
        assert_eq!(decoded_chunk, chunk);
    }

    #[test]
    fn test_decode_binary_frame_too_short() {
        assert!(decode_binary_frame(&[1, 2, 3]).is_none());
        assert!(decode_binary_frame(&[]).is_none());
    }

    #[test]
    fn test_checksum_round_trip() {
        let data = b"test data for checksum";
        let checksum = compute_checksum(data);
        assert!(checksum.starts_with("sha256:"));
        assert!(verify_checksum(data, &checksum));
    }

    #[test]
    fn test_checksum_mismatch() {
        let data = b"original data";
        let checksum = compute_checksum(data);
        assert!(!verify_checksum(b"different data", &checksum));
    }

    #[test]
    fn test_stream_id_monotonic() {
        let id1 = next_stream_id();
        let id2 = next_stream_id();
        assert!(id2 > id1);
    }
}
