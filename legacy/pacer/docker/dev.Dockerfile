# Toolchain image for the gate. Everything `make check` needs, nothing else.
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      clang-18 clang-format-18 clang-tidy-18 llvm-18 libclang-rt-18-dev \
      make git ca-certificates python3 \
 && rm -rf /var/lib/apt/lists/* \
 && for t in clang clang-format clang-tidy llvm-profdata llvm-cov llvm-symbolizer; do \
      ln -sf "/usr/bin/$t-18" "/usr/local/bin/$t"; \
    done

WORKDIR /src
