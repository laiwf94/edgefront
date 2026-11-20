//! Peer fetch: a node that does not own a key fetches it from the owner
//! instead of the origin, so the cluster sends one origin request per object
//! however many nodes are asked for it.
//!
//! A request between nodes carries the hop header, `edgefront-tier`, naming
//! the node that sent it and the tier it was sent to, signed with HMAC-SHA256
//! over those and the request's method and target with the cluster's key. A
//! node that receives a validly signed hop never sends the request to another
//! peer, so a request makes at most one peer hop (and one more to a shield):
//! membership views that disagree for a moment cannot make it loop. A hop
//! header without a valid signature (from a client, say) is removed and the
//! request treated as any other.

use hmac::{Hmac, KeyInit, Mac};
use http::header::{HeaderName, HeaderValue};
use sha2::Sha256;

/// The hop header between nodes.
pub const HOP: HeaderName = HeaderName::from_static("edgefront-tier");

/// Which tier a hop was sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The key's owner among the edge nodes.
    Owner,
    /// A shield node above the edge.
    Shield,
    /// One of a hot key's replicas, told by its owner to fetch it from the
    /// owner and keep it.
    Replica,
}

impl Tier {
    fn as_str(self) -> &'static str {
        match self {
            Tier::Owner => "owner",
            Tier::Shield => "shield",
            Tier::Replica => "replica",
        }
    }
}

/// A verified hop: who sent the request and to which tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub from: String,
    pub tier: Tier,
}

fn mac(key: &[u8], from: &str, tier: Tier, method: &str, target: &str) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    for part in [from, tier.as_str(), method, target] {
        m.update(&(part.len() as u32).to_be_bytes());
        m.update(part.as_bytes());
    }
    m
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The hop header for a request `from` this node to `tier`, for `method`
/// and `target` (the path and query).
pub fn sign(key: &[u8], from: &str, tier: Tier, method: &str, target: &str) -> HeaderValue {
    let tag = mac(key, from, tier, method, target).finalize().into_bytes();
    HeaderValue::from_str(&format!("{from} {} {}", tier.as_str(), hex(&tag)))
        .expect("node ids are tokens")
}

/// The hop a request carries, if its header is present and validly signed
/// for this method and target.
pub fn verify(key: &[u8], value: &HeaderValue, method: &str, target: &str) -> Option<Hop> {
    let text = value.to_str().ok()?;
    let mut parts = text.split(' ');
    let (from, tier, tag) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || from.is_empty() {
        return None;
    }
    let tier = match tier {
        "owner" => Tier::Owner,
        "shield" => Tier::Shield,
        "replica" => Tier::Replica,
        _ => return None,
    };
    let tag = unhex(tag)?;
    mac(key, from, tier, method, target)
        .verify_slice(&tag)
        .ok()?;
    Some(Hop {
        from: from.to_owned(),
        tier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"the cluster key, 32 bytes or more";

    #[test]
    fn a_signed_hop_verifies_only_for_its_request() {
        let v = sign(KEY, "edge-3", Tier::Owner, "GET", "/a?b=1");
        assert_eq!(
            verify(KEY, &v, "GET", "/a?b=1"),
            Some(Hop {
                from: "edge-3".into(),
                tier: Tier::Owner
            })
        );
        let s = sign(KEY, "edge-3", Tier::Shield, "GET", "/a");
        assert_eq!(verify(KEY, &s, "GET", "/a").unwrap().tier, Tier::Shield);
        let r = sign(KEY, "edge-3", Tier::Replica, "GET", "/a");
        assert_eq!(verify(KEY, &r, "GET", "/a").unwrap().tier, Tier::Replica);
        // Another target, method, key or tier: refused.
        assert_eq!(verify(KEY, &v, "GET", "/a?b=2"), None);
        assert_eq!(verify(KEY, &v, "HEAD", "/a?b=1"), None);
        assert_eq!(
            verify(b"another key entirely, 32 bytes...", &v, "GET", "/a?b=1"),
            None
        );
        let text = v.to_str().unwrap().replace(" owner ", " shield ");
        assert_eq!(
            verify(KEY, &HeaderValue::from_str(&text).unwrap(), "GET", "/a?b=1"),
            None
        );
        // Malformed headers.
        for bad in [
            "",
            "edge-3",
            "edge-3 owner",
            "edge-3 owner zz",
            "edge-3 owner abc",
            "edge-3 peer 00",
            " owner 00",
            "edge-3 owner 00 extra",
        ] {
            assert_eq!(
                verify(KEY, &HeaderValue::from_static(bad), "GET", "/"),
                None,
                "{bad:?}"
            );
        }
        assert_eq!(
            verify(
                KEY,
                &HeaderValue::from_bytes(b"\xff owner 00").unwrap(),
                "GET",
                "/"
            ),
            None
        );
        // The sender's id, its tier and the tag are all covered: moving a
        // byte between the id and the target changes the tag.
        let a = sign(KEY, "ab", Tier::Owner, "GET", "/c");
        let b = sign(KEY, "a", Tier::Owner, "GET", "b/c");
        assert_ne!(
            a.to_str().unwrap().split(' ').nth(2),
            b.to_str().unwrap().split(' ').nth(2)
        );
    }
}
