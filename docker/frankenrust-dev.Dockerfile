# frankenrust-dev: the toolchain image every Rust step of scripts/gate.sh runs
# in. Verified on the machine this loop runs on: rustup has no toolchain
# installed, and the only PHP is Homebrew's, which has no `embed` SAPI and no
# ZTS. vendor/frankenphp/frankenphp.c:24 includes <sapi/embed/php_embed.h> and
# the whole design requires ZTS, so `cargo build` linking `-lphp` cannot work
# on the host. This image supplies a PHP that has both, plus a pinned Rust
# toolchain and the libclang bindgen (#7) needs to read PHP's headers.
#
# ---------------------------------------------------------------------------
# FAIRNESS NOTES — read before trusting a build produced by this image
# ---------------------------------------------------------------------------
# (same reasoning as docker/pasir.Dockerfile:14-35; repeated here because this
#  image, not that one, is what actually compiles frankenrust.)
#
#  * PHP PARITY. dunglas/frankenphp:latest ships PHP 8.5.9 on Debian 13
#    (trixie). This image is php:8.5-zts-trixie, the same PHP 8.5.9 on the
#    same trixie base, from the same docker-library/php build — so whatever
#    frankenrust links against at dev/test time is the same engine the bench
#    harness measures against, not a lookalike.
#  * EMBED SAPI + ZTS, VERIFIED NOT ASSUMED. The official image already ships
#    `--enable-embed --enable-zts` (verified directly: `php-config
#    --php-sapis` -> "cli embed phpdbg cgi", `php -r 'echo PHP_ZTS;'` -> "1"),
#    but a base image can change under a floating tag. The RUN step below
#    re-checks both at build time and fails the build loudly if either
#    regresses, instead of every agent independently discovering it as a
#    confusing cargo/link failure.
#
# Build:
#   docker build --platform linux/arm64 -t frankenrust-dev -f docker/frankenrust-dev.Dockerfile .
# Normally you don't invoke this directly — scripts/dev.sh builds it on demand.
#
# Pinned by digest, not just tag: a silent patch bump to either image changes
# the ABI bindgen will generate against (#7), and a toolchain that moves under
# you is not a pinned one.
ARG PHP_DIGEST=sha256:91bb0745c5045ad9872bbe87151b73808ead8a8a8c382157be6282b200dfb040
ARG RUST_DIGEST=sha256:f75071363e7f4771769d4cf81b1b7b290e607f4d4459e8731f6abdcee9982dc8

# Built from the official Rust image rather than rustup-in-place: a pinned
# toolchain image is a cached layer, where installing rustup at build time
# means re-running a network installer on every cold build.
FROM rust:1.91-slim-trixie@${RUST_DIGEST} AS rust-toolchain

FROM php:8.5-zts-trixie@${PHP_DIGEST}

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup

# rust:*-slim ships only rustc+cargo; scripts/gate.sh runs `cargo fmt` and
# `cargo clippy`, which need these components explicitly installed.
RUN rustup component add rustfmt clippy

# libclang is what bindgen (#7) needs to parse php.h and friends. gcc,
# pkg-config and ca-certificates already ship in php:*-zts-trixie (the image
# needs them itself to build extensions); git does not, and cargo wants it
# for git dependencies.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends libclang-dev git; \
    rm -rf /var/lib/apt/lists/*

# Fail the image build, not the first agent's gate run, if a base image
# update ever ships a PHP without what frankenphp.c requires.
RUN set -eux; \
    sapis="$(php-config --php-sapis)"; \
    case " $sapis " in \
      *" embed "*) ;; \
      *) echo "FATAL: php-config --php-sapis has no 'embed' SAPI (got: $sapis)" >&2; exit 1 ;; \
    esac; \
    zts="$(php -r 'echo PHP_ZTS;')"; \
    [ "$zts" = "1" ] || { echo "FATAL: PHP is not built ZTS (PHP_ZTS=$zts)" >&2; exit 1; }; \
    echo "verified: php-config --php-sapis has embed, PHP_ZTS=1"

WORKDIR /work
CMD ["bash"]
