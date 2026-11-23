//! `envoy.service.ratelimit.v3.RateLimitService`: Envoy's global rate
//! limit filter, and any other client of the API, asking edgefront for a
//! decision.
//!
//! [`RateLimitService::decide`] answers a [`RateLimitRequest`] from a
//! [`DescriptorLimiter`] the way envoyproxy/ratelimit answers it:
//!
//! - The request must name a domain and carry at least one descriptor;
//!   otherwise the call fails with `INVALID_ARGUMENT` and the service's
//!   own messages, `rate limit domain must not be empty` and `rate limit
//!   descriptor list must not be empty`.
//! - Each descriptor adds its own `hits_addend` when it sets one (0 checks
//!   without counting), else the request's, else 1. With
//!   `is_negative_hits` the hits are given back instead, never taking a
//!   counter below zero.
//! - A descriptor's `limit` override is used instead of the configured
//!   limit for its entries, with its unit and `unit_multiplier`. An
//!   override without a unit fails the call with `INVALID_ARGUMENT`, as
//!   the service cannot count it either.
//! - There is one status per descriptor, in order. A descriptor with no
//!   limit is `OK` with no `current_limit` and nothing remaining; an
//!   unlimited one is `OK` with `limit_remaining` at `u32::MAX`. A limited
//!   one carries its `current_limit` (name, requests per unit, unit and,
//!   when over 1, the multiplier), what remains of it and
//!   `duration_until_reset`, the time to the end of its window. It is
//!   `OVER_LIMIT` once its counter passes the limit, except in shadow
//!   mode, where it stays `OK`.
//! - `overall_code` is `OVER_LIMIT` when any status is, unless the service
//!   is in global shadow mode (the service's `SHADOW_MODE`), where it is
//!   `OK` while the statuses still say which descriptors are over.
//! - With [`ResponseHeaders`] set (the service's
//!   `LIMIT_RESPONSE_HEADERS_ENABLED`), `response_headers_to_add` carries
//!   `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset` for
//!   the descriptor closest to its limit: the last one over it, else the
//!   one with the least remaining. Envoy adds them to the response it
//!   sends downstream.
//!
//! [`RateLimitService::call`] serves the method over gRPC on tonic's
//! framing, with the messages encoded by prost through [`ProstCodec`];
//! any HTTP/2 server that hands it the request can serve it.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::server::{Grpc, UnaryService};
use tonic::{Request, Response, Status};

use super::proto::envoy::config::core::v3::HeaderValue;
use super::proto::envoy::service::ratelimit::v3::rate_limit_response::{
    Code, DescriptorStatus, RateLimit, rate_limit,
};
use super::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use super::proto::envoy::r#type::v3::RateLimitUnit;
use super::proto::google::protobuf;
use crate::descriptor::{self, DescriptorLimiter, LimitOverride, Query, Unit};

/// The service's full name.
pub const SERVICE_NAME: &str = "envoy.service.ratelimit.v3.RateLimitService";

/// The path of its one method, `ShouldRateLimit`.
pub const SHOULD_RATE_LIMIT: &str = "/envoy.service.ratelimit.v3.RateLimitService/ShouldRateLimit";

/// A tonic codec for prost messages: `E` is encoded, `D` decoded.
pub struct ProstCodec<E, D>(PhantomData<fn(E) -> D>);

impl<E, D> Default for ProstCodec<E, D> {
    fn default() -> Self {
        ProstCodec(PhantomData)
    }
}

/// [`ProstCodec`]'s encoder.
pub struct ProstEncoder<E>(PhantomData<fn(E)>);

/// [`ProstCodec`]'s decoder.
pub struct ProstDecoder<D>(PhantomData<fn() -> D>);

impl<E: Message + Send + 'static> Encoder for ProstEncoder<E> {
    type Item = E;
    type Error = Status;

    fn encode(&mut self, item: E, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        // The buffer grows as it is written, so there is always room.
        item.encode(dst)
            .map_err(|e| Status::internal(e.to_string()))
    }
}

impl<D: Message + Default + Send + 'static> Decoder for ProstDecoder<D> {
    type Item = D;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<D>, Status> {
        D::decode(src)
            .map(Some)
            .map_err(|e| Status::internal(e.to_string()))
    }
}

impl<E, D> Codec for ProstCodec<E, D>
where
    E: Message + Send + 'static,
    D: Message + Default + Send + 'static,
{
    type Encode = E;
    type Decode = D;
    type Encoder = ProstEncoder<E>;
    type Decoder = ProstDecoder<D>;

    fn encoder(&mut self) -> ProstEncoder<E> {
        ProstEncoder(PhantomData)
    }

    fn decoder(&mut self) -> ProstDecoder<D> {
        ProstDecoder(PhantomData)
    }
}

