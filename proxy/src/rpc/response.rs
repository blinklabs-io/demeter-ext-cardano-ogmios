//! Cheap scanning of one Ogmios JSON-RPC response, without parsing it.
//!
//! Ogmios writes every response as `jsonrpc`, `method` (absent from successes
//! only under `--strict-rpc`), then `result` or `error {code, message, data}`,
//! then `id`, always last. So the method and the error code sit in the first
//! bytes of the frame and the id in its last ones, and a scan reads at most
//! [`HEAD_BYTES`] from the front and a few hundred from the back, whatever the
//! frame's length. `result` payloads run to many megabytes with
//! `--include-cbor`; they are never looked at.

use super::classify::{scan_method, skip_whitespace, RequestId};
use super::vocabulary::Method;

/// How far from the start of a frame the method and the error code are read.
pub const HEAD_BYTES: usize = 512;

/// How far back from the final `}` the last `"id":` key is looked for.
pub const TAIL_BYTES: usize = 128;

/// How much trailing whitespace is skipped before the final `}`.
const TRAILING_WHITESPACE_BYTES: usize = 128;

const METHOD_KEY: &[u8] = b"\"method\"";
const RESULT_KEY: &[u8] = b"\"result\"";
const ERROR_KEY: &[u8] = b"\"error\"";
const CODE_KEY: &[u8] = b"\"code\"";
const ID_KEY: &[u8] = b"\"id\"";

/// The JSON-RPC error code of an error response; `None` when it could not be
/// read as an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode(pub Option<i64>);

impl ErrorCode {
    /// The `code` label: the integer when it is a JSON-RPC reserved code
    /// (-32768 to -32000) or an Ogmios one (1000 to 9999), `other` otherwise.
    pub fn label(self) -> String {
        match self.0 {
            Some(code @ (-32768..=-32000 | 1000..=9999)) => code.to_string(),
            _ => "other".to_string(),
        }
    }
}

/// What the first [`HEAD_BYTES`] of a response say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Head {
    /// The echoed method, when present before `result` or `error`.
    pub method: Option<Method>,
    /// Set when `error` comes before `result`.
    pub error: Option<ErrorCode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response {
    pub method: Option<Method>,
    pub error: Option<ErrorCode>,
    pub id: RequestId,
}

pub fn scan(frame: &[u8]) -> Response {
    let Head { method, error } = scan_head(frame);
    Response {
        method,
        error,
        id: scan_id(frame),
    }
}

pub fn scan_head(frame: &[u8]) -> Head {
    let head = &frame[..frame.len().min(HEAD_BYTES)];

    let result_at = find_key(head, RESULT_KEY).map(|(key, _)| key);
    let error = find_key(head, ERROR_KEY).filter(|(key, _)| precedes(*key, result_at));
    let body_at = match (result_at, error) {
        (Some(at), _) => Some(at),
        (None, Some((at, _))) => Some(at),
        (None, None) => None,
    };

    // A `"method"` key inside `result` or `error.data` is not the echo.
    let method = find_key(head, METHOD_KEY)
        .filter(|(key, _)| precedes(*key, body_at))
        .and_then(|(key, _)| scan_method(&head[key + METHOD_KEY.len()..]))
        .map(Method::from_name);

    let error = error.map(|(_, value)| {
        let error = &head[value..];
        ErrorCode(find_key(error, CODE_KEY).and_then(|(_, code)| parse_int(&error[code..])))
    });

    Head { method, error }
}

