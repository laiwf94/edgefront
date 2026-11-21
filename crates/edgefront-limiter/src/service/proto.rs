//! The messages of Envoy's rate limit service API, `envoy.service.ratelimit.v3`,
//! and the types they use, as Rust types with protobuf encoding.
//!
//! The code is generated from the files vendored under `proto/` at the
//! crate's root, from Envoy v1.40.2 (`proto/README.md` says what was
//! trimmed), and is checked in under `src/service/proto/`, so building the
//! limiter needs neither protoc nor a build script. `tests/proto_codegen.rs`
//! regenerates it with the pinned protox and prost-build and fails if
//! anything differs; `make proto` writes it after a change. The modules
//! follow the protobuf packages, which the generated code's `super::` paths
//! rely on.
//!
//! The well-known types (`google.protobuf.Duration`, `Struct`, `Timestamp`
//! and the wrappers) are generated here too, so the limiter needs no crate
//! beyond prost for them.

// Generated code: its documentation is Envoy's, its shapes are prost's.
#![allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    rustdoc::bare_urls,
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_html_tags
)]

/// `google.protobuf`: the well-known types Envoy's messages use.
pub mod google {
    pub mod protobuf {
        include!("proto/google.protobuf.rs");
    }
}

/// `envoy.*`: the rate limit service and the messages it is made of.
pub mod envoy {
    pub mod config {
        pub mod core {
            pub mod v3 {
                include!("proto/envoy.config.core.v3.rs");
            }
        }
    }
    pub mod extensions {
        pub mod common {
            pub mod ratelimit {
                pub mod v3 {
                    include!("proto/envoy.extensions.common.ratelimit.v3.rs");
                }
            }
        }
    }
    pub mod r#type {
        pub mod v3 {
            include!("proto/envoy.type.v3.rs");
        }
    }
    pub mod service {
        pub mod ratelimit {
            pub mod v3 {
                include!("proto/envoy.service.ratelimit.v3.rs");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::envoy::config::core::v3::HeaderValue;
    use super::envoy::extensions::common::ratelimit::v3::{
        LocalClusterRateLimit, LocalRateLimitDescriptor, RateLimitDescriptor, VhRateLimitsOptions,
        XRateLimitHeadersRfcVersion, rate_limit_descriptor,
    };
    use super::envoy::service::ratelimit::v3::{
        RateLimitRequest, RateLimitResponse, rate_limit_response,
    };
    use super::envoy::r#type::v3::{RateLimitUnit, TokenBucket};
    use super::google::protobuf::{
        BoolValue, BytesValue, DoubleValue, Duration, FloatValue, Int32Value, Int64Value,
        ListValue, NullValue, StringValue, Struct, Timestamp, UInt32Value, UInt64Value, Value,
        value,
    };
    use prost::Message;
    use std::collections::HashMap;
    use std::fmt::Debug;

    use rate_limit_response::rate_limit::Unit;
    use rate_limit_response::{Code, DescriptorStatus, Quota, RateLimit, quota};

    /// Encodes, decodes, compares, formats and clears one message: every
    /// method prost derives for it.
    fn round_trip<M: Message + Default + PartialEq + Debug>(message: M) {
        let bytes = message.encode_to_vec();
        assert_eq!(bytes.len(), message.encoded_len());
        let back = M::decode(bytes.as_slice()).expect("decodes what it encoded");
        assert_eq!(back, message);
        let mut delimited = Vec::new();
        message.encode_length_delimited(&mut delimited).unwrap();
        assert_eq!(
            M::decode_length_delimited(delimited.as_slice()).unwrap(),
            message
        );
        assert!(!format!("{message:?}").is_empty());
        let mut cleared = back;
        cleared.clear();
        assert_eq!(cleared, M::default());
    }

    fn entry(key: &str, value: &str) -> rate_limit_descriptor::Entry {
        rate_limit_descriptor::Entry {
            key: key.into(),
            value: value.into(),
        }
    }

    // A request as Envoy encodes it, fields in tag order, written out byte
    // by byte from the protobuf encoding rules: the field numbers and types
    // in the vendored files are the ones Envoy sends.
    const REQUEST: &[u8] = &[
        0x0a, 0x04, b'e', b'd', b'g', b'e', // 1 domain: "edge"
        0x12, 0x18, // 2 descriptors[0]: 24 bytes
        0x0a, 0x06, 0x0a, 0x01, b'k', 0x12, 0x01, b'v', //   1 entries[0]: k = v
        0x12, 0x08, 0x08, 0x0a, 0x10, 0x02, 0x1a, 0x02, 0x08,
        0x01, //   2 limit: 10 a MINUTE, ×1
        0x1a, 0x02, 0x08, 0x07, //   3 hits_addend: 7
        0x20, 0x01, //   4 is_negative_hits: true
        0x18, 0x03, // 3 hits_addend: 3
    ];

    fn request() -> RateLimitRequest {
        RateLimitRequest {
            domain: "edge".into(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![entry("k", "v")],
                limit: Some(rate_limit_descriptor::RateLimitOverride {
                    requests_per_unit: 10,
                    unit: RateLimitUnit::Minute as i32,
                    unit_multiplier: Some(UInt32Value { value: 1 }),
                }),
                hits_addend: Some(UInt64Value { value: 7 }),
                is_negative_hits: true,
            }],
            hits_addend: 3,
        }
    }

    // A response with a status over its limit, a header and a body, in the
    // same hand-written form.
    const RESPONSE: &[u8] = &[
        0x08, 0x02, // 1 overall_code: OVER_LIMIT
        0x12, 0x13, // 2 statuses[0]: 19 bytes
        0x08, 0x02, //   1 code: OVER_LIMIT
        0x12, 0x0b, //   2 current_limit: 11 bytes
        0x08, 0x0a, //     1 requests_per_unit: 10
        0x10, 0x02, //     2 unit: MINUTE
        0x1a, 0x01, b'n', //     3 name: "n"
        0x22, 0x02, 0x08, 0x05, //     4 unit_multiplier: 5
        0x22, 0x02, 0x08, 0x1e, //   4 duration_until_reset: 30 s
        0x1a, 0x06, 0x0a, 0x01, b'x', 0x12, 0x01, b'1', // 3 response_headers_to_add: x: 1
        0x2a, 0x02, b'n', b'o', // 5 raw_body: "no"
    ];

    fn response() -> RateLimitResponse {
        RateLimitResponse {
            overall_code: Code::OverLimit as i32,
            statuses: vec![DescriptorStatus {
                code: Code::OverLimit as i32,
                current_limit: Some(RateLimit {
                    name: "n".into(),
                    requests_per_unit: 10,
                    unit: Unit::Minute as i32,
                    unit_multiplier: Some(UInt32Value { value: 5 }),
                }),
                limit_remaining: 0,
                duration_until_reset: Some(Duration {
                    seconds: 30,
                    nanos: 0,
                }),
                quota: None,
            }],
            response_headers_to_add: vec![HeaderValue {
                key: "x".into(),
                value: "1".into(),
                raw_value: Vec::new(),
            }],
            raw_body: b"no".to_vec(),
            ..RateLimitResponse::default()
        }
    }

    #[test]
    fn decodes_a_request_as_envoy_encodes_it() {
        assert_eq!(RateLimitRequest::decode(REQUEST).unwrap(), request());
    }

    #[test]
    fn encodes_a_request_byte_for_byte() {
        // prost writes fields in declaration order; here that is tag order
        // at every level, so the bytes are exactly the hand-written ones.
        assert_eq!(request().encode_to_vec(), REQUEST);
    }

    #[test]
    fn decodes_a_response_as_written_and_encodes_one_envoy_reads_back() {
        assert_eq!(RateLimitResponse::decode(RESPONSE).unwrap(), response());
        // RateLimit declares name (3) before requests_per_unit (1), so the
        // encoding is in another field order; protobuf allows any order and
        // the bytes decode to the same message.
        let encoded = response().encode_to_vec();
        assert_eq!(encoded.len(), RESPONSE.len());
        assert_eq!(
            RateLimitResponse::decode(encoded.as_slice()).unwrap(),
            response()
        );
    }

    #[test]
    fn skips_fields_from_newer_envoys() {
        // Field 15 (varint) and 16 (bytes) are not in v1.40.2's request.
        let mut bytes = REQUEST.to_vec();
        bytes.extend_from_slice(&[0x78, 0x2a, 0x82, 0x01, 0x02, b'h', b'i']);
        assert_eq!(
            RateLimitRequest::decode(bytes.as_slice()).unwrap(),
            request()
        );
    }

    #[test]
    fn refuses_truncated_and_mistyped_input() {
        for cut in 1..REQUEST.len() {
            let prefix = &REQUEST[..cut];
            // A cut that falls between top-level fields is a valid shorter
            // request; every other cut is an error, never a panic.
            if let Ok(decoded) = RateLimitRequest::decode(prefix) {
                assert_eq!(decoded.encode_to_vec(), prefix, "cut at {cut}");
            }
        }
        // The domain (1) sent as a varint instead of bytes.
        assert!(RateLimitRequest::decode(&[0x08, 0x01][..]).is_err());
        // A descriptor whose length runs past the end.
        assert!(RateLimitRequest::decode(&[0x12, 0x7f, 0x0a][..]).is_err());
    }

    #[test]
    fn every_message_round_trips_with_every_field_set() {
        let duration = Duration {
            seconds: 1,
            nanos: 500_000_000,
        };
        let timestamp = Timestamp {
            seconds: 1_800_000_000,
            nanos: 1,
        };
        let quota = Quota {
            requests: 100,
            id: "shared".into(),
            expiration_specifier: Some(quota::ExpirationSpecifier::ValidUntil(timestamp)),
        };
        let mut fields = HashMap::new();
        let kinds = [
            value::Kind::NullValue(NullValue::NullValue as i32),
            value::Kind::NumberValue(1.5),
            value::Kind::StringValue("s".into()),
            value::Kind::BoolValue(true),
            value::Kind::StructValue(Struct::default()),
            value::Kind::ListValue(ListValue {
                values: vec![Value {
                    kind: Some(value::Kind::NumberValue(2.0)),
                }],
            }),
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            fields.insert(format!("f{i}"), Value { kind: Some(kind) });
        }
        let metadata = Struct { fields };
        round_trip(request());
        round_trip(RateLimitResponse {
            statuses: vec![DescriptorStatus {
                quota: Some(quota.clone()),
                limit_remaining: 4,
                ..response().statuses[0].clone()
            }],
            request_headers_to_add: vec![HeaderValue {
                key: "raw".into(),
                value: String::new(),
                raw_value: vec![0xff, 0x00],
            }],
            dynamic_metadata: Some(metadata.clone()),
            quota: Some(quota.clone()),
            ..response()
        });
        round_trip(quota);
        round_trip(metadata);
        round_trip(duration);
        round_trip(timestamp);
        round_trip(LocalRateLimitDescriptor {
            entries: vec![entry("a", "")],
            token_bucket: Some(TokenBucket {
                max_tokens: 10,
                tokens_per_fill: Some(UInt32Value { value: 2 }),
                fill_interval: Some(duration),
            }),
            shadow_mode: true,
        });
        round_trip(LocalClusterRateLimit {});
        round_trip(DoubleValue { value: 0.25 });
        round_trip(FloatValue { value: 0.5 });
        round_trip(Int64Value { value: -2 });
        round_trip(UInt64Value { value: 2 });
        round_trip(Int32Value { value: -3 });
        round_trip(UInt32Value { value: 3 });
        round_trip(BoolValue { value: true });
        round_trip(StringValue { value: "w".into() });
        round_trip(BytesValue { value: vec![1] });
    }

    /// Every variant's protobuf name maps back to the variant, the number
    /// is the one in the .proto file, and a name not in it maps to nothing.
    macro_rules! names_round_trip {
        ($enum:ty: $($variant:ident = $number:literal $name:literal),+ $(,)?) => {{
            $(
                let variant = <$enum>::$variant;
                assert_eq!(variant as i32, $number);
                assert_eq!(variant.as_str_name(), $name);
                assert_eq!(<$enum>::from_str_name($name), Some(variant));
                assert_eq!(<$enum>::try_from($number), Ok(variant));
            )+
            assert_eq!(<$enum>::from_str_name("NOT_A_NAME"), None);
            assert!(<$enum>::try_from(99).is_err());
        }};
    }

    #[test]
    fn enum_names_and_numbers_match_the_protos() {
        names_round_trip!(Code: Unknown = 0 "UNKNOWN", Ok = 1 "OK", OverLimit = 2 "OVER_LIMIT");
        names_round_trip!(Unit:
            Unknown = 0 "UNKNOWN", Second = 1 "SECOND", Minute = 2 "MINUTE", Hour = 3 "HOUR",
            Day = 4 "DAY", Week = 7 "WEEK", Month = 5 "MONTH", Year = 6 "YEAR");
        names_round_trip!(RateLimitUnit:
            Unknown = 0 "UNKNOWN", Second = 1 "SECOND", Minute = 2 "MINUTE", Hour = 3 "HOUR",
            Day = 4 "DAY", Month = 5 "MONTH", Year = 6 "YEAR");
        names_round_trip!(XRateLimitHeadersRfcVersion:
            Off = 0 "OFF", DraftVersion03 = 1 "DRAFT_VERSION_03");
        names_round_trip!(VhRateLimitsOptions:
            Override = 0 "OVERRIDE", Include = 1 "INCLUDE", Ignore = 2 "IGNORE");
        names_round_trip!(NullValue: NullValue = 0 "NULL_VALUE");
    }
}
