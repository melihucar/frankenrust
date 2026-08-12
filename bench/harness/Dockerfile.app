# Bakes a benchmark fixture INTO the server image.
#
# The apps were previously bind-mounted from macOS. On Docker Desktop that is a
# VirtioFS mount, and any server that stat()s the document root per request
# pays a large, wildly asymmetric tax: measured 3,248 rps mounted vs 21,317 rps
# baked for the same FrankenPHP build on the same fixture. A benchmark that
# bind-mounts is measuring the filesystem bridge, not the server.
ARG BASE
FROM ${BASE}
ARG APP
COPY ${APP}/ /app/public/