/// The top-level `id`, read back from the final `}`. Anything that does not
/// read as one JSON value there is `Other`.
pub fn scan_id(frame: &[u8]) -> RequestId {
    let tail = &frame[frame
        .len()
        .saturating_sub(TAIL_BYTES + TRAILING_WHITESPACE_BYTES)..];
    let end = tail
        .iter()
        .rposition(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .filter(|end| tail.len() - end - 1 <= TRAILING_WHITESPACE_BYTES);
    let Some(end) = end.filter(|end| tail[*end] == b'}') else {
        return RequestId::Other;
    };

    let window = &tail[end.saturating_sub(TAIL_BYTES)..end];
    let Some(value) = memchr::memmem::rfind_iter(window, ID_KEY)
        .find_map(|key| value_start(window, key + ID_KEY.len()))
    else {
        return RequestId::Other;
    };

    // The token is short by construction, so parsing it costs a constant. The
    // same deserializer as the request side keeps string hashes comparable.
    serde_json::from_slice(&window[value..]).unwrap_or(RequestId::Other)
}

/// Whether a key at `key` comes before `at`, or `at` is absent.
fn precedes(key: usize, at: Option<usize>) -> bool {
    match at {
        Some(at) => key < at,
        None => true,
    }
}

/// Finds the first `key` that is followed by `:`, and returns where the key
/// starts and where its value starts.
fn find_key(haystack: &[u8], key: &[u8]) -> Option<(usize, usize)> {
    memchr::memmem::find_iter(haystack, key)
        .find_map(|at| value_start(haystack, at + key.len()).map(|value| (at, value)))
}

/// The index after `<ws>:<ws>` at `after_key`, if that is what is there.
fn value_start(haystack: &[u8], after_key: usize) -> Option<usize> {
    let rest = skip_whitespace(&haystack[after_key..]);
    let rest = skip_whitespace(rest.strip_prefix(b":")?);
    Some(haystack.len() - rest.len())
}

/// An integer at the start of `bytes`; `None` for a float, an overflow, or
/// anything else.
fn parse_int(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes.strip_prefix(b"-") {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    let len = digits.iter().take_while(|b| b.is_ascii_digit()).count();
    if len == 0 || matches!(digits.get(len), Some(b'.' | b'e' | b'E')) {
        return None;
    }
    let mut value: i64 = 0;
    for digit in &digits[..len] {
        let digit = i64::from(digit - b'0');
        value = value.checked_mul(10)?;
        value = if negative {
            value.checked_sub(digit)?
        } else {
            value.checked_add(digit)?
        };
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::classify;

    fn id_of_request(id: &str) -> RequestId {
        classify(format!(r#"{{"method":"nextBlock","id":{id}}}"#).as_bytes()).id
    }

    #[test]
    fn a_success_with_its_method() {
        let response = scan(br#"{"jsonrpc":"2.0","method":"queryNetwork/tip","result":{"slot":1,"id":"ab"},"id":7}"#);
        assert_eq!(
            response,
            Response {
                method: Some(Method::QueryNetworkTip),
                error: None,
                id: RequestId::Int(7),
            }
        );
    }

    #[test]
    fn a_success_without_its_method() {
        let response = scan(br#"{"jsonrpc":"2.0","result":{"method":"nextBlock"},"id":7}"#);
        assert_eq!(response.method, None);
        assert_eq!(response.error, None);
        assert_eq!(response.id, RequestId::Int(7));
    }

    #[test]
    fn an_error_with_its_method_and_code() {
        let response = scan(
            br#"{"jsonrpc":"2.0","method":"submitTransaction","error":{"code":3005,"message":"x","data":{"result":1}},"id":"a"}"#,
        );
        assert_eq!(response.method, Some(Method::SubmitTransaction));
        assert_eq!(response.error, Some(ErrorCode(Some(3005))));
        assert_eq!(response.id, id_of_request(r#""a""#));
    }

    #[test]
    fn an_error_without_a_method() {
        let response = scan(
            br#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Invalid JSON","data":{"method":"nextBlock"}},"id":null}"#,
        );
        assert_eq!(response.method, None);
        assert_eq!(response.error, Some(ErrorCode(Some(-32700))));
        assert_eq!(response.id, RequestId::Null);
    }

    #[test]
    fn an_unreadable_error_code_is_still_an_error() {
        for code in ["1.5", "\"x\"", "99999999999999999999", "-"] {
            let frame = format!(r#"{{"error":{{"code":{code},"message":"m"}},"id":1}}"#);
            assert_eq!(
                scan(frame.as_bytes()).error,
                Some(ErrorCode(None)),
                "{code}"
            );
        }
    }

    #[test]
    fn error_code_labels() {
        let cases = [
            (Some(-32768), "-32768"),
            (Some(-32000), "-32000"),
            (Some(-31999), "other"),
            (Some(-32769), "other"),
            (Some(1000), "1000"),
            (Some(9999), "9999"),
            (Some(999), "other"),
            (Some(10000), "other"),
            (None, "other"),
        ];
        for (code, label) in cases {
            assert_eq!(ErrorCode(code).label(), label);
        }
    }

    #[test]
    fn an_unlisted_method_is_unknown() {
        let response =
            scan(br#"{"jsonrpc":"2.0","method":"frobnicate","error":{"code":-32601},"id":1}"#);
        assert_eq!(response.method, Some(Method::Unknown));
    }

    #[test]
    fn ids_of_every_kind() {
        let cases = [
            ("42", RequestId::Int(42)),
            ("-3", RequestId::Int(-3)),
            ("null", RequestId::Null),
            (r#""plain""#, id_of_request(r#""plain""#)),
            (r#""esc\"aped\\é""#, id_of_request(r#""esc\"aped\\é""#)),
            (r#"{"a":1}"#, RequestId::Other),
            (r#"{"id":5}"#, RequestId::Other),
            ("[1,2]", RequestId::Other),
            ("1.5", RequestId::Other),
            ("true", RequestId::Other),
        ];
        for (id, expected) in cases {
            let frame =
                format!(r#"{{"jsonrpc":"2.0","method":"nextBlock","result":{{}},"id":{id}}}"#);
            assert_eq!(scan_id(frame.as_bytes()), expected, "{id}");
        }
        assert!(matches!(
            id_of_request(r#""esc\"aped\\é""#),
            RequestId::Str(_)
        ));
        assert_ne!(
            id_of_request(r#""plain""#),
            id_of_request(r#""esc\"aped\\é""#)
        );
    }

    #[test]
    fn whitespace_variants() {
        let frames = [
            "{ \"jsonrpc\" : \"2.0\" , \"method\" : \"nextBlock\" , \"result\" : {} , \"id\" : 9 }",
            "{\"jsonrpc\":\"2.0\",\n  \"method\":\t\"nextBlock\",\n  \"result\": {},\n  \"id\":\n9\n}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"nextBlock\",\"result\":{},\"id\":9}\r\n  ",
        ];
        for frame in frames {
            let response = scan(frame.as_bytes());
            assert_eq!(response.method, Some(Method::NextBlock), "{frame:?}");
            assert_eq!(response.id, RequestId::Int(9), "{frame:?}");
        }

        let error = "{\"error\" :\n {\"code\" : -32602 }, \"id\" : 1}";
        assert_eq!(scan(error.as_bytes()).error, Some(ErrorCode(Some(-32602))));
    }

    #[test]
    fn nested_ids_under_a_null_id_do_not_match() {
        let block = format!(
            r#"{{"jsonrpc":"2.0","method":"nextBlock","result":{{"direction":"forward","block":{{"id":"b1","transactions":[{}]}},"tip":{{"slot":1,"id":"t1"}}}},"id":null}}"#,
            vec![r#"{"id":"tx","inputs":[{"transaction":{"id":"in"},"index":0}]}"#; 50].join(",")
        );
        assert_eq!(scan_id(block.as_bytes()), RequestId::Null);
        assert_eq!(scan(block.as_bytes()).method, Some(Method::NextBlock));
    }

    #[test]
    fn a_frame_that_is_not_an_object_is_other() {
        for frame in [
            "",
            "   ",
            "[1]",
            "{\"id\":1",
            "{\"id\":1}x",
            "{\"identity\":1}",
        ] {
            assert_eq!(scan_id(frame.as_bytes()), RequestId::Other, "{frame:?}");
        }
    }

    #[test]
    fn an_id_outside_the_tail_window_is_other() {
        let long = format!(r#"{{"result":{{}},"id":"{}"}}"#, "x".repeat(TAIL_BYTES));
        assert_eq!(scan_id(long.as_bytes()), RequestId::Other);

        let padded = format!(
            "{{\"result\":{{}},\"id\":1}}{}",
            " ".repeat(TRAILING_WHITESPACE_BYTES + 1)
        );
        assert_eq!(scan_id(padded.as_bytes()), RequestId::Other);
    }

    #[test]
    fn a_method_past_the_head_is_not_read() {
        let frame = format!(
            r#"{{"jsonrpc":"2.0","padding":"{}","method":"nextBlock","result":{{}},"id":1}}"#,
            "x".repeat(HEAD_BYTES)
        );
        assert_eq!(scan_head(frame.as_bytes()), Head::default());
    }
}
