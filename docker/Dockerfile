# syntax=docker/dockerfile:1
# The Outer image: one container on the VPS. `outer` is PID 1's only child
# (under tini) and owns every other process:
#
#   outer (uid 0)       MCP, OAuth, app proxy, deploys, supervision
#   ├── celld × N       one fleet per awake app, each as the app's own uid
#   └── bun / sh        builds, as the app's uid
#
# outer terminates TLS itself (rustls) on OUTER_LISTEN: the Cloudflare
# Origin CA certificate plus Authenticated Origin Pulls (mTLS).
ARG CELLD_VERSION=0.6.2
ARG ESBUILD_VERSION=0.28.2

FROM ghcr.io/denoland/celld:${CELLD_VERSION} AS celld

# --- outer binary -------------------------------------------------------------
FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# The whole crates/ tree: sources pull files in with include_str!/sqlx
# migrate! (docs, migrations), and listing subdirectories here broke the
# image twice when a new one appeared.
COPY crates ./crates
RUN --mount=type=cache,id=outer-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=outer-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=outer-target,target=/src/target,sharing=locked \
    cargo build --release --locked \
  && install -D -m 0755 target/release/outer /out/outer \
  && strip /out/outer

# --- runtime ------------------------------------------------------------------
# bun runs app installs and builds. git backs each app's repo. nftables
# enforces the egress policy. esbuild is what celld bundles Worker code with.
FROM docker.io/oven/bun:1.4-slim AS outer
ARG ESBUILD_VERSION
ARG CELLD_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends \
      tini ca-certificates curl git nftables \
  && rm -rf /var/lib/apt/lists/* \
  && arch="$(dpkg --print-architecture)" \
  && case "${arch}" in \
       amd64) ESBUILD_PKG=linux-x64 ;; \
       arm64) ESBUILD_PKG=linux-arm64 ;; \
       *) echo "unsupported arch: ${arch}" >&2; exit 1 ;; \
     esac \
  && curl -fsSL --retry 5 "https://registry.npmjs.org/@esbuild/${ESBUILD_PKG}/-/${ESBUILD_PKG}-${ESBUILD_VERSION}.tgz" \
    | tar -xz -C /tmp \
  && install -m 0755 /tmp/package/bin/esbuild /usr/local/bin/esbuild \
  && rm -rf /tmp/package \
  && esbuild --version
# App uids: each app runs (and builds) as its own uid from this range. setuid
# needs no passwd entry, but bun and Node's os.userInfo() throw for a uid
# without one, so the range is pre-created. Keep it in step with config.rs.
RUN for uid in $(seq 200000 201023); do \
      echo "app-${uid}:x:${uid}:${uid}::/nonexistent:/usr/sbin/nologin" >> /etc/passwd; \
      echo "app-${uid}:x:${uid}:" >> /etc/group; \
    done \
  && mkdir -p /data
COPY --from=celld /usr/local/bin/celld /usr/local/bin/celld
COPY --from=build /out/outer /usr/local/bin/outer
# Apache-2.0 §4(a) and §4(d): the image carries the License and the NOTICE.
COPY LICENSE NOTICE /licenses/
WORKDIR /data
ENV OUTER_DATA=/data \
    OUTER_LISTEN=[::]:443 \
    CELLD_ESBUILD=/usr/local/bin/esbuild \
    CELLD_VERSION="${CELLD_VERSION}"
VOLUME /data
EXPOSE 443
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/outer"]
CMD ["serve"]
