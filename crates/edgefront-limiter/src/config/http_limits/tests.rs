use super::*;

fn section(text: &str) -> Vec<HttpLimitConfig> {
    toml::from_str::<HttpLimitsSection>(text)
        .unwrap()
        .http_limits
}

fn err(body: &str) -> String {
    let text = format!("[[http_limits]]\nname = \"r\"\nrate = \"10/s\"\n{body}\n");
    match toml::from_str::<HttpLimitsSection>(&text) {
        Ok(s) => build(&s.http_limits).unwrap_err(),
        Err(e) => e.to_string(),
    }
}

#[test]
fn the_documented_example_builds() {
    let rules = section(
        r#"
        [[http_limits]]
        name = "login"
        routes = [0]
        rate = "5/min burst 5"
        key = ["path", "header:x-forwarded-for"]
        count = [401, 403]
        action = "block"
        table_size = 100000
        table_full = "evict"
        headers = true
        [http_limits.match]
        host = "App.Example.com"
        path_prefix = "/login"
        methods = ["post"]
        [http_limits.response]
        status = 429
        body = "slow down\n"
        content_type = "text/plain"

        [[http_limits]]
        name = "api_keys-2"
        rate = "100/s burst 200"
        key = ["header:x-api-key"]
        count = ["5xx", 499]
        action = "delay"
        max_delay = "500ms"
        "#,
    );
    let built = build(&rules).unwrap();
    let r = &built[0];
    assert_eq!(r.name, "login");
    assert_eq!(r.routes, Some(vec![0]));
    // Rates are kept in lowest terms.
    assert_eq!(r.rate, Rate::new(1, 12 * 1_000_000_000, 5));
    assert_eq!(r.matches.host.as_deref(), Some("app.example.com"));
    assert_eq!(r.matches.methods, [http::Method::POST]);
    assert_eq!(r.count.unwrap().items(), ["401", "403"]);
    assert_eq!(r.action, Action::Block);
    assert_eq!(
        (r.table_size, r.table_full, r.headers),
        (100000, FullPolicy::Evict, true)
    );
    assert_eq!(r.refusal.body, "slow down\n");
    assert_eq!(r.key.selectors().len(), 2);
    let r = &built[1];
    assert_eq!(r.action, Action::Delay(Duration::from_millis(500)));
    assert_eq!(r.count.unwrap().len(), 101);
    assert_eq!(r.table_size, DEFAULT_TABLE_SIZE);
    assert!(lint_configs(&rules, &built).is_empty());
    // The section beside a limiter section and others is read alone.
    let other =
        "workers = 2\n[[listeners]]\nname = \"a\"\n[address_policy]\nheader = \"forwarded\"\n";
    assert!(section(other).is_empty());
    use crate::http::key::Selector;
    assert_eq!(built[0].key.selectors()[0], Selector::Path);
}

use crate::gcra::Rate;

