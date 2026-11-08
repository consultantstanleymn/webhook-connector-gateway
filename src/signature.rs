//! HMAC-SHA256 request signatures with replay protection.
//!
//! Header format: `t=<unix seconds>,v1=<hex hmac>`. The signed text is
//! `"<t>.<raw body>"`. The header can hold several `v1` values, so a sender
//! can sign with the old and the new secret during a rotation.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Default accepted clock difference between sender and gateway.
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("signature header is malformed")]
    Malformed,
    #[error("timestamp is outside the allowed window")]
    TimestampOutOfWindow,
    #[error("signature does not match")]
    Mismatch,
}

fn mac_for(secret: &[u8], ts: i64, body: &[u8]) -> HmacSha256 {
    // HMAC accepts keys of any length, so this cannot fail.
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(ts.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// Hex HMAC for the given timestamp and body.
pub fn compute(secret: &[u8], ts: i64, body: &[u8]) -> String {
    hex::encode(mac_for(secret, ts, body).finalize().into_bytes())
}

/// Full header value, ready to send.
pub fn header_value(secret: &[u8], ts: i64, body: &[u8]) -> String {
    format!("t={},v1={}", ts, compute(secret, ts, body))
}

/// Parse `t=...,v1=...` into a timestamp and the decoded signatures.
pub fn parse_header(header: &str) -> Result<(i64, Vec<Vec<u8>>), SignatureError> {
    let mut ts = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        let (k, v) = part
            .trim()
            .split_once('=')
            .ok_or(SignatureError::Malformed)?;
        match k {
            "t" => ts = Some(v.parse::<i64>().map_err(|_| SignatureError::Malformed)?),
            "v1" => sigs.push(hex::decode(v).map_err(|_| SignatureError::Malformed)?),
            _ => {} // ignore unknown keys so the format can grow
        }
    }
    match (ts, sigs.is_empty()) {
        (Some(t), false) => Ok((t, sigs)),
        _ => Err(SignatureError::Malformed),
    }
}

/// Verify a header against the accepted secrets.
///
/// `now` and `tolerance` are in seconds. The compare uses
/// `Mac::verify_slice`, which is constant time.
pub fn verify(
    secrets: &[&[u8]],
    header: &str,
    body: &[u8],
    now: i64,
    tolerance: i64,
) -> Result<(), SignatureError> {
    let (ts, sigs) = parse_header(header)?;
    if now.abs_diff(ts) > tolerance.max(0) as u64 {
        return Err(SignatureError::TimestampOutOfWindow);
    }
    // Check every pair, no early exit, so timing does not show which matched.
    let mut ok = false;
    for secret in secrets {
        for sig in &sigs {
            ok |= mac_for(secret, ts, body).verify_slice(sig).is_ok();
        }
    }
    if ok {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn known_vector_roundtrip() {
        let h = header_value(b"s3cret", 1_700_000_000, b"{\"a\":1}");
        assert!(verify(&[b"s3cret"], &h, b"{\"a\":1}", 1_700_000_010, 300).is_ok());
    }

    #[test]
    fn matches_openssl_value() {
        // echo -n '1700000000.hello' | openssl dgst -sha256 -hmac key
        assert_eq!(
            compute(b"key", 1_700_000_000, b"hello"),
            "4d583a269f4f276a3fa80ff31b5a01879a848096983222a17893d198418939aa"
        );
    }

    #[test]
    fn rejects_stale_and_future() {
        let h = header_value(b"k", 1000, b"x");
        assert_eq!(
            verify(&[b"k"], &h, b"x", 1000 + 301, 300),
            Err(SignatureError::TimestampOutOfWindow)
        );
        assert_eq!(
            verify(&[b"k"], &h, b"x", 1000 - 301, 300),
            Err(SignatureError::TimestampOutOfWindow)
        );
        assert!(verify(&[b"k"], &h, b"x", 1000 + 300, 300).is_ok());
    }

    #[test]
    fn accepts_either_secret_during_rotation() {
        let h = header_value(b"old", 50, b"x");
        assert!(verify(&[b"new", b"old"], &h, b"x", 50, 300).is_ok());
        assert_eq!(
            verify(&[b"new"], &h, b"x", 50, 300),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn malformed_headers() {
        for h in ["", "t=1", "v1=aa", "t=x,v1=aa", "t=1,v1=zz", "garbage"] {
            assert_eq!(
                verify(&[b"k"], h, b"", 1, 300),
                Err(SignatureError::Malformed)
            );
        }
    }

    proptest! {
        #[test]
        fn sign_then_verify(secret in proptest::collection::vec(any::<u8>(), 1..64),
                            body in proptest::collection::vec(any::<u8>(), 0..512),
                            ts in 0i64..4_000_000_000i64,
                            skew in -300i64..=300i64) {
            let h = header_value(&secret, ts, &body);
            prop_assert!(verify(&[&secret], &h, &body, ts + skew, 300).is_ok());
        }

        #[test]
        fn any_body_change_fails(secret in proptest::collection::vec(any::<u8>(), 1..32),
                                 body in proptest::collection::vec(any::<u8>(), 1..256),
                                 idx in any::<usize>(), flip in 1u8..=255u8) {
            let h = header_value(&secret, 100, &body);
            let mut bad = body.clone();
            let i = idx % bad.len();
            bad[i] ^= flip;
            prop_assert_eq!(verify(&[&secret], &h, &bad, 100, 300), Err(SignatureError::Mismatch));
        }

        #[test]
        fn wrong_secret_fails(a in proptest::collection::vec(any::<u8>(), 1..32),
                              b in proptest::collection::vec(any::<u8>(), 1..32),
                              body in proptest::collection::vec(any::<u8>(), 0..64)) {
            prop_assume!(a != b);
            let h = header_value(&a, 100, &body);
            prop_assert!(verify(&[&b], &h, &body, 100, 300).is_err());
        }

        #[test]
        fn outside_window_fails(ts in 0i64..4_000_000_000i64, extra in 1i64..100_000i64) {
            let h = header_value(b"k", ts, b"x");
            prop_assert_eq!(verify(&[b"k"], &h, b"x", ts + 300 + extra, 300),
                Err(SignatureError::TimestampOutOfWindow));
        }

        #[test]
        fn parser_never_panics(s in ".{0,200}") {
            let _ = parse_header(&s);
        }
    }
}
