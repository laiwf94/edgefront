//! The shared-cache rules of RFC 9111: what may be stored, how long it is
//! fresh, how old it is, and how a stored response may be used.

use std::time::{Duration, SystemTime};

use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};

use crate::cache_control::CacheControl;
use crate::headers::{EntityTag, age_value, date_header, field_list};

/// Why a response was not stored. Logged and counted, never sent to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotStorable {
    Method,
    RequestNoStore,
    Status,
    NoStore,
    Private,
    Authorization,
    VaryStar,
    SetCookie,
    NeverReusable,
}

impl NotStorable {
    pub fn as_str(self) -> &'static str {
        match self {
            NotStorable::Method => "method",
            NotStorable::RequestNoStore => "request-no-store",
            NotStorable::Status => "status",
            NotStorable::NoStore => "no-store",
            NotStorable::Private => "private",
            NotStorable::Authorization => "authorization",
            NotStorable::VaryStar => "vary-star",
            NotStorable::SetCookie => "set-cookie",
            NotStorable::NeverReusable => "never-reusable",
        }
    }
}

/// Status codes this cache understands well enough to store.
fn understood(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        200 | 203 | 204 | 300 | 301 | 302 | 303 | 307 | 308 | 404 | 405 | 410 | 414 | 501
    )
}

/// Heuristically cacheable status codes (RFC 9110 section 15.1), less 206,
/// which this cache never stores.
fn heuristically_cacheable(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        200 | 203 | 204 | 300 | 301 | 308 | 404 | 405 | 410 | 414 | 501
    )
}

/// One request and the response the origin gave to it.
#[derive(Debug, Clone, Copy)]
pub struct Exchange<'a> {
    pub method: &'a Method,
    pub req_headers: &'a HeaderMap,
    pub req_cc: &'a CacheControl,
    pub status: StatusCode,
    pub resp_headers: &'a HeaderMap,
    pub resp_cc: &'a CacheControl,
    pub response_time: SystemTime,
}

/// Whether a shared cache may store this response (RFC 9111 section 3).
///
/// Two choices go beyond the RFC: responses carrying Set-Cookie are not
/// stored, because sharing a cookie with every client is almost never what
/// the origin meant; and responses that could never be reused (no
/// freshness, no validator, no stale allowance) are not stored either.
pub fn storable(x: &Exchange<'_>, heuristic_max: Duration) -> Result<(), NotStorable> {
    let Exchange {
        method,
        req_headers,
        req_cc,
        status,
        resp_headers,
        resp_cc,
        response_time,
    } = *x;
    if method != Method::GET {
        return Err(NotStorable::Method);
    }
    if req_cc.no_store {
        return Err(NotStorable::RequestNoStore);
    }
    if !understood(status) {
        return Err(NotStorable::Status);
    }
    // must-understand overrides no-store for status codes the cache
    // understands (RFC 9111 section 5.2.2.3).
    if resp_cc.no_store && !resp_cc.must_understand {
        return Err(NotStorable::NoStore);
    }
    if resp_cc.private {
        return Err(NotStorable::Private);
    }
    if req_headers.contains_key(header::AUTHORIZATION)
        && !(resp_cc.public || resp_cc.s_maxage.is_some() || resp_cc.must_revalidate)
    {
        return Err(NotStorable::Authorization);
    }
    if field_list(resp_headers, &header::VARY)
        .iter()
        .any(|v| v == "*")
    {
        return Err(NotStorable::VaryStar);
    }
    if resp_headers.contains_key(header::SET_COOKIE) {
        return Err(NotStorable::SetCookie);
    }
    let explicit = resp_cc.s_maxage.is_some()
        || resp_cc.max_age.is_some()
        || resp_headers.contains_key(header::EXPIRES);
    if !explicit && !resp_cc.public && !heuristically_cacheable(status) {
        return Err(NotStorable::Status);
    }
    let has_validator =
        resp_headers.contains_key(header::ETAG) || resp_headers.contains_key(header::LAST_MODIFIED);
    let lifetime = freshness_lifetime(status, resp_headers, resp_cc, response_time, heuristic_max);
    let stale_ok = resp_cc.stale_while_revalidate.is_some() || resp_cc.stale_if_error.is_some();
    if lifetime.is_zero() && !has_validator && !stale_ok {
        return Err(NotStorable::NeverReusable);
    }
    Ok(())
}

