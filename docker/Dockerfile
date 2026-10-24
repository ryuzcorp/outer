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
# Pinned formatter for agent commits. Keep in step with the template's
# `oxfmt` devDependency, `.github/workflows/ci.yml`, and the extension list
# in `crates/outer/src/git/format.rs`.
ARG OXFMT_VERSION=0.74.0

FROM ghcr.io/denoland/celld:${CELLD_VERSION} AS celld

# --- web site (Blume) -----------------------------------------------------------
# The platform host serves this static build for humans and agents; the MCP
# `docs` tool serves the same Markdown from `web/docs/guides/`. Installed with
# bun: npm's resolver stalls on Astro's dependency tree (250k+ placements,
# no lockfile after hours), while `bun install` finishes in seconds.
FROM docker.io/oven/bun:1.4-slim AS site
WORKDIR /site/web
COPY web/package.json web/bun.lock web/blume.config.ts ./
COPY web/docs ./docs
RUN bun install --frozen-lockfile \
 && bun ./node_modules/blume/bin/blume.mjs build

# --- oxfmt (agent-commit formatter) ------------------------------------------
# oxfmt ships as a JS CLI plus a native binding, not a standalone binary:
# it runs under the runtime image's bun. The whole install dir (package +
# platform binding) is copied; `/usr/local/bin/oxfmt` below is a shim.
FROM docker.io/oven/bun:1.4-slim AS oxfmt
ARG OXFMT_VERSION
WORKDIR /oxfmt
RUN bun add --exact oxfmt@${OXFMT_VERSION} \
 && bun /oxfmt/node_modules/oxfmt/dist/cli.js --version

# --- outer binary -------------------------------------------------------------
FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# The whole crates/ tree: sources pull files in with include_str!/sqlx
# migrate! (web/docs, migrations), and listing subdirectories here broke the
# image twice when a new one appeared. `web/docs` rides along too: the MCP
# `docs` tool and `outer docs tools` embed it with include_str!.
COPY crates ./crates
COPY web ./web
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
# The built web site, served on the platform host from OUTER_SITE_DIR.
COPY --from=site /site/web/dist /usr/share/outer/site
# Pinned formatter for agent commits (`OUTER_OXFMT_BIN`, default
# `/usr/local/bin/oxfmt`). Absolute bun path: commits run oxfmt with a
# cleared environment, so no PATH lookup.
COPY --from=oxfmt /oxfmt /opt/oxfmt
RUN printf '#!/bin/sh\nexec /usr/local/bin/bun /opt/oxfmt/node_modules/oxfmt/dist/cli.js "$@"\n' > /usr/local/bin/oxfmt \
 && chmod 0755 /usr/local/bin/oxfmt \
 && oxfmt --version
# Apache-2.0 §4(a) and §4(d): the image carries the License and the NOTICE.
COPY LICENSE NOTICE /licenses/
WORKDIR /data
ENV OUTER_DATA=/data \
    OUTER_LISTEN=[::]:443 \
    OUTER_SITE_DIR=/usr/share/outer/site \
    CELLD_ESBUILD=/usr/local/bin/esbuild \
    CELLD_VERSION="${CELLD_VERSION}"
VOLUME /data
EXPOSE 443
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/outer"]
CMD ["serve"]
