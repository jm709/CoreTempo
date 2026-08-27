//! Standard base64 (RFC 4648, padded) — the encoding the PTY SSE stream's
//! chunks use. Hand-rolled for the reason the server hand-rolls the encoder
//! (`core::api::sse::b64_encode`): ~30 lines beats a new dependency.

fn sextet(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// The bytes `text` encodes, or `None` when it is not standard padded base64.
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let quanta = bytes.len() / 4;
    let mut out = Vec::with_capacity(quanta * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
        // Padding says "this quantum is short", so it can only end the input.
        if pad > 2 || (pad > 0 && index + 1 != quanta) {
            return None;
        }
        let mut n = 0u32;
        for (position, c) in chunk.iter().enumerate() {
            let value = if position >= 4 - pad { 0 } else { sextet(*c)? };
            n = (n << 6) | value;
        }
        // n < 2^24, so the big-endian quad is [0, byte, byte, byte].
        let quad = n.to_be_bytes();
        out.push(quad[1]);
        if pad < 2 {
            out.push(quad[2]);
        }
        if pad < 1 {
            out.push(quad[3]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn rfc4648_vectors_decode() {
        assert_eq!(decode(""), Some(Vec::new()));
        assert_eq!(decode("Zg=="), Some(b"f".to_vec()));
        assert_eq!(decode("Zm8="), Some(b"fo".to_vec()));
        assert_eq!(decode("Zm9v"), Some(b"foo".to_vec()));
        assert_eq!(decode("Zm9vYmFy"), Some(b"foobar".to_vec()));
        assert_eq!(decode("/wAb"), Some(vec![0xff, 0x00, 0x1b]));
        assert_eq!(decode("Zm9v!"), None);
        assert_eq!(decode("Zm9"), None, "length must be a multiple of 4");
    }

    #[test]
    fn padding_only_ever_ends_the_last_quantum() {
        assert_eq!(decode("Zg==Zg=="), None, "a padded chunk must be the last");
        assert_eq!(decode("===="), None, "a quantum is at least one sextet");
        assert_eq!(decode("Z==="), None, "three pad bytes encode nothing");
    }
}
