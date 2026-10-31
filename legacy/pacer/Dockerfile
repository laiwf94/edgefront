# Runtime image: static pacerd and pacerctl on an empty base, running as an
# unprivileged user. Build: docker build -t pacerd .
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac AS build
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      clang-18 make libc6-dev \
 && rm -rf /var/lib/apt/lists/* \
 && ln -s /usr/bin/clang-18 /usr/local/bin/clang
WORKDIR /src
COPY Makefile ./
COPY include include
COPY src src
RUN make VARIANT=release STATIC=1 build/release/pacerd build/release/pacerctl \
 && mkdir -p /out/run/pacerd /out/tmp \
 && chmod 1777 /out/tmp

FROM scratch
COPY --from=build /src/build/release/pacerd /src/build/release/pacerctl /usr/local/bin/
COPY --from=build --chown=65532:65532 /out/run /run
COPY --from=build /out/tmp /tmp
COPY examples/pacerd.conf /etc/pacerd/pacerd.conf
USER 65532:65532
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/pacerd"]
CMD ["-c", "/etc/pacerd/pacerd.conf"]
