//! Cheap classification of one client JSON-RPC message.
//!
//! The order is fixed by the contract:
//!
//! 1. A message without the bytes `"method"` is `invalid`.
//! 2. A message over 64 KiB is never JSON-parsed: the method comes from the
//!    first `"method"` key followed by an escape-free string.
//! 3. Anything else is parsed once into a borrowed envelope. `params` is parsed
//!    again, from its raw slice, only for the methods with a shape rule, and
//!    arrays are counted without allocating per element.
//! 4. A JSON array or any other non-object payload is `invalid`.
//!
//! When `params` is present but is not a JSON object, the shape is `params`
//! (`bytes/…` for submit and evaluate): the request is malformed, and only
//! Ogmios can say how.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::vocabulary::{CountBucket, Method, Shape, SizeBucket};

/// Messages larger than this are classified without parsing them.
pub const OVERSIZE_BYTES: usize = 64 * 1024;

const METHOD_KEY: &[u8] = b"\"method\"";

/// The JSON-RPC `id` of a request, reduced to what correlation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RequestId {
    #[default]
    Absent,
    Null,
    Int(i64),
    /// A hash of the decoded string; the string itself is never kept.
    Str(u64),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub method: Method,
    pub shape: Shape,
    pub id: RequestId,
    pub bytes: usize,
}

impl Classification {
    pub fn synthetic(method: Method, bytes: usize) -> Self {
        Self {
            method,
            shape: Shape::None,
            id: RequestId::Absent,
            bytes,
        }
    }
}

/// Classes whose cost to the instance is out of proportion to their count.
/// This is the contract's initial set; it changes only by amending it.
pub fn is_heavy(classification: &Classification) -> bool {
    matches!(
        (classification.method, classification.shape),
        (Method::QueryUtxo, Shape::Whole | Shape::Addresses(_))
            | (
                Method::QueryUtxo,
                Shape::OutputReferences(CountBucket::Over100)
            )
            | (Method::QueryStakePools, Shape::All)
            | (Method::QueryDelegateRepresentatives, Shape::All)
            | (
                Method::QueryLiveStakeDistribution
                    | Method::QueryRewardsProvenance
                    | Method::QueryDump
                    | Method::QueryStakePoolsPerformances,
                _,
            )
    )
}

pub fn classify(message: &[u8]) -> Classification {
    let bytes = message.len();

    let Some(method_at) = memchr::memmem::find(message, METHOD_KEY) else {
        return Classification::synthetic(Method::Invalid, bytes);
    };

    if !starts_with_object(message) {
        return Classification::synthetic(Method::Invalid, bytes);
    }

    if bytes > OVERSIZE_BYTES {
        return classify_oversize(&message[method_at + METHOD_KEY.len()..], bytes);
    }

    let Ok(envelope) = serde_json::from_slice::<Envelope>(message) else {
        return Classification::synthetic(Method::Invalid, bytes);
    };
    let Some(MethodName(method)) = envelope.method else {
        return Classification::synthetic(Method::Invalid, bytes);
    };

    Classification {
        method,
        shape: shape(method, envelope.params, bytes),
        id: envelope.id,
        bytes,
    }
}

fn classify_oversize(after_key: &[u8], bytes: usize) -> Classification {
    let method = scan_method(after_key)
        .map(Method::from_name)
        .unwrap_or(Method::Unknown);

    let shape = match method {
        Method::SubmitTransaction | Method::EvaluateTransaction => {
            Shape::Bytes(SizeBucket::of(bytes))
        }
        Method::Unknown | Method::Invalid | Method::Binary => Shape::None,
        _ => Shape::Oversize,
    };

    Classification {
        method,
        shape,
        id: RequestId::Other,
        bytes,
    }
}

/// Reads `<ws>:<ws>"<escape-free string>"` and returns the string.
pub(super) fn scan_method(after_key: &[u8]) -> Option<&str> {
    let rest = skip_whitespace(after_key);
    let rest = skip_whitespace(rest.strip_prefix(b":")?);
    let rest = rest.strip_prefix(b"\"")?;
    let end = memchr::memchr2(b'"', b'\\', rest)?;
    if rest[end] != b'"' {
        return None;
    }
    std::str::from_utf8(&rest[..end]).ok()
}

pub(super) fn skip_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

/// A struct deserializer also accepts a JSON array, so objects are checked
/// for up front.
fn starts_with_object(json: &[u8]) -> bool {
    skip_whitespace(json).first() == Some(&b'{')
}

