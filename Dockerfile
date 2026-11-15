# syntax=docker/dockerfile:1.7
FROM debian:bookworm-slim AS media-builder

ARG FFMPEG_VERSION=8.0.3
ARG FFMPEG_SHA256=6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818
ARG BUILD_JOBS=2
# Debian security updates are resolved at build time and recorded in the manifest.
# hadolint ignore=DL3008
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl libdvdnav-dev libdvdread-dev \
    libx264-dev nasm pkg-config xz-utils zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN curl --fail --show-error --location --proto '=https' --tlsv1.2 \
      "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" -o ffmpeg.tar.xz \
    && printf '%s  ffmpeg.tar.xz\n' "$FFMPEG_SHA256" > ffmpeg.sha256 \
    && sha256sum --check --strict ffmpeg.sha256 \
    && tar -xJf ffmpeg.tar.xz --strip-components=1 \
    && ./configure --prefix=/opt/ffmpeg --disable-shared --enable-static \
      --disable-autodetect --disable-doc --disable-debug --disable-ffplay \
      --disable-network --enable-gpl --enable-libx264 --enable-libdvdnav \
      --enable-libdvdread --enable-zlib --arch=x86_64 --cpu=x86-64 \
      --extra-cflags='-O2 -march=x86-64 -mtune=generic' \
    && make -j "$BUILD_JOBS" \
    && make install \
    && mkdir -p /opt/ffmpeg/share \
    && cp COPYING.GPLv2 LICENSE.md /opt/ffmpeg/share/ \
    && printf 'FFmpeg source: https://ffmpeg.org/releases/ffmpeg-%s.tar.xz\nSHA256: %s\n' \
      "$FFMPEG_VERSION" "$FFMPEG_SHA256" > /opt/ffmpeg/share/source.txt

FROM rust:1.94.0-slim-bookworm AS app-builder

ARG BUILD_JOBS=2
# hadolint ignore=DL3008
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown \
    && cargo install trunk --version 0.22.1 --locked --jobs "$BUILD_JOBS"
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ENV CARGO_BUILD_JOBS=${BUILD_JOBS}
# Build on bookworm for glibc 2.36; never inherit host-native CPU flags.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64"
RUN cargo build --release --locked --package wetube-server --package wetube-worker
WORKDIR /build/crates/web
RUN trunk build --release --locked --skip-version-check

FROM debian:bookworm-slim AS runtime

# hadolint ignore=DL3008
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl libdvdnav4 libdvdread8 libx264-164 zlib1g \
    tesseract-ocr tesseract-ocr-eng tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 568 wetube \
    && useradd --uid 568 --gid wetube --no-create-home \
      --home-dir /nonexistent --shell /usr/sbin/nologin wetube
COPY --from=media-builder /opt/ffmpeg/bin/ffmpeg /opt/ffmpeg/bin/ffprobe /usr/local/bin/
COPY --from=media-builder /opt/ffmpeg/share/ /usr/share/wetube/ffmpeg/
COPY --from=app-builder /build/target/release/wetube-server /build/target/release/wetube-worker /usr/local/bin/
COPY --from=app-builder /build/dist/ /opt/wetube/dist/
COPY --chmod=755 docker/entrypoint.sh docker/check-media.sh /usr/local/bin/
RUN /usr/local/bin/check-media.sh \
    && { printf 'Rust: 1.94.0\nTrunk: 0.22.1\n'; \
         wetube-server --version; \
         wetube-worker --version; \
         sha256sum /usr/local/bin/wetube-server /usr/local/bin/wetube-worker; \
         cat /usr/share/wetube/ffmpeg/source.txt; \
         ffmpeg -version; \
         ffprobe -version; \
         dpkg-query -W; \
         sha256sum /usr/share/tesseract-ocr/5/tessdata/eng.traineddata; \
       } > /usr/share/wetube/build-manifest.txt
WORKDIR /opt/wetube
ENV WETUBE_STATE_DIR=/data/state \
    WETUBE_INCOMING_DIR=/data/incoming \
    WETUBE_SCRATCH_DIR=/data/scratch \
    WETUBE_MEDIA_DIR=/data/media \
    WETUBE_STATIC_DIR=/opt/wetube/dist \
    WETUBE_BIND=0.0.0.0:8080 \
    TMPDIR=/tmp \
    DVDCSS_CACHE=off \
    RUST_LOG=info
USER 568:568
RUN wetube-server --version && wetube-worker doctor
EXPOSE 8080
STOPSIGNAL SIGTERM
# Do not use tini -g: the coordinator owns and drains its tool process groups.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint.sh"]
CMD ["server", "serve"]