/// The names of the headers a response asks Envoy to add, as the
/// service's `LIMIT_LIMIT_HEADER`, `LIMIT_REMAINING_HEADER` and
/// `LIMIT_RESET_HEADER` set them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHeaders {
    pub limit: String,
    pub remaining: String,
    pub reset: String,
}

impl Default for ResponseHeaders {
    /// The service's defaults, from the IETF RateLimit header fields draft.
    fn default() -> Self {
        ResponseHeaders {
            limit: "RateLimit-Limit".to_owned(),
            remaining: "RateLimit-Remaining".to_owned(),
            reset: "RateLimit-Reset".to_owned(),
        }
    }
}

type Clock = dyn Fn() -> u64 + Send + Sync;

/// The rate limit service over a [`DescriptorLimiter`]. Cheap to clone:
/// clones share the limiter and its counters.
#[derive(Clone)]
pub struct RateLimitService {
    limiter: Arc<DescriptorLimiter>,
    headers: Option<Arc<ResponseHeaders>>,
    clock: Arc<Clock>,
    global_shadow: bool,
}

impl std::fmt::Debug for RateLimitService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimitService")
            .field("limiter", &self.limiter)
            .field("headers", &self.headers)
            .field("global_shadow", &self.global_shadow)
            .finish_non_exhaustive()
    }
}

/// Unix time in whole seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The response's unit for a limit's.
fn response_unit(u: Unit) -> rate_limit::Unit {
    match u {
        Unit::Second => rate_limit::Unit::Second,
        Unit::Minute => rate_limit::Unit::Minute,
        Unit::Hour => rate_limit::Unit::Hour,
        Unit::Day => rate_limit::Unit::Day,
        Unit::Week => rate_limit::Unit::Week,
        Unit::Month => rate_limit::Unit::Month,
        Unit::Year => rate_limit::Unit::Year,
    }
}

/// An override's unit, `None` for `UNKNOWN` or a number the API does not
/// define.
fn override_unit(n: i32) -> Option<Unit> {
    Some(match RateLimitUnit::try_from(n).ok()? {
        RateLimitUnit::Unknown => return None,
        RateLimitUnit::Second => Unit::Second,
        RateLimitUnit::Minute => Unit::Minute,
        RateLimitUnit::Hour => Unit::Hour,
        RateLimitUnit::Day => Unit::Day,
        RateLimitUnit::Month => Unit::Month,
        RateLimitUnit::Year => Unit::Year,
    })
}

