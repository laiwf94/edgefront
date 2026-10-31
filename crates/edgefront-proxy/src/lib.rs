//! The edgefront proxy: routing, and later balancing, health and the rest of
//! the request path that steer used to provide.
//!
//! This crate is being ported from `legacy/steer` one package at a time.
//! Each port keeps steer's behaviour exactly, so that `edgefront-difftest`
//! can diff every decision against the Go original before steer is deleted.

pub mod route;
