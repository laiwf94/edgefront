`basic_config.yaml` and `wildcard.yaml` are envoyproxy/ratelimit's test
configs (`test/config/` at https://github.com/envoyproxy/ratelimit, Apache
License 2.0), copied unchanged. `src/descriptor/tests.rs` checks every
`GetLimit` case of that repository's `test/config/config_test.go` that uses
them (`TestBasicConfig`, `TestWildcardConfig`) and its error messages.
