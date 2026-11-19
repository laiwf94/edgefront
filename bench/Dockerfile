# syntax=docker/dockerfile:1
# The benchmark images, built from this repository by `edgefront-bench run`
# (or examples/bench.sh):
#
#   --target edgefront  the edgefront binary alone, the image the cache
#                       comparison measures (edgefront-bench/edgefront:local)
#   --target harness    edgefront-bench, for the origin and the load
#                       generator (edgefront-bench/harness:local)
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