#[test]
fn every_mistake_names_its_field() {
    for (body, want) in [
        (
            "key = [\"ip\"]",
            "http_limits[0].key[0]: unknown key selector \"ip\"",
        ),
        (
            "key = [\"path\", \"path\"]",
            "http_limits[0].key[1]: \"path\" is listed twice",
        ),
        (
            "count = [700]",
            "http_limits[0].count: \"700\" is not a status code 100 to 599",
        ),
        ("count = []", "http_limits[0].count: the list is empty"),
        (
            "action = \"drop\"",
            "http_limits[0].action: \"drop\" is not block, delay or log",
        ),
        (
            "action = \"delay\"",
            "http_limits[0].max_delay: delay needs a max_delay",
        ),
        (
            "action = \"delay\"\nmax_delay = \"2m\"",
            "http_limits[0].max_delay: must be from 1ms to 60s",
        ),
        (
            "action = \"delay\"\nmax_delay = \"soon\"",
            "http_limits[0].max_delay: expected a duration like 250ms or 2s, got \"soon\"",
        ),
        (
            "max_delay = \"x\"",
            "http_limits[0].max_delay: expected a duration",
        ),
        (
            "table_size = 0",
            "http_limits[0].table_size: must be between 1 and 268435456",
        ),
        (
            "table_full = \"drop\"",
            "http_limits[0].table_full: \"drop\" is not evict or deny",
        ),
        (
            "[http_limits.match]\nmethods = [\"GE T\"]",
            "http_limits[0].match.methods[0]: \"GE T\" is not a method",
        ),
        (
            "[http_limits.match]\nmethods = [\"GET\", \"get\"]",
            "http_limits[0].match.methods[1]: \"get\" is listed twice",
        ),
        (
            "[http_limits.match]\npath_prefix = \"api\"",
            "http_limits[0].match.path_prefix: \"api\" must start with /",
        ),
        (
            "[http_limits.match]\npath = \"/a//b\"",
            "http_limits[0].match.path: \"/a//b\" is not a clean path",
        ),
        (
            "[http_limits.match]\nhost = \"a b\"",
            "http_limits[0].match.host: \"a b\" is not a host name",
        ),
        (
            "[http_limits.match]\nheader = \"x\"",
            "http_limits[0].match.header: matching on headers is not supported",
        ),
        (
            "[http_limits.response]\nstatus = 200",
            "http_limits[0].response.status: 200 is not a status from 400 to 599",
        ),
        (
            "[http_limits.response]\ncontent_type = \"\"",
            "http_limits[0].response.content_type: \"\" is not a header value",
        ),
        ("unknown = 1", "unknown field `unknown`"),
    ] {
        let got = err(body);
        assert!(got.contains(want), "{body}: {got}");
    }
    let long = format!("[http_limits.response]\nbody = \"{}\"", "x".repeat(5000));
    assert!(err(&long).contains("response.body: longer than 4096 bytes"));
    let r = |rate: &str, name: &str| {
        let t = format!("[[http_limits]]\nname = \"{name}\"\nrate = \"{rate}\"\n");
        build(&section(&t)).unwrap_err()
    };
    assert!(
        r("10/day", "q").contains("rate: \"10/day\" is a quota window"),
        "{}",
        r("10/day", "q")
    );
    assert!(r("ten", "q").starts_with("http_limits[0].rate: "));
    assert!(r("1/s", "a b").contains("name: \"a b\" must be 1 to 63"));
    assert!(r("1/s", "").contains("name: \"\" must be"));
    let dup = section(
        "[[http_limits]]\nname = \"a\"\nrate = \"1/s\"\n[[http_limits]]\nname = \"a\"\nrate = \"1/s\"\n",
    );
    assert_eq!(
        build(&dup).unwrap_err(),
        "http_limits[1].name: \"a\" is used twice"
    );
}

#[test]
fn lints_find_keys_that_can_never_match() {
    let rules = section(
        r#"
        [[http_limits]]
        name = "outside"
        rate = "1/s"
        key = ["path_prefix:/admin"]
        [http_limits.match]
        path_prefix = "/api"

        [[http_limits]]
        name = "inside"
        rate = "1/s"
        key = ["path_prefix:/api/v1"]
        [http_limits.match]
        path_prefix = "/api"

        [[http_limits]]
        name = "path-outside"
        rate = "1/s"
        [http_limits.match]
        path_prefix = "/api"
        path = "/login"

        [[http_limits]]
        name = "path-key"
        rate = "1/s"
        key = ["path_prefix:/api"]
        [http_limits.match]
        path = "/login"

        [[http_limits]]
        name = "host-header"
        rate = "1/s"
        key = ["header:host"]

        [[http_limits]]
        name = "own-status"
        rate = "1/s"
        count = ["4xx"]

        [[http_limits]]
        name = "no-delay"
        rate = "1/s"
        max_delay = "1s"
        "#,
    );
    let built = build(&rules).unwrap();
    let found: Vec<String> = lint_configs(&rules, &built)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(
        found,
        [
            "warning[http-key-never-matches]: http limit outside: key path_prefix:/admin lies outside match.path_prefix /api, so no request has the key and the rule never applies",
            "warning[http-key-never-matches]: http limit path-outside: match.path /login lies outside match.path_prefix /api, so the rule never applies",
            "warning[http-key-never-matches]: http limit path-key: key path_prefix:/api does not cover match.path /login, so no request has the key and the rule never applies",
            "warning[http-key-host-header]: http limit host-header: key header:host finds nothing in HTTP/2 requests, which carry :authority; the host selector reads both",
            "warning[http-count-own-status]: http limit own-status: count includes 429, the rule's own refusal, which is answered before any upstream and never counted; only upstream 429 responses are",
            "warning[http-delay-unused]: http limit no-delay: max_delay is set but the action is block, so it never delays",
        ]
    );
}