/// Freshness lifetime (RFC 9111 section 4.2.1). `response_time` stands in
/// for a missing Date header.
pub fn freshness_lifetime(
    status: StatusCode,
    headers: &HeaderMap,
    cc: &CacheControl,
    response_time: SystemTime,
    heuristic_max: Duration,
) -> Duration {
    if let Some(s) = cc.s_maxage {
        return Duration::from_secs(s);
    }
    if let Some(s) = cc.max_age {
        return Duration::from_secs(s);
    }
    let date = date_header(headers, &header::DATE).unwrap_or(response_time);
    if headers.contains_key(header::EXPIRES) {
        // An invalid Expires (such as "0") means already expired.
        return date_header(headers, &header::EXPIRES)
            .and_then(|e| e.duration_since(date).ok())
            .unwrap_or(Duration::ZERO);
    }
    if !(heuristically_cacheable(status) || cc.public) {
        return Duration::ZERO;
    }
    // Ten percent of the time since last modification, as RFC 9111
    // section 4.2.2 suggests, capped by configuration.
    date_header(headers, &header::LAST_MODIFIED)
        .and_then(|lm| date.duration_since(lm).ok())
        .map_or(Duration::ZERO, |d| (d / 10).min(heuristic_max))
}

/// Freshness facts fixed when a response is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    pub lifetime: Duration,
    pub corrected_initial_age: Duration,
    pub response_time: SystemTime,
}

impl Freshness {
    /// Computes lifetime and the corrected initial age (RFC 9111 section 4.2.3).
    pub fn compute(
        status: StatusCode,
        headers: &HeaderMap,
        cc: &CacheControl,
        request_time: SystemTime,
        response_time: SystemTime,
        heuristic_max: Duration,
    ) -> Self {
        let lifetime = freshness_lifetime(status, headers, cc, response_time, heuristic_max);
        let date = date_header(headers, &header::DATE).unwrap_or(response_time);
        let apparent_age = response_time.duration_since(date).unwrap_or(Duration::ZERO);
        let response_delay = response_time
            .duration_since(request_time)
            .unwrap_or(Duration::ZERO);
        let corrected_age_value =
            Duration::from_secs(age_value(headers)).saturating_add(response_delay);
        Freshness {
            lifetime,
            corrected_initial_age: apparent_age.max(corrected_age_value),
            response_time,
        }
    }

    pub fn current_age(&self, now: SystemTime) -> Duration {
        let resident = now
            .duration_since(self.response_time)
            .unwrap_or(Duration::ZERO);
        self.corrected_initial_age.saturating_add(resident)
    }

    /// How long the response has been stale; zero while fresh.
    pub fn staleness(&self, now: SystemTime) -> Duration {
        self.current_age(now).saturating_sub(self.lifetime)
    }

    pub fn is_fresh(&self, now: SystemTime) -> bool {
        self.lifetime > self.current_age(now)
    }

    /// Seconds of freshness left, negative once stale (for Cache-Status ttl).
    pub fn ttl_secs(&self, now: SystemTime) -> i64 {
        let age = self.current_age(now).as_secs();
        let life = self.lifetime.as_secs();
        if life >= age {
            i64::try_from(life - age).unwrap_or(i64::MAX)
        } else {
            -i64::try_from(age - life).unwrap_or(i64::MAX)
        }
    }
}

/// How a stored response may be used for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// Fresh enough for this request.
    Fresh,
    /// Stale, but the client accepts it through max-stale.
    StaleAccepted,
    /// Stale inside the stale-while-revalidate window: serve it and refresh
    /// in the background.
    StaleWhileRevalidate,
    /// Must be validated with the origin before use.
    Revalidate,
}

/// Turns stale serving off: RFC 9111 section 4.2.4 forbids it under these
/// directives, and RFC 5861 defers to them.
fn stale_forbidden(resp: &CacheControl) -> bool {
    resp.no_cache || resp.must_revalidate || resp.proxy_revalidate || resp.s_maxage.is_some()
}

