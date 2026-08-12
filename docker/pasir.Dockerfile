# Pasir (https://github.com/el7cosmos/pasir) — a third-party PHP application
# server written in Rust, built here as a BASELINE for the head-to-head.
#
# Why this image exists: before writing a from-scratch Rust port, we need to
# know whether an existing Rust PHP server actually beats FrankenPHP. If it
# does not, the port is not worth building. Pasir is the closest existing
# analogue: embedded PHP via a custom SAPI (ext-php-rs), Hyper + Tokio front
# end, non-persistent per-request execution.
#
# ---------------------------------------------------------------------------
# FAIRNESS NOTES — read before trusting a number from this image
# ---------------------------------------------------------------------------
#
#  * PHP PARITY. dunglas/frankenphp:latest ships PHP 8.5.9 on Debian 13
#    (trixie). We build on php:8.5-zts-trixie, which is the same PHP 8.5.9 on
#    the same trixie base, produced by the same docker-library/php build. That
#    makes the PHP engine a constant and leaves the server as the variable.
#    Upstream Pasir's own published image is pinned to PHP 8.5.3, so we build
#    from source rather than using it — a patch-version delta in the engine is
#    exactly the kind of thing that quietly explains a 3% result.
#
#  * EMBED SAPI. Pasir needs PHP built with
#    `--enable-embed --enable-zts --disable-zend-signals`. The official
#    php:8.5-zts-trixie image already satisfies all three (verified with
#    `php-config --php-sapis` -> "cli embed phpdbg cgi"), so there is no need
#    to compile php-src. This is the whole reason this Dockerfile is short.
#
#  * NO php.ini. Deliberate, and a change from upstream Pasir's Dockerfile,
#    which copies php.ini-development. FrankenPHP's image loads NO ini file at
#    all ("Loaded Configuration File => (none)"), so it runs on compiled-in
#    defaults. php.ini-development is not a neutral file — among other things
#    it sets variables_order=GPCS, which skips populating $_ENV on every
#    request. Handing Pasir a cheaper request setup than FrankenPHP would
#    flatter it for a reason that has nothing to do with Rust. Both servers
#    therefore run PHP on stock defaults plus the same docker-php conf.d.
#
#  * NO pasir.toml. Pasir's default routing already sends any path ending in
#    "/" straight to the PHP service, resolving "/" to "/index.php" without a
#    filesystem stat. That is its fast path. Supplying a routing config would
#    add per-request regex matching and measure our config, not the server.
#    The "Using default routes" warning at startup is expected and is emitted
#    once, not per request.
#
# Build:
#   docker build --platform linux/arm64 -t pasir:bench -f docker/pasir.Dockerfile .
#
# Contract with bench/harness/run.sh start_server(): serve the document root
# mounted at /app/public over plain HTTP/1.1 on port 80.

ARG PHP_VERSION=8.5
ARG VARIANT=trixie
ARG RUST_VERSION=1
# Pinned to a release tag, not a branch: a benchmark baseline that silently
# changes under you is not a baseline.
ARG PASIR_REF=0.6.0

FROM rust:${RUST_VERSION}-slim-${VARIANT} AS rust-toolchain

# Build inside the PHP image itself. ext-php-rs generates its bindings against
# the headers and libphp.so of the PHP it will link to, so the build and
# runtime PHP must be the same build — not merely the same version.
FROM php:${PHP_VERSION}-zts-${VARIANT} AS builder

ARG PASIR_REF

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup

# libclang is what ext-php-rs's bindgen needs to read the PHP headers.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends libclang-dev git ca-certificates; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /src
RUN git clone --depth 1 --branch "${PASIR_REF}" https://github.com/el7cosmos/pasir.git .

# LIBRARY_PATH is where the official image puts libphp.so; without it the
# final link fails to find -lphp.
#
# No --locked: the Cargo.lock committed at tag 0.6.0 is stale against its own
# manifest and cargo refuses to proceed without refreshing it. Upstream's
# Dockerfile does not pass --locked either. The source is still pinned by tag;
# only transitive dependency patch versions are free to move.
RUN LIBRARY_PATH=/usr/local/lib cargo build --bins --release

# --- runtime -----------------------------------------------------------------
FROM php:${PHP_VERSION}-zts-${VARIANT}

COPY --from=builder /src/target/release/pasir /usr/local/bin/pasir

# Read by clap. Address must be 0.0.0.0 or the container answers only itself.
ENV PASIR_ADDRESS=0.0.0.0 \
    PASIR_PORT=80

WORKDIR /app
EXPOSE 80

CMD ["pasir", "/app/public"]
