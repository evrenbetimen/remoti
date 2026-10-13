//! Conversion from length-prefixed NAL units (AVCC/HVCC sample format used by
//! VideoToolbox and some Media Foundation encoders) to Annex B byte streams.

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Appends `nal` to `out` with a 4-byte start code.
pub fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

/// Converts `src` (big-endian `len_size`-byte length before every NAL unit) to
/// Annex B, appending to `out`. Returns `false` on a truncated or malformed
/// buffer, leaving `out` with whatever complete units preceded the error.
pub fn from_length_prefixed(src: &[u8], len_size: usize, out: &mut Vec<u8>) -> bool {
    if !(1..=4).contains(&len_size) {
        return false;
    }
    let mut rest = src;
    while !rest.is_empty() {
        if rest.len() < len_size {
            return false;
        }
        let (len_bytes, tail) = rest.split_at(len_size);
        let len = len_bytes.iter().fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        if len == 0 || len > tail.len() {
            return false;
        }
        let (nal, next) = tail.split_at(len);
        push_nal(out, nal);
        rest = next;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_multiple_units() {
        let src = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x41];
        let mut out = Vec::new();
        assert!(from_length_prefixed(&src, 4, &mut out));
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn rejects_truncation_and_bad_sizes() {
        let mut out = Vec::new();
        assert!(!from_length_prefixed(&[0, 0, 0, 5, 1, 2], 4, &mut out));
        assert!(!from_length_prefixed(&[0, 0], 4, &mut out));
        assert!(!from_length_prefixed(&[1, 2], 0, &mut out));
        assert!(!from_length_prefixed(&[0, 0, 0, 0], 4, &mut out));
        let mut out = Vec::new();
        assert!(from_length_prefixed(&[0, 1, 9], 2, &mut out));
        assert_eq!(out, [0, 0, 0, 1, 9]);
    }
}
