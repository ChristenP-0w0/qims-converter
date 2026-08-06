# syntax=docker/dockerfile:1.7
#
# The QIMS document converter: a stateless HTTP service wrapping a
# LibreOffice/poppler pipeline. POST /convert?name=<filename> with the file as
# the body; it answers with editor-ready HTML, page geometry and salvaged
# footers.
#
# Build from this directory:
#   docker build -t <your-registry>/qims/converter:<gitsha> .
#
# SECURITY — this service has NO authentication and CORS is allow-any. It runs
# headless LibreOffice over caller-supplied files, which is a large parsing
# surface. It must never be exposed through an ingress. Deploy it as a
# ClusterIP service with no ingress rule and let an authenticated application
# proxy to it, so that application enforces auth on every conversion.

# ---------- builder ----------
# edition = "2024" in Cargo.toml needs a recent toolchain; rust:1-bookworm
# tracks stable and matches the runtime's glibc.
FROM rust:1-bookworm AS builder

WORKDIR /src

# Compile dependencies against a stub main first, so editing src/ does not
# invalidate the (slow) dependency layer. surrealdb dominates that cost and is
# still compiled in even though convert-only mode never opens a connection.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# Cargo keys off mtime; the stub's timestamp would otherwise look current.
RUN touch src/main.rs && cargo build --release --locked

# ---------- runtime ----------
FROM debian:bookworm-slim AS runtime

# What each package is for:
#   libreoffice-writer  doc/docx/odt/rtf -> xhtml, and wmf/emf -> png
#   poppler-utils       pdftohtml + pdftotext, the PDF path
#   unzip               unpacking OOXML to salvage embedded media
#   imagemagick         trims the canvas LibreOffice pads around metafiles
#
# The fonts are not cosmetic. The converter derives page geometry and
# pagination from LibreOffice's rendering, so metric-compatible substitutes
# for the fonts these documents actually use decide whether the imported page
# breaks land where the author put them. Carlito matches Calibri, Caladea
# matches Cambria, Liberation matches Arial/Times/Courier.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libreoffice-writer \
        poppler-utils \
        unzip \
        imagemagick \
        fonts-liberation2 \
        fonts-crosextra-carlito \
        fonts-crosextra-caladea \
        fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*

# Debian ships ImageMagick 6, whose binary is `convert`; the converter calls
# `magick` (the v7 name) and silently keeps the untrimmed image when it is
# missing. The v6 CLI accepts these arguments unchanged, so a shim buys back
# the trimming rather than leaving whitespace around every metafile.
RUN printf '#!/bin/sh\nexec convert "$@"\n' > /usr/local/bin/magick \
    && chmod +x /usr/local/bin/magick

COPY --from=builder /src/target/release/qims-backend /usr/local/bin/qims-backend

# Unprivileged, with a writable HOME: LibreOffice insists on one even though
# each conversion points -env:UserInstallation at its own scratch profile.
RUN useradd --system --create-home --home-dir /home/qims --shell /usr/sbin/nologin qims
USER qims
ENV HOME=/home/qims

# Drops the legacy SurrealDB routes so the binary serves /convert and /health
# alone — without this it tries to reach a database that no longer exists.
ENV QIMS_CONVERT_ONLY=1
# The binary defaults to loopback, which is unreachable from outside the pod.
ENV QIMS_BIND=0.0.0.0:8787

EXPOSE 8787

# Deliberately no HEALTHCHECK: the image carries no curl/wget, and the
# platform should own liveness. Point a probe at GET /health, which answers
# "ok" without touching the conversion pipeline.
CMD ["qims-backend"]