/// Decides how a stored response may be used (RFC 9111 section 4 and
/// RFC 5861 section 3).
pub fn decide(f: &Freshness, resp: &CacheControl, req: &CacheControl, now: SystemTime) -> Use {
    if req.no_cache || resp.no_cache {
        return Use::Revalidate;
    }
    let age = f.current_age(now);
    if req.max_age.is_some_and(|m| age > Duration::from_secs(m)) {
        return Use::Revalidate;
    }
    if let Some(min_fresh) = req.min_fresh
        && f.lifetime.saturating_sub(age) < Duration::from_secs(min_fresh)
    {
        return Use::Revalidate;
    }
    if f.lifetime > age {
        return Use::Fresh;
    }
    if stale_forbidden(resp) {
        return Use::Revalidate;
    }
    let staleness = age - f.lifetime;
    if req
        .max_stale
        .is_some_and(|m| staleness <= Duration::from_secs(m))
    {
        return Use::StaleAccepted;
    }
    if resp
        .stale_while_revalidate
        .is_some_and(|w| staleness <= Duration::from_secs(w))
    {
        return Use::StaleWhileRevalidate;
    }
    Use::Revalidate
}

/// Whether a stored response may stand in for a failed origin request
/// (RFC 5861 section 4). Either the response or the request may grant it.
pub fn stale_if_error_allows(
    f: &Freshness,
    resp: &CacheControl,
    req: &CacheControl,
    now: SystemTime,
) -> bool {
    if stale_forbidden(resp) {
        return false;
    }
    let staleness = f.staleness(now);
    [resp.stale_if_error, req.stale_if_error]
        .into_iter()
        .flatten()
        .any(|w| staleness <= Duration::from_secs(w))
}

/// Validators to send when revalidating a stored response (RFC 9111
/// section 4.3.1): the entity tag if there is one, otherwise Last-Modified.
pub fn conditional_headers(stored: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    if let Some(etag) = stored.get(header::ETAG) {
        return vec![(header::IF_NONE_MATCH, etag.clone())];
    }
    match stored.get(header::LAST_MODIFIED) {
        Some(lm) => vec![(header::IF_MODIFIED_SINCE, lm.clone())],
        None => Vec::new(),
    }
}

/// Whether a 304 refers to the stored response: when the 304 carries an
/// entity tag it must match the stored one (RFC 9111 section 4.3.4).
pub fn not_modified_matches(stored: &HeaderMap, not_modified: &HeaderMap) -> bool {
    match (
        EntityTag::from_header(stored),
        EntityTag::from_header(not_modified),
    ) {
        (_, None) => true,
        (Some(s), Some(n)) => s.weak_eq(&n),
        (None, Some(_)) => false,
    }
}

/// Fields that a 304 must not overwrite in the stored response (RFC 9111
/// section 3.2): framing fields, plus hop-by-hop fields, which are already
/// stripped from both maps.
const KEEP_ON_UPDATE: [HeaderName; 3] = [
    header::CONTENT_LENGTH,
    header::CONTENT_RANGE,
    header::TRANSFER_ENCODING,
];

