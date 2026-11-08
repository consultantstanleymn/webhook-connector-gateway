//! W3C `traceparent` handling, so one trace id follows an event from the
//! inbound request to the downstream call.

use rand::RngExt;

/// A trace id is 32 lowercase hex characters and not all zero.
fn valid_trace_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && s.bytes().any(|b| b != b'0')
}

/// Read the trace id from a `traceparent` header, if valid.
pub fn parse_traceparent(header: &str) -> Option<String> {
    let parts: Vec<&str> = header.trim().split('-').collect();
    if parts.len() == 4 && parts[0].len() == 2 && valid_trace_id(parts[1]) {
        Some(parts[1].to_string())
    } else {
        None
    }
}

pub fn new_trace_id() -> String {
    loop {
        let id = format!("{:032x}", rand::rng().random::<u128>());
        if valid_trace_id(&id) {
            return id;
        }
    }
}

pub fn new_span_id() -> String {
    format!("{:016x}", rand::rng().random::<u64>() | 1)
}

/// Header value for the downstream call: same trace id, new span id.
pub fn traceparent(trace_id: &str) -> String {
    format!("00-{}-{}-01", trace_id, new_span_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_and_invalid() {
        let tid = "4bf92f3577b34da6a3ce929d0e0e4736";
        assert_eq!(
            parse_traceparent(&format!("00-{tid}-00f067aa0ba902b7-01")).as_deref(),
            Some(tid)
        );
        assert!(
            parse_traceparent("00-0000000000000000000000000000000-00f067aa0ba902b7-01").is_none()
        );
        assert!(parse_traceparent("00-ZZZ-00f067aa0ba902b7-01").is_none());
        assert!(parse_traceparent("nonsense").is_none());
    }

    #[test]
    fn generated_ids_roundtrip() {
        let t = new_trace_id();
        assert_eq!(
            parse_traceparent(&traceparent(&t)).as_deref(),
            Some(t.as_str())
        );
    }
}
