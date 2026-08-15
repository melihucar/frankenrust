# FrankenRust HTTP server, built here as `frankenrust:bench` for the
# head-to-head against dunglas/frankenphp:latest.
#
# bench/harness/run.sh never builds this image. bake()'s frankenrust|pasir
# branch (run.sh:193-202) only `docker image inspect`s "$server:bench" and
# returns 1 -- SKIP, not a build -- when it is absent. Building it is this
# issue's (#15) job, done once by hand, out of band from the harness.
#
# ---------------------------------------------------------------------------
# FAIRNESS NOTES -- read before trusting a number produced from this image
# ---------------------------------------------------------------------------
#
#  * PHP PARITY. dunglas/frankenphp:latest ships PHP 8.5.9 on trixie. Builder
#    and runtime stages below both pin the exact digest
#    docker/frankenrust-dev.Dockerfile already pins for the same reason: the
#    headers bindgen (crates/frankenrust-sys/build.rs) reads and the
#    libphp.so this binary links and runs against must be the same build,
#    not merely the same version tag -- a silent patch bump on `trixie`
#    between builder and runtime pulls would change struct layouts bindgen
#    generated against without anyone noticing until a benchmark run behaved
#    strangely.
#
#  * NO php.ini. This image loads none, matching dunglas/frankenphp:latest
#    ("Loaded Configuration File => (none)") and docker/pasir.Dockerfile.
#    php.ini-development is not neutral -- among other things it sets
#    variables_order=GPCS, which skips populating $_ENV on every request.
#    Handing ourselves a cheaper request setup than FrankenPHP would flatter
#    us for a reason that has nothing to do with Rust.
#
#  * BUILD INSIDE THE PHP IMAGE ITSELF, so bindgen generates against the same
#    headers and libphp.so the binary will link and run against -- the same
#    build, not merely the same version. This is the pattern
#    docker/frankenrust-dev.Dockerfile established for #5; reused here, not
#    reinvented.
#
# Build:
#   docker build --platform linux/arm64 -t frankenrust:bench -f docker/frankenrust.Dockerfile .
#
# Contract with bench/harness/run.sh start_server() (run.sh:193-228): serve
# the document root at /app/public over plain HTTP/1.1 on port 80, bound to
# 0.0.0.0 (unpublished -- the loadgen container reaches it by container DNS
# name on the frankenbench network), with a self-sufficient CMD, honouring
# FR_THREADS (injected at run.sh:216) as the PHP thread count. No TLS. The
# app itself is baked in afterwards by bench/harness/Dockerfile.app, which
# COPYs a fixture into /app/public/ -- this image ships that directory empty.

ARG PHP_DIGEST=sha256:91bb0745c5045ad9872bbe87151b73808ead8a8a8c382157be6282b200dfb040
ARG RUST_VERSION=1.97.1
ARG VARIANT=trixie

# Copy a prebuilt toolchain in from the official Rust image rather than
# `curl | sh` from rustup.rs inside the PHP image -- a pinned, versioned
# layer instead of a network fetch of "whatever rustup.rs serves today".
# Same pattern docker/pasir.Dockerfile and docker/frankenrust-dev.Dockerfile
# use for the same reason.
FROM rust:${RUST_VERSION}-slim-${VARIANT} AS rust-toolchain

FROM php@${PHP_DIGEST} AS builder

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup

# libclang-dev is what frankenrust-sys/build.rs's bindgen needs to parse
# php.h. A C compiler and php-config already ship in the base php image --
# docker/frankenrust-dev.Dockerfile relies on the same fact to run `cargo
# build`/`cargo test` for the whole gate, so it is already proven true for
# this exact digest.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends libclang-dev; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /src
# frankenrust-sys/build.rs resolves upstream's C sources as
# CARGO_MANIFEST_DIR/../../vendor/frankenphp (build.rs:110), so the vendor
# checkout has to land at that same relative path here -- copied in
# read-only source form, never modified, same as every other consumer of it.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY vendor/frankenphp/ vendor/frankenphp/
RUN cargo build --release --locked -p frankenrust-server

# --- runtime -----------------------------------------------------------------
FROM php@${PHP_DIGEST}

COPY --from=builder /src/target/release/frankenrust-server /usr/local/bin/frankenrust-server

# FRANKENRUST_LISTEN must be 0.0.0.0, not main.rs's loopback default
# (127.0.0.1:8080) -- otherwise the container answers only itself and fails
# contract item 2. FRANKENRUST_DOCUMENT_ROOT is /app/public, matching where
# Dockerfile.app COPYs the fixture and where the (currently disarmed, #199)
# conformance replay mounts vendor/frankenphp/testdata -- not a path to
# index.php itself, which would make DOCUMENT_ROOT a lie in every server var
# and break that replay's directory mount. See #187 for why GET / already
# resolves to index.php without that shortcut.
ENV FRANKENRUST_DOCUMENT_ROOT=/app/public \
    FRANKENRUST_LISTEN=0.0.0.0:80

WORKDIR /app
# Dockerfile.app COPYs a fixture into /app/public/ on top of this image, and
# the conformance replay bind-mounts a testdata tree over the same path
# (common.py:316) -- both expect this directory to already exist so the
# image is well-formed (serves an empty document root, not a startup crash)
# before either happens.
RUN mkdir -p /app/public
EXPOSE 80

# FR_THREADS is injected at `docker run` time (run.sh:216); the binary itself
# only ever reads FRANKENRUST_THREADS (main.rs:18-20), so the rename has to
# happen here, not in main.rs -- roughly 30 open issues target
# crates/frankenrust-server/, and renaming an env var there is a merge
# conflict that discards someone else's work. The ${FR_THREADS:-8} default
# matches config/Caddyfile.bench:20's `num_threads {$FR_THREADS:8}`, so a
# bare `docker run frankenrust:bench` outside the harness (FR_THREADS unset)
# gets the same PHP concurrency budget FrankenPHP would.
CMD ["sh", "-c", "FRANKENRUST_THREADS=\"${FR_THREADS:-8}\" exec frankenrust-server"]
