# frankenrust-dev — the image scripts/dev.sh runs every Rust gate step
# inside: `cargo build`, `cargo fmt`, `cargo clippy`, `cargo test`.
#
# Why this image has to exist: this port links against libphp
# (vendor/frankenphp/frankenphp.c:24 includes <sapi/embed/php_embed.h>) and
# the whole design requires ZTS. The machine this loop runs on has neither —
# no Rust toolchain at all (`~/.rustup/` has no `toolchains/`), and the only
# PHP is Homebrew's, which is non-ZTS and has no `embed` SAPI
# (`php-config --php-sapis` -> "apache2handler cli fpm phpdbg cgi"). See
# issue #5 for the verification. `cargo build` cannot work on the host; it
# can work in here.
#
# Base: php:8.5-zts-trixie, pinned by digest below. This is the SAME base
# docker/pasir.Dockerfile builds against — read the fairness notes there
# (docker/pasir.Dockerfile:14-35), they apply here too — and the same PHP
# 8.5.9-on-trixie build that dunglas/frankenphp:latest ships, so a build that
# succeeds here is exercising the ABI the benchmark images actually run
# against. Digest-pinned (not just tag-pinned) because a silent patch bump
# would change the struct layouts bindgen generates against in #7 without
# anyone noticing until a benchmark run behaved strangely.
#
#   php:8.5-zts-trixie, linux/arm64, resolved 2026-08-13:
#   docker.io/library/php@sha256:91bb0745c5045ad9872bbe87151b73808ead8a8a8c382157be6282b200dfb040
ARG PHP_DIGEST=sha256:91bb0745c5045ad9872bbe87151b73808ead8a8a8c382157be6282b200dfb040
ARG RUST_VERSION=1.97.1
ARG VARIANT=trixie

# Copy a prebuilt toolchain in from the official Rust image rather than
# `curl | sh` from rustup.rs inside the PHP image — a pinned, versioned layer
# instead of a network fetch of "whatever rustup.rs serves today". Same
# pattern docker/pasir.Dockerfile uses for the same reason.
FROM rust:${RUST_VERSION}-slim-${VARIANT} AS rust-toolchain

FROM php@${PHP_DIGEST}

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup

# The official rust:* images run `rustup-init --profile minimal`, which lays
# down cargo, rustc and rust-std and nothing else. The COPY above therefore
# brings in the rustup *proxy shims* /usr/local/cargo/bin/{rustfmt,cargo-fmt,
# cargo-clippy} — they exist, so the image looks complete — but the components
# they proxy to were never installed, and both `cargo fmt` and `cargo clippy`
# die with "not installed for the toolchain". scripts/gate.sh runs both, so
# without this line the fmt and clippy steps fail for every possible repo
# state, forever.
#
# The three --version calls are the assertion, not decoration: an incomplete
# toolchain must break the image build here, where the error names the cause,
# rather than a gate run in some other worktree weeks from now. Same reasoning
# as the PHP capability check below.
RUN set -eux; \
    rustup component add rustfmt clippy; \
    cargo --version; \
    cargo fmt --version; \
    cargo clippy --version

# libclang-dev is what bindgen (used by #7's build.rs to generate FFI
# bindings against php.h) needs to parse PHP's headers.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends libclang-dev; \
    rm -rf /var/lib/apt/lists/*

# Fail the image build, not a random gate run weeks from now, if this base
# ever stops shipping what frankenphp.c and #7's build.rs both assume: ZTS
# PHP with the embed SAPI compiled in. vendor/frankenphp/frankenphp.c:24
# includes <sapi/embed/php_embed.h> directly, and there is no ZTS fallback
# anywhere in this design — see docs/PORTING-NOTES.md.
#
# --disable-zend-signals is checked alongside them because upstream lists it
# as mandatory (vendor/frankenphp/docs/compile.md:50-56) and because it is
# unobservable from PHP: unlike the other two it has no constant and no SAPI
# entry, so nothing downstream would ever notice its absence — the port would
# just install PHP's signal handlers in a threaded process and misbehave under
# load. Verified present in both this base and dunglas/frankenphp:latest (both
# PHP 8.5.9): "--enable-embed --enable-zts --disable-zend-signals". Upstream's
# doc also lists --enable-zend-max-execution-timers, which neither image
# carries, so it is deliberately not asserted here.
RUN set -eux; \
    sapis="$(php-config --php-sapis)"; \
    echo "php-config --php-sapis: $sapis"; \
    case " $sapis " in \
      *" embed "*) ;; \
      *) echo "FATAL: embed SAPI missing from php-config --php-sapis ('$sapis')" >&2; exit 1 ;; \
    esac; \
    zts="$(php -r 'echo PHP_ZTS;')"; \
    echo "PHP_ZTS=$zts"; \
    [ "$zts" = "1" ] || { echo "FATAL: PHP is not built --enable-zts (PHP_ZTS=$zts)" >&2; exit 1; }; \
    opts="$(php-config --configure-options)"; \
    case " $opts " in \
      *" --disable-zend-signals "*) echo "zend signals: disabled" ;; \
      *) echo "FATAL: PHP is not built --disable-zend-signals ('$opts')" >&2; exit 1 ;; \
    esac

WORKDIR /work
