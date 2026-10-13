//! Minimal RFC 8489 (STUN) Binding codec: enough for server-reflexive address
//! discovery against Remoti's own responder or any public STUN server.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
pub const HEADER_LEN: usize = 20;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

pub type TransactionId = [u8; 12];

pub fn encode_request(txid: &TransactionId) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Message length 0: no attributes.
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..20].copy_from_slice(txid);
    out
}

fn header(buf: &[u8], expected_type: u16) -> Option<(TransactionId, &[u8])> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if msg_type != expected_type || cookie != MAGIC_COOKIE || buf.len() < HEADER_LEN + len || len % 4 != 0 {
        return None;
    }
    let mut txid = [0u8; 12];
    txid.copy_from_slice(&buf[8..20]);
    Some((txid, &buf[HEADER_LEN..HEADER_LEN + len]))
}

/// Parses a Binding request, returning its transaction id.
pub fn parse_request(buf: &[u8]) -> Option<TransactionId> {
    header(buf, BINDING_REQUEST).map(|(t, _)| t)
}

/// Encodes a Binding success response carrying XOR-MAPPED-ADDRESS.
pub fn encode_response(txid: &TransactionId, addr: SocketAddr) -> Vec<u8> {
    let mut value = Vec::with_capacity(20);
    value.push(0);
    let xport = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
    match addr.ip() {
        IpAddr::V4(ip) => {
            value.push(0x01);
            value.extend_from_slice(&xport.to_be_bytes());
            let x = u32::from(ip) ^ MAGIC_COOKIE;
            value.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(ip) => {
            value.push(0x02);
            value.extend_from_slice(&xport.to_be_bytes());
            let mut key = [0u8; 16];
            key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            key[4..].copy_from_slice(txid);
            for (o, k) in ip.octets().iter().zip(key) {
                value.push(o ^ k);
            }
        }
    }
    let mut out = Vec::with_capacity(HEADER_LEN + 4 + value.len());
    out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    out.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(txid);
    out.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(&value);
    out
}

/// Parses a Binding success response for `txid`, returning the mapped address.
pub fn parse_response(buf: &[u8], txid: &TransactionId) -> Option<SocketAddr> {
    let (got, mut attrs) = header(buf, BINDING_SUCCESS)?;
    if &got != txid {
        return None;
    }
    let mut fallback = None;
    while attrs.len() >= 4 {
        let ty = u16::from_be_bytes([attrs[0], attrs[1]]);
        let len = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let padded = (len + 3) & !3;
        if attrs.len() < 4 + padded {
            return None;
        }
        let value = &attrs[4..4 + len];
        match ty {
            ATTR_XOR_MAPPED_ADDRESS => return decode_address(value, Some(txid)),
            ATTR_MAPPED_ADDRESS => fallback = decode_address(value, None),
            _ => {}
        }
        attrs = &attrs[4 + padded..];
    }
    fallback
}

fn decode_address(v: &[u8], xor_txid: Option<&TransactionId>) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let family = v[1];
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    if xor_txid.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    match (family, v.len()) {
        (0x01, 8) => {
            let mut ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
            if xor_txid.is_some() {
                ip ^= MAGIC_COOKIE;
            }
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
        }
        (0x02, 20) => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&v[4..20]);
            if let Some(txid) = xor_txid {
                let mut key = [0u8; 16];
                key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                key[4..].copy_from_slice(txid);
                for (b, k) in o.iter_mut().zip(key) {
                    *b ^= k;
                }
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(o)), port))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_v4_and_v6() {
        let txid = [5u8; 12];
        let req = encode_request(&txid);
        assert_eq!(parse_request(&req), Some(txid));
        for addr in ["203.0.113.7:51820", "[2001:db8::1]:443"] {
            let addr: SocketAddr = addr.parse().unwrap();
            let resp = encode_response(&txid, addr);
            assert_eq!(parse_response(&resp, &txid), Some(addr));
            assert_eq!(parse_response(&resp, &[6u8; 12]), None);
        }
    }

    #[test]
    fn rejects_truncated() {
        let resp = encode_response(&[1; 12], "1.2.3.4:5".parse().unwrap());
        assert_eq!(parse_response(&resp[..resp.len() - 2], &[1; 12]), None);
    }
}
