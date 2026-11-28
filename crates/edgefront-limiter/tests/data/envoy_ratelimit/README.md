`basic_config.yaml` and `wildcard.yaml` are envoyproxy/ratelimit's test
configs (`test/config/` at https://github.com/envoyproxy/ratelimit, Apache
License 2.0), copied unchanged. `src/descriptor/tests.rs` checks every
`GetLimit` case of that repository's `test/config/config_test.go` that uses
them (`TestBasicConfig`, `TestWildcardConfig`) and its error messages.

`integration/` holds the files the service's own integration run gives
Envoy and the service, copied unchanged from commit 9b8d6515 of the same
repository (the version the benchmark's envoyproxy/ratelimit target runs):

| file | upstream path | SHA-256 |
|---|---|---|
| `integration/proxy.yaml` | `examples/envoy/proxy.yaml` | `46c995b403b71cbaafd19f51b43dbe3aa2eca8fcb901d7915c6d3ff96dfd28fb` |
| `integration/example.yaml` | `examples/ratelimit/config/example.yaml` | `0b14a793a26bb459476de71867a9b4086267d6223053cb3a61d32eec4426d32f` |

`tests/rls_envoy.rs` runs Envoy on `proxy.yaml`, with only its cluster
addresses pointed at the test's listeners, against the service on
`example.yaml`, and replays the scripts under `integration-test/scripts/`.