/// Applies the header fields of a 304 to a stored response's headers.
pub fn merge_not_modified(stored: &HeaderMap, not_modified: &HeaderMap) -> HeaderMap {
    let mut out = stored.clone();
    for name in not_modified.keys() {
        if KEEP_ON_UPDATE.contains(name) {
            continue;
        }
        out.remove(name);
        for value in not_modified.get_all(name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::headers::http_date;

    const DAY: Duration = Duration::from_secs(86_400);

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn date(secs: u64) -> String {
        http_date(t(secs)).to_str().unwrap().to_string()
    }

    fn store_check(
        req: &[(&str, &str)],
        status: u16,
        resp: &[(&str, &str)],
    ) -> Result<(), NotStorable> {
        store_check_method(Method::GET, req, status, resp)
    }

    fn store_check_method(
        method: Method,
        req: &[(&str, &str)],
        status: u16,
        resp: &[(&str, &str)],
    ) -> Result<(), NotStorable> {
        let rq = map(req);
        let rs = map(resp);
        let (rq_cc, rs_cc) = (
            CacheControl::from_headers(&rq),
            CacheControl::from_headers(&rs),
        );
        let x = Exchange {
            method: &method,
            req_headers: &rq,
            req_cc: &rq_cc,
            status: StatusCode::from_u16(status).unwrap(),
            resp_headers: &rs,
            resp_cc: &rs_cc,
            response_time: t(0),
        };
        storable(&x, DAY)
    }

    #[test]
    fn storable_basics() {
        assert_eq!(
            store_check(&[], 200, &[("cache-control", "max-age=60")]),
            Ok(())
        );
        assert_eq!(
            store_check_method(Method::POST, &[], 200, &[("cache-control", "max-age=60")]),
            Err(NotStorable::Method)
        );
        assert_eq!(
            store_check(
                &[("cache-control", "no-store")],
                200,
                &[("cache-control", "max-age=60")]
            ),
            Err(NotStorable::RequestNoStore)
        );
        assert_eq!(
            store_check(&[], 206, &[("cache-control", "max-age=60")]),
            Err(NotStorable::Status)
        );
        assert_eq!(
            store_check(&[], 500, &[("cache-control", "max-age=60")]),
            Err(NotStorable::Status)
        );
        assert_eq!(
            store_check(&[], 200, &[("cache-control", "no-store, max-age=60")]),
            Err(NotStorable::NoStore)
        );
        assert_eq!(
            store_check(&[], 200, &[("cache-control", "private, max-age=60")]),
            Err(NotStorable::Private)
        );
        assert_eq!(
            store_check(&[], 200, &[("cache-control", "max-age=60"), ("vary", "*")]),
            Err(NotStorable::VaryStar)
        );
        assert_eq!(
            store_check(
                &[],
                200,
                &[("cache-control", "max-age=60"), ("set-cookie", "a=b")]
            ),
            Err(NotStorable::SetCookie)
        );
    }

    #[test]
    fn must_understand_overrides_no_store() {
        assert_eq!(
            store_check(
                &[],
                200,
                &[("cache-control", "no-store, must-understand, max-age=5")]
            ),
            Ok(())
        );
        assert_eq!(
            store_check(
                &[],
                299,
                &[("cache-control", "no-store, must-understand, max-age=5")]
            ),
            Err(NotStorable::Status)
        );
    }

    #[test]
    fn authorization_needs_explicit_permission() {
        let auth = [("authorization", "Bearer x")];
        assert_eq!(
            store_check(&auth, 200, &[("cache-control", "max-age=60")]),
            Err(NotStorable::Authorization)
        );
        for ok in [
            "public, max-age=60",
            "s-maxage=60",
            "must-revalidate, max-age=60",
        ] {
            assert_eq!(
                store_check(&auth, 200, &[("cache-control", ok)]),
                Ok(()),
                "{ok}"
            );
        }
    }

    #[test]
    fn status_needs_explicit_freshness_unless_heuristic() {
        assert_eq!(
            store_check(&[], 302, &[("etag", "\"a\"")]),
            Err(NotStorable::Status)
        );
        assert_eq!(
            store_check(&[], 302, &[("cache-control", "max-age=5")]),
            Ok(())
        );
        assert_eq!(
            store_check(&[], 302, &[("cache-control", "public"), ("etag", "\"a\"")]),
            Ok(())
        );
        assert_eq!(store_check(&[], 302, &[("expires", &date(10))]), Ok(()));
        assert_eq!(store_check(&[], 404, &[("etag", "\"a\"")]), Ok(()));
    }

    #[test]
    fn never_reusable_responses_are_not_stored() {
        assert_eq!(store_check(&[], 200, &[]), Err(NotStorable::NeverReusable));
        assert_eq!(
            store_check(&[], 200, &[("cache-control", "max-age=0")]),
            Err(NotStorable::NeverReusable)
        );
        assert_eq!(
            store_check(
                &[],
                200,
                &[("cache-control", "no-cache"), ("etag", "\"a\"")]
            ),
            Ok(())
        );
        assert_eq!(
            store_check(&[], 200, &[("last-modified", &date(0))]),
            Ok(())
        );
        assert_eq!(
            store_check(
                &[],
                200,
                &[("cache-control", "max-age=0, stale-if-error=60")]
            ),
            Ok(())
        );
        assert_eq!(
            store_check(
                &[],
                200,
                &[("cache-control", "max-age=0, stale-while-revalidate=5")]
            ),
            Ok(())
        );
    }

    #[test]
    fn reasons_have_names() {
        let all = [
            NotStorable::Method,
            NotStorable::RequestNoStore,
            NotStorable::Status,
            NotStorable::NoStore,
            NotStorable::Private,
            NotStorable::Authorization,
            NotStorable::VaryStar,
            NotStorable::SetCookie,
            NotStorable::NeverReusable,
        ];
        let names: std::collections::HashSet<_> = all.iter().map(|r| r.as_str()).collect();
        assert_eq!(names.len(), all.len());
    }

    fn lifetime(status: u16, resp: &[(&str, &str)]) -> Duration {
        let h = map(resp);
        freshness_lifetime(
            StatusCode::from_u16(status).unwrap(),
            &h,
            &CacheControl::from_headers(&h),
            t(0),
            DAY,
        )
    }

    #[test]
    fn lifetime_precedence() {
        let s = |n| Duration::from_secs(n);
        assert_eq!(
            lifetime(200, &[("cache-control", "max-age=60, s-maxage=30")]),
            s(30)
        );
        assert_eq!(
            lifetime(
                200,
                &[("cache-control", "max-age=60"), ("expires", &date(10))]
            ),
            s(60)
        );
        assert_eq!(
            lifetime(200, &[("date", &date(0)), ("expires", &date(100))]),
            s(100)
        );
        assert_eq!(lifetime(200, &[("expires", &date(100))]), s(100));
        assert_eq!(
            lifetime(200, &[("date", &date(100)), ("expires", &date(50))]),
            s(0)
        );
        assert_eq!(lifetime(200, &[("expires", "0")]), s(0));
    }

    #[test]
    fn heuristic_lifetime() {
        let s = |n| Duration::from_secs(n);
        let lm = UNIX_EPOCH + Duration::from_secs(1_700_000_000 - 1000);
        let lm = http_date(lm).to_str().unwrap().to_string();
        assert_eq!(
            lifetime(200, &[("date", &date(0)), ("last-modified", &lm)]),
            s(100)
        );
        assert_eq!(lifetime(200, &[("last-modified", &lm)]), s(100));
        assert_eq!(lifetime(302, &[("last-modified", &lm)]), s(0));
        assert_eq!(
            lifetime(302, &[("last-modified", &lm), ("cache-control", "public")]),
            s(100)
        );
        assert_eq!(lifetime(200, &[("last-modified", &date(50))]), s(0));
        assert_eq!(lifetime(200, &[]), s(0));
        let old = http_date(UNIX_EPOCH).to_str().unwrap().to_string();
        assert_eq!(lifetime(200, &[("last-modified", &old)]), DAY);
    }

    fn fresh(resp: &[(&str, &str)], request_time: u64, response_time: u64) -> Freshness {
        let h = map(resp);
        Freshness::compute(
            StatusCode::OK,
            &h,
            &CacheControl::from_headers(&h),
            t(request_time),
            t(response_time),
            DAY,
        )
    }

    #[test]
    fn age_calculation() {
        // Date one second before the response arrived, Age 10, two seconds
        // of response delay: corrected initial age is max(1, 10 + 2) = 12.
        let f = fresh(
            &[
                ("date", &date(9)),
                ("age", "10"),
                ("cache-control", "max-age=60"),
            ],
            8,
            10,
        );
        assert_eq!(f.corrected_initial_age, Duration::from_secs(12));
        assert_eq!(f.current_age(t(20)), Duration::from_secs(22));
        // Clock skew: a Date in the future counts as zero apparent age.
        let f = fresh(&[("date", &date(100))], 10, 10);
        assert_eq!(f.corrected_initial_age, Duration::ZERO);
        // Time running backwards does not underflow.
        assert_eq!(f.current_age(t(0)), Duration::ZERO);
        let f = fresh(&[("date", &date(0))], 10, 5);
        assert_eq!(f.corrected_initial_age, Duration::from_secs(5));
    }

    #[test]
    fn ttl_and_staleness() {
        let f = fresh(&[("cache-control", "max-age=10")], 0, 0);
        assert!(f.is_fresh(t(9)));
        assert!(!f.is_fresh(t(10)));
        assert_eq!(f.ttl_secs(t(4)), 6);
        assert_eq!(f.ttl_secs(t(15)), -5);
        assert_eq!(f.staleness(t(4)), Duration::ZERO);
        assert_eq!(f.staleness(t(15)), Duration::from_secs(5));
    }

    fn use_of(resp: &str, req: &str, at: u64) -> Use {
        let f = fresh(&[("cache-control", resp)], 0, 0);
        decide(
            &f,
            &CacheControl::parse_str(resp),
            &CacheControl::parse_str(req),
            t(at),
        )
    }

    #[test]
    fn decide_fresh_and_stale() {
        assert_eq!(use_of("max-age=10", "", 5), Use::Fresh);
        assert_eq!(use_of("max-age=10", "", 10), Use::Revalidate);
        assert_eq!(
            use_of("max-age=10, stale-while-revalidate=5", "", 12),
            Use::StaleWhileRevalidate
        );
        assert_eq!(
            use_of("max-age=10, stale-while-revalidate=5", "", 15),
            Use::StaleWhileRevalidate
        );
        assert_eq!(
            use_of("max-age=10, stale-while-revalidate=5", "", 16),
            Use::Revalidate
        );
    }

    #[test]
    fn decide_request_directives() {
        assert_eq!(use_of("max-age=10", "no-cache", 1), Use::Revalidate);
        assert_eq!(use_of("max-age=10", "max-age=0", 1), Use::Revalidate);
        assert_eq!(use_of("max-age=10", "max-age=5", 3), Use::Fresh);
        assert_eq!(use_of("max-age=10", "min-fresh=5", 3), Use::Fresh);
        assert_eq!(use_of("max-age=10", "min-fresh=5", 6), Use::Revalidate);
        assert_eq!(use_of("max-age=10", "max-stale=5", 14), Use::StaleAccepted);
        assert_eq!(use_of("max-age=10", "max-stale=5", 16), Use::Revalidate);
        assert_eq!(
            use_of("max-age=10", "max-stale", 10_000),
            Use::StaleAccepted
        );
    }

    #[test]
    fn decide_response_directives_forbid_stale() {
        assert_eq!(use_of("no-cache, max-age=10", "", 1), Use::Revalidate);
        for d in ["must-revalidate", "proxy-revalidate"] {
            let resp = format!("max-age=10, stale-while-revalidate=50, {d}");
            assert_eq!(use_of(&resp, "max-stale", 12), Use::Revalidate, "{d}");
            assert_eq!(use_of(&resp, "", 5), Use::Fresh, "{d}");
        }
        assert_eq!(
            use_of("s-maxage=10, stale-while-revalidate=50", "", 12),
            Use::Revalidate
        );
    }

    fn sie(resp: &str, req: &str, at: u64) -> bool {
        let f = fresh(&[("cache-control", resp)], 0, 0);
        stale_if_error_allows(
            &f,
            &CacheControl::parse_str(resp),
            &CacheControl::parse_str(req),
            t(at),
        )
    }

    #[test]
    fn stale_if_error_window() {
        assert!(sie("max-age=10, stale-if-error=60", "", 70));
        assert!(!sie("max-age=10, stale-if-error=60", "", 71));
        assert!(!sie("max-age=10", "", 11));
        assert!(sie("max-age=10", "stale-if-error=5", 15));
        assert!(!sie(
            "max-age=10, stale-if-error=60, must-revalidate",
            "",
            11
        ));
        assert!(!sie("max-age=10, stale-if-error=60, no-cache", "", 11));
        assert!(sie("max-age=10, stale-if-error=0", "", 5));
    }

    #[test]
    fn conditional_request_headers() {
        let h = map(&[("etag", "\"v1\""), ("last-modified", &date(0))]);
        assert_eq!(
            conditional_headers(&h),
            vec![(header::IF_NONE_MATCH, HeaderValue::from_static("\"v1\""))]
        );
        let h = map(&[("last-modified", &date(0))]);
        assert_eq!(conditional_headers(&h)[0].0, header::IF_MODIFIED_SINCE);
        assert!(conditional_headers(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn not_modified_validator_check() {
        let stored = map(&[("etag", "\"v1\"")]);
        assert!(not_modified_matches(&stored, &map(&[("etag", "\"v1\"")])));
        assert!(not_modified_matches(&stored, &map(&[("etag", "W/\"v1\"")])));
        assert!(!not_modified_matches(&stored, &map(&[("etag", "\"v2\"")])));
        assert!(not_modified_matches(&stored, &HeaderMap::new()));
        assert!(!not_modified_matches(
            &HeaderMap::new(),
            &map(&[("etag", "\"v1\"")])
        ));
    }

    #[test]
    fn merge_keeps_framing_fields() {
        let stored = map(&[
            ("content-length", "100"),
            ("content-type", "text/html"),
            ("cache-control", "max-age=10"),
            ("x-a", "1"),
            ("x-a", "2"),
        ]);
        let nm = map(&[
            ("content-length", "0"),
            ("cache-control", "max-age=99"),
            ("x-a", "3"),
            ("x-new", "y"),
        ]);
        let out = merge_not_modified(&stored, &nm);
        assert_eq!(out["content-length"], "100");
        assert_eq!(out["content-type"], "text/html");
        assert_eq!(out["cache-control"], "max-age=99");
        assert_eq!(out.get_all("x-a").iter().collect::<Vec<_>>(), ["3"]);
        assert_eq!(out["x-new"], "y");
    }
}
