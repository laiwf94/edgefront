# Vendored protobuf definitions

The decision service speaks Envoy's rate limit service API,
`envoy.service.ratelimit.v3`. These are the `.proto` files it is compiled
from, taken from Envoy's `api/` tree at tag v1.40.2 (commit
b2d89e0ae53916f28770771fbdd49c5c99c5c5a2), with these changes:

- The `udpa.annotations` and `validate` imports, and every option that uses
  them, are removed. They are annotations for Envoy's own code generators
  and validators; they change nothing on the wire, and keeping them would
  mean vendoring the udpa and protoc-gen-validate trees as well.
- `envoy/config/core/v3/base.proto` keeps only `HeaderValue`, the one message
  of it the service uses, with its field numbers, types and comments as they
  are upstream. The full file imports a dozen more.

Everything else, including every message, field number, enum value and
comment, is as upstream. The upstream files had these SHA-256 sums:

| File | SHA-256 at v1.40.2 |
|---|---|
| `envoy/service/ratelimit/v3/rls.proto` | `e034ece83cb0b6f1ea7a88c40f28855c2165c542c4cbeb668e2b69d4df448745` |
| `envoy/extensions/common/ratelimit/v3/ratelimit.proto` | `5d70fb53b33fae4dfb866cd57958cf7e68d0559727a2e2d563ea0872dc60feb2` |
| `envoy/type/v3/ratelimit_unit.proto` | `f746bc8695dcd09d12c1df3276b4ea90086b18cc6ee3842a19a3655fad625b0d` |
| `envoy/type/v3/token_bucket.proto` | `267a27afdc735fa581a83d8186361dede7c8b32b076fb185851833c811c7f1e5` |
| `envoy/config/core/v3/base.proto` | `2759bee6cb33ac5206d51f6b9559c08febd88b6f75a0298d0ac9c6c209aefa67` |

The `google/protobuf` well-known types come from protox, which embeds them.

## Generated code

The Rust code is generated from these files and checked in under
`src/service/proto/`, so a build needs neither protoc nor a build script.
`tests/proto_codegen.rs` is the build step: it compiles the files with protox
and generates the code with prost-build, both pinned exactly in the
workspace's `Cargo.toml`, and fails if the result differs from what is
checked in. After changing a file here or a pin, run `make proto` to write
the code again, and commit both.

`make proto` builds the limiter without its `service` feature, so it works
even when a change adds or renames a generated file that the module tree in
`src/service/proto.rs` names before the file exists.