fn clamp(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn header(key: &str, value: u64) -> HeaderValue {
    HeaderValue {
        key: key.to_owned(),
        value: value.to_string(),
        raw_value: Vec::new(),
    }
}

impl RateLimitService {
    /// The service answering from `limiter`, on the system clock, adding
    /// no response headers.
    pub fn new(limiter: Arc<DescriptorLimiter>) -> Self {
        RateLimitService {
            limiter,
            headers: None,
            clock: Arc::new(unix_now),
            global_shadow: false,
        }
    }

    /// Answers `OK` overall to every request, as the service's
    /// `SHADOW_MODE` does: descriptors are still counted and their
    /// statuses still say `OVER_LIMIT`, so the effect of the limits can be
    /// watched before they are enforced.
    #[must_use]
    pub fn with_global_shadow_mode(mut self, on: bool) -> Self {
        self.global_shadow = on;
        self
    }

    /// Asks Envoy to add the RateLimit headers, under `headers`' names.
    #[must_use]
    pub fn with_response_headers(mut self, headers: ResponseHeaders) -> Self {
        self.headers = Some(Arc::new(headers));
        self
    }

    /// Reads Unix seconds from `clock` instead of the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    pub fn limiter(&self) -> &Arc<DescriptorLimiter> {
        &self.limiter
    }

    /// `ShouldRateLimit` on one request, at the clock's time.
    pub fn decide(&self, req: &RateLimitRequest) -> Result<RateLimitResponse, Status> {
        self.decide_at(req, (self.clock)())
    }

    /// `ShouldRateLimit` on one request at `now_secs`, Unix time.
    pub fn decide_at(
        &self,
        req: &RateLimitRequest,
        now_secs: u64,
    ) -> Result<RateLimitResponse, Status> {
        if req.domain.is_empty() {
            return Err(Status::invalid_argument(
                "rate limit domain must not be empty",
            ));
        }
        if req.descriptors.is_empty() {
            return Err(Status::invalid_argument(
                "rate limit descriptor list must not be empty",
            ));
        }
        let request_hits = u64::from(req.hits_addend.max(1));
        let entries: Vec<Vec<(&str, &str)>> = req
            .descriptors
            .iter()
            .map(|d| {
                d.entries
                    .iter()
                    .map(|e| (e.key.as_str(), e.value.as_str()))
                    .collect()
            })
            .collect();
        let mut queries = Vec::with_capacity(entries.len());
        for (i, (d, entries)) in req.descriptors.iter().zip(&entries).enumerate() {
            let limit_override = match &d.limit {
                None => None,
                Some(o) => Some(LimitOverride {
                    requests_per_unit: u64::from(o.requests_per_unit),
                    unit: override_unit(o.unit).ok_or_else(|| {
                        Status::invalid_argument(format!(
                            "descriptor {i}: limit override has no valid unit ({})",
                            o.unit
                        ))
                    })?,
                    unit_multiplier: o.unit_multiplier.map_or(1, |m| m.value.max(1)),
                }),
            };
            queries.push(Query {
                entries,
                hits: d.hits_addend.map_or(request_hits, |h| h.value),
                negative: d.is_negative_hits,
                limit_override,
            });
        }
        let decision = self.limiter.decide(&req.domain, &queries, now_secs);
        Ok(self.response(&decision))
    }

    /// The v3 response for a decision.
    fn response(&self, decision: &descriptor::Response) -> RateLimitResponse {
        let statuses: Vec<DescriptorStatus> = decision
            .statuses
            .iter()
            .map(|s| {
                let code = if s.over_limit {
                    Code::OverLimit
                } else {
                    Code::Ok
                };
                match &s.limit {
                    // No limit, or unlimited: no current limit to report.
                    Some(l) if !l.unlimited => DescriptorStatus {
                        code: code.into(),
                        current_limit: Some(RateLimit {
                            name: l.name.clone().unwrap_or_default(),
                            requests_per_unit: clamp(l.requests_per_unit),
                            unit: l
                                .unit
                                .map_or(rate_limit::Unit::Unknown, response_unit)
                                .into(),
                            unit_multiplier: (l.unit_multiplier > 1).then_some(
                                protobuf::UInt32Value {
                                    value: l.unit_multiplier,
                                },
                            ),
                        }),
                        limit_remaining: clamp(s.limit_remaining),
                        duration_until_reset: Some(protobuf::Duration {
                            seconds: i64::try_from(s.reset_secs).unwrap_or(i64::MAX),
                            nanos: 0,
                        }),
                        quota: None,
                    },
                    Some(_) => DescriptorStatus {
                        code: code.into(),
                        limit_remaining: u32::MAX,
                        ..DescriptorStatus::default()
                    },
                    None => DescriptorStatus {
                        code: code.into(),
                        ..DescriptorStatus::default()
                    },
                }
            })
            .collect();
        let mut response_headers_to_add = Vec::new();
        if let Some(names) = &self.headers {
            // The descriptor closest to its limit: the last over it, else
            // the first with the least remaining.
            let mut closest: Option<(&DescriptorStatus, u64)> = None;
            for (st, s) in statuses.iter().zip(&decision.statuses) {
                if st.current_limit.is_none() {
                    continue;
                }
                let closer = closest.is_none_or(|(c, _)| {
                    s.over_limit
                        || (c.code != i32::from(Code::OverLimit)
                            && st.limit_remaining < c.limit_remaining)
                });
                if closer {
                    closest = Some((st, s.reset_secs));
                }
            }
            if let Some((st, reset)) = closest {
                let limit = st.current_limit.as_ref().map_or(0, |l| l.requests_per_unit);
                response_headers_to_add = vec![
                    header(&names.limit, u64::from(limit)),
                    header(&names.remaining, u64::from(st.limit_remaining)),
                    header(&names.reset, reset),
                ];
            }
        }
        RateLimitResponse {
            overall_code: if decision.over_limit && !self.global_shadow {
                Code::OverLimit
            } else {
                Code::Ok
            }
            .into(),
            statuses,
            response_headers_to_add,
            ..RateLimitResponse::default()
        }
    }

    /// Serves one gRPC request: `ShouldRateLimit` at [`SHOULD_RATE_LIMIT`],
    /// `UNIMPLEMENTED` for any other path. The caller wraps its request
    /// body with [`tonic::body::Body::new`].
    pub async fn call(
        &self,
        req: http::Request<tonic::body::Body>,
    ) -> http::Response<tonic::body::Body> {
        if req.uri().path() != SHOULD_RATE_LIMIT {
            return Status::unimplemented(format!(
                "{} is not a method of {SERVICE_NAME}",
                req.uri().path()
            ))
            .into_http();
        }
        let mut grpc = Grpc::new(ProstCodec::<RateLimitResponse, RateLimitRequest>::default());
        grpc.unary(ShouldRateLimit(self.clone()), req).await
    }
}

/// `ShouldRateLimit` as a tonic unary service.
struct ShouldRateLimit(RateLimitService);

impl UnaryService<RateLimitRequest> for ShouldRateLimit {
    type Response = RateLimitResponse;
    type Future = std::future::Ready<Result<Response<RateLimitResponse>, Status>>;

    fn call(&mut self, req: Request<RateLimitRequest>) -> Self::Future {
        std::future::ready(self.0.decide(req.get_ref()).map(Response::new))
    }
}

#[cfg(test)]
mod tests;
