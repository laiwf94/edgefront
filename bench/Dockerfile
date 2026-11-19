# syntax=docker/dockerfile:1
# The benchmark images, built from this repository by `edgefront-bench run`
# (or examples/bench.sh):
#
#   --target edgefront  the edgefront binary alone, the image the cache
#                       comparison measures (edgefront-bench/edgefront)
#   --target harness    edgefront-bench, for the origin and the load
#                       generator (edgefront-bench/harness)
#   --target steer      steer, the Go proxy edgefront's proxy is ported
#                       from, built from legacy/steer at the last commit
#                       that had it, as its own Dockerfile built it
#                       (edgefront-bench/steer), on edgefront's base image
#
# The runner tags them local-<hash of the checkout's path>, so checkouts
# sharing one Docker never run each other's builds; EDGEFRONT_BENCH_TAG
# overrides the tag.
#
# Both come from one build stage, so building the second reuses the first.
FROM rust:1.99-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src
COPY benches ./benches
COPY examples ./examples
# The result schema, which edgefront-bench embeds.
COPY bench/schema ./bench/schema
# The registry and the target directory live in BuildKit caches, so a
# rebuild after a source edit recompiles only what changed.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin edgefront --bin edgefront-bench \
    && mkdir -p /out \
    && cp target/release/edgefront target/release/edgefront-bench /out/

FROM gcr.io/distroless/cc-debian13:nonroot AS harness
COPY --from=build /out/edgefront-bench /edgefront-bench
USER nonroot:nonroot
ENTRYPOINT ["/edgefront-bench"]

FROM gcr.io/distroless/cc-debian13:nonroot AS edgefront
COPY --from=build /out/edgefront /edgefront
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/edgefront"]
CMD ["--config", "/etc/edgefront/edgefront.toml"]

# steer as its own Dockerfile built it: static, trimmed, stripped. Its
# source is legacy/steer at the last commit that had it, fetched from the
# repository, so the image is pinned by that commit.
FROM golang:1.27.4-alpine AS steer-build
ARG STEER_REV=8f0c2a5d45975259c0e385ada04dd47cd1eb2057
ADD https://github.com/laiwf94/edgefront.git#${STEER_REV}:legacy/steer /src
WORKDIR /src
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/steer ./cmd/steer

FROM gcr.io/distroless/cc-debian13:nonroot AS steer
COPY --from=steer-build /out/steer /steer
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/steer"]