fn is_empty_object(raw: &RawValue) -> bool {
    let json = raw.get().as_bytes();
    starts_with_object(json) && skip_whitespace(&skip_whitespace(json)[1..]) == b"}"
}

fn shape(method: Method, params: Option<&RawValue>, bytes: usize) -> Shape {
    match method {
        Method::QueryUtxo => match parse::<UtxoParams>(params) {
            Params::Absent | Params::Empty => Shape::Whole,
            Params::Object(UtxoParams {
                addresses: Some(Len(Some(count))),
                ..
            }) => Shape::Addresses(CountBucket::of(count)),
            Params::Object(UtxoParams {
                output_references: Some(Len(Some(count))),
                ..
            }) => Shape::OutputReferences(CountBucket::of(count)),
            Params::Object(_) | Params::Other => Shape::Params,
        },
        Method::FindIntersection => match parse::<IntersectionParams>(params) {
            Params::Absent
            | Params::Empty
            | Params::Object(IntersectionParams { points: None }) => Shape::None,
            Params::Object(IntersectionParams {
                points: Some(points),
            }) => {
                if points.len == 1 && points.first_is_origin {
                    Shape::Origin
                } else {
                    Shape::Points(CountBucket::of(points.len))
                }
            }
            Params::Other => Shape::Params,
        },
        Method::AcquireLedgerState => match parse::<PointParams>(params) {
            Params::Object(PointParams {
                point: Some(Token::Origin),
            }) => Shape::Origin,
            Params::Other => Shape::Params,
            _ => Shape::Point,
        },
        Method::NextTransaction => match parse::<FieldsParams>(params) {
            Params::Object(FieldsParams {
                fields: Some(Token::All),
            }) => Shape::Full,
            Params::Other => Shape::Params,
            _ => Shape::Ids,
        },
        Method::SubmitTransaction => Shape::Bytes(SizeBucket::of(bytes)),
        Method::EvaluateTransaction => match parse::<EvaluateParams>(params) {
            Params::Object(EvaluateParams {
                additional_utxo: Some(Len(Some(1..))),
            }) => Shape::BytesUtxo(SizeBucket::of(bytes)),
            _ => Shape::Bytes(SizeBucket::of(bytes)),
        },
        Method::QueryStakePools => match parse::<StakePoolsParams>(params) {
            Params::Object(StakePoolsParams {
                stake_pools: Some(Len(Some(1..))),
            }) => Shape::Filtered,
            Params::Other => Shape::Params,
            _ => Shape::All,
        },
        Method::QueryDelegateRepresentatives => match parse::<KeysParams>(params) {
            Params::Object(keys) if keys.count() > 0 => Shape::Filtered,
            Params::Other => Shape::Params,
            _ => Shape::All,
        },
        Method::QueryRewardAccountSummaries => match parse::<KeysParams>(params) {
            Params::Object(keys) => Shape::Keys(CountBucket::of(keys.count())),
            Params::Absent | Params::Empty => Shape::Keys(CountBucket::Zero),
            Params::Other => Shape::Params,
        },
        Method::Unknown | Method::Invalid | Method::Binary => Shape::None,
        _ => match params {
            None => Shape::None,
            Some(raw) if is_empty_object(raw) => Shape::None,
            Some(_) => Shape::Params,
        },
    }
}

enum Params<T> {
    Absent,
    /// `{}`, which every rule reads as absent.
    Empty,
    Object(T),
    /// Not a JSON object, or an object of the wrong shape.
    Other,
}

fn parse<'a, T: Deserialize<'a>>(params: Option<&'a RawValue>) -> Params<T> {
    let Some(raw) = params else {
        return Params::Absent;
    };
    if is_empty_object(raw) {
        return Params::Empty;
    }
    if !starts_with_object(raw.get().as_bytes()) {
        return Params::Other;
    }
    match serde_json::from_str(raw.get()) {
        Ok(parsed) => Params::Object(parsed),
        Err(_) => Params::Other,
    }
}

#[derive(Deserialize)]
struct Envelope<'a> {
    method: Option<MethodName>,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
    #[serde(default)]
    id: RequestId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UtxoParams {
    addresses: Option<Len>,
    output_references: Option<Len>,
}

#[derive(Deserialize)]
struct IntersectionParams {
    points: Option<Points>,
}

#[derive(Deserialize)]
struct PointParams {
    point: Option<Token>,
}

