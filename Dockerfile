# syntax=docker/dockerfile:1
FROM rust:1.99-slim-trixie AS build
WORKDIR /src
# Dependencies first, against stub sources, so source edits reuse the layer.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN mkdir -p src benches examples \
    && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && for b in hit_path layer_overhead; do echo 'fn main() {}' > benches/$b.rs; done \
    && echo 'fn main() {}' > examples/origin.rs \
    && cargo build --release --locked --bin cachet --example origin \
    && rm -rf src benches examples
COPY src ./src
COPY benches ./benches
COPY examples ./examples
RUN touch src/lib.rs src/main.rs examples/origin.rs \
    && cargo build --release --locked --bin cachet --example origin \
    && mkdir -p /cache

# The example origin, for the compose file and the demo.
FROM gcr.io/distroless/cc-debian13:nonroot AS origin
COPY --from=build /src/target/release/examples/origin /origin
USER nonroot:nonroot
EXPOSE 9100
ENTRYPOINT ["/origin", "0.0.0.0:9100"]

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /src/target/release/cachet /cachet
# An empty cache directory owned by the runtime user, so a named volume
# mounted here starts out writable.
COPY --from=build --chown=65532:65532 /cache /var/cache/cachet
USER nonroot:nonroot
EXPOSE 8080 9090
ENTRYPOINT ["/cachet"]
CMD ["--config", "/etc/cachet/cachet.toml"]
