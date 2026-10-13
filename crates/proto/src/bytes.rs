use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Byte buffer that serializes as unpadded base64 in human-readable formats
/// (JSON signaling) and as raw bytes in binary formats (postcard).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Self {
        Self(v.to_vec())
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never dump key material or payloads into logs.
        write!(f, "Bytes({} bytes)", self.0.len())
    }
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&STANDARD_NO_PAD.encode(&self.0))
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
            STANDARD_NO_PAD.decode(s.trim_end_matches('=').as_bytes()).map(Bytes).map_err(serde::de::Error::custom)
        } else {
            let v = serde_bytes_vec(deserializer)?;
            Ok(Bytes(v))
        }
    }
}

fn serde_bytes_vec<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a byte buffer")
        }
        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
            Ok(v.to_vec())
        }
        fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
            Ok(v)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
            while let Some(b) = seq.next_element::<u8>()? {
                out.push(b);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_bytes(V)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_is_base64() {
        let b = Bytes(vec![1, 2, 3, 250]);
        let s = serde_json::to_string(&b).unwrap();
        assert_eq!(s, "\"AQID+g\"");
        let back: Bytes = serde_json::from_str(&s).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn postcard_roundtrip_is_raw() {
        let b = Bytes(vec![9; 300]);
        let enc = postcard::to_stdvec(&b).unwrap();
        assert_eq!(enc.len(), 302);
        let back: Bytes = postcard::from_bytes(&enc).unwrap();
        assert_eq!(back, b);
    }
}
