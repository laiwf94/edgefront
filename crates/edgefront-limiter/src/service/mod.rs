//! The decision service: edgefront's limiter answering other proxies over
//! the network. Its first front end is Envoy's rate limit service API,
//! `envoy.service.ratelimit.v3`, whose messages are in [`proto`] and whose
//! gRPC service is [`rls`].

pub mod proto;
pub mod rls;