#[derive(Deserialize)]
struct FieldsParams {
    fields: Option<Token>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvaluateParams {
    additional_utxo: Option<Len>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StakePoolsParams {
    stake_pools: Option<Len>,
}

#[derive(Deserialize)]
struct KeysParams {
    keys: Option<Len>,
    scripts: Option<Len>,
}

impl KeysParams {
    fn count(&self) -> usize {
        let len = |field: &Option<Len>| field.as_ref().and_then(|len| len.0).unwrap_or(0);
        len(&self.keys) + len(&self.scripts)
    }
}

/// The method string, mapped straight to the vocabulary. Unlike a `Cow<str>`
/// it never allocates, even when the string is escaped.
struct MethodName(Method);

impl<'de> Deserialize<'de> for MethodName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MethodVisitor;
        impl Visitor<'_> for MethodVisitor {
            type Value = MethodName;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a method string")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<MethodName, E> {
                Ok(MethodName(Method::from_name(v)))
            }
        }
        deserializer.deserialize_str(MethodVisitor)
    }
}

/// Implements the non-string scalar `visit_*` methods of a `deserialize_any`
/// visitor as a constant, leaving strings and containers to the visitor.
macro_rules! scalars_are {
    ($value:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($value)
        }
    };
}

fn drain_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<usize, A::Error> {
    let mut count = 0;
    while seq.next_element::<IgnoredAny>()?.is_some() {
        count += 1;
    }
    Ok(count)
}

fn drain_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<usize, A::Error> {
    let mut count = 0;
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
        count += 1;
    }
    Ok(count)
}

/// The element count of an array, or the entry count of an object; `None`
/// for a scalar.
struct Len(Option<usize>);

impl<'de> Deserialize<'de> for Len {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LenVisitor;
        impl<'de> Visitor<'de> for LenVisitor {
            type Value = Len;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            scalars_are!(Len(None));
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Len, E> {
                Ok(Len(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Len, A::Error> {
                drain_seq(seq).map(|count| Len(Some(count)))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Len, A::Error> {
                drain_map(map).map(|count| Len(Some(count)))
            }
        }
        deserializer.deserialize_any(LenVisitor)
    }
}

/// The few string values a shape rule distinguishes.
#[derive(PartialEq)]
enum Token {
    Origin,
    All,
    Other,
}

impl<'de> Deserialize<'de> for Token {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TokenVisitor;
        impl<'de> Visitor<'de> for TokenVisitor {
            type Value = Token;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            scalars_are!(Token::Other);
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Token, E> {
                Ok(match v {
                    "origin" => Token::Origin,
                    "all" => Token::All,
                    _ => Token::Other,
                })
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Token, A::Error> {
                drain_seq(seq).map(|_| Token::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Token, A::Error> {
                drain_map(map).map(|_| Token::Other)
            }
        }
        deserializer.deserialize_any(TokenVisitor)
    }
}

/// `findIntersection` points: how many, and whether the first is `"origin"`.
struct Points {
    len: usize,
    first_is_origin: bool,
}

impl<'de> Deserialize<'de> for Points {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PointsVisitor;
        impl<'de> Visitor<'de> for PointsVisitor {
            type Value = Points;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array of points")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Points, A::Error> {
                let Some(first) = seq.next_element::<Token>()? else {
                    return Ok(Points {
                        len: 0,
                        first_is_origin: false,
                    });
                };
                Ok(Points {
                    len: 1 + drain_seq(seq)?,
                    first_is_origin: first == Token::Origin,
                })
            }
        }
        deserializer.deserialize_seq(PointsVisitor)
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;
        impl<'de> Visitor<'de> for IdVisitor {
            type Value = RequestId;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<RequestId, E> {
                Ok(RequestId::Null)
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<RequestId, E> {
                Ok(RequestId::Int(v))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<RequestId, E> {
                Ok(i64::try_from(v).map_or(RequestId::Other, RequestId::Int))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<RequestId, E> {
                let mut hasher = DefaultHasher::new();
                v.hash(&mut hasher);
                Ok(RequestId::Str(hasher.finish()))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<RequestId, E> {
                Ok(RequestId::Other)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<RequestId, E> {
                Ok(RequestId::Other)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<RequestId, A::Error> {
                drain_seq(seq).map(|_| RequestId::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<RequestId, A::Error> {
                drain_map(map).map(|_| RequestId::Other)
            }
        }
        deserializer.deserialize_any(IdVisitor)
    }
}
