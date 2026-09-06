# syntax=docker/dockerfile:1

FROM node:24-bookworm@sha256:be23f54a88d34e8824c741b19b91064094f92c1c97b194144bfc8b50d67258e2 AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    curl \
    file \
    libappindicator3-dev \
    librsvg2-dev \
    libssl-dev \
    libwebkit2gtk-4.1-dev \
    libxdo-dev \
    wget \
  && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
ENV RUSTUP_TOOLCHAIN=stable-x86_64-unknown-linux-gnu

WORKDIR /app
COPY .npmrc package.json package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci

COPY rust-toolchain.toml tsconfig.json tsconfig.node.json index.html vite.config.ts ./
COPY src ./src
COPY src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/build.rs src-tauri/tauri.conf.json ./src-tauri/
COPY src-tauri/capabilities ./src-tauri/capabilities
COPY src-tauri/icons ./src-tauri/icons
COPY src-tauri/migrations/0001_init.sql ./src-tauri/migrations/0001_init.sql
COPY src-tauri/migrations/0002_retrospection.sql ./src-tauri/migrations/0002_retrospection.sql
COPY src-tauri/src/ ./src-tauri/src/
COPY e2e ./e2e

RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    cargo install tauri-driver --version 2.0.6 --locked --root /tmp/tauri-driver

RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/app/src-tauri/target \
    VITE_E2E_PICKER_PATH=/tmp/worktreeview-e2e-selection \
    npm run tauri -- build --no-bundle \
      --config '{"build":{"beforeBuildCommand":"npm run build:e2e"}}' \
  && cp /app/src-tauri/target/release/worktreeview /tmp/worktreeview

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    dbus \
    git \
    libwebkit2gtk-4.1-0 \
    scrot \
    webkit2gtk-driver \
    xauth \
    xvfb \
  && rm -rf /var/lib/apt/lists/*

COPY --from=build /usr/local/bin/node /usr/local/bin/node
WORKDIR /app
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/e2e ./e2e
COPY --from=build /tmp/tauri-driver/bin/tauri-driver /usr/local/bin/tauri-driver
COPY --from=build /tmp/worktreeview /app/worktreeview

RUN for command in cargo rustc cc gcc g++ make; do \
      if command -v "$command"; then exit 1; fi; \
    done \
  && ! dpkg-query -W -f='${Package}\n' | grep -Eq -- '-dev(:[^ ]+)?$' \
  && test ! -e /usr/local/cargo/registry \
  && test ! -e /usr/local/cargo/git \
  && test ! -e /app/src-tauri/target

ENV HOME=/tmp/worktreeview-e2e-home \
    XDG_DATA_HOME=/tmp/worktreeview-e2e-data \
    XDG_RUNTIME_DIR=/tmp/worktreeview-e2e-runtime \
    GDK_BACKEND=x11 \
    LIBGL_ALWAYS_SOFTWARE=1 \
    WEBKIT_DISABLE_DMABUF_RENDERER=1 \
    GIT_CONFIG_NOSYSTEM=1 \
    GIT_CONFIG_GLOBAL=/dev/null

CMD ["sh", "-c", "mkdir -m 700 \"$HOME\" \"$XDG_RUNTIME_DIR\" && exec dbus-run-session -- xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' node node_modules/@wdio/cli/bin/wdio.js run e2e/wdio.conf.mjs"]
