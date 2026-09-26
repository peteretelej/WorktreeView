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
# The whole migrations directory: sqlx::migrate! embeds every .sql it
# finds, so a build must never pin an explicit migration file list.
COPY src-tauri/migrations/ ./src-tauri/migrations/
# The whole skills directory: SKILL_BUNDLE embeds every file home.rs names,
# and the drift test fails when the folder and the bundle disagree.
COPY skills/ ./skills/
COPY src-tauri/src/ ./src-tauri/src/
COPY e2e ./e2e

# The suite runs the container as the host's numeric uid, which no passwd
# entry names, and ssh refuses to run for such a user. This NSS service
# synthesizes that one missing entry (and the matching "e2e" login name the
# fixture authenticates as); it sits after `files` in nsswitch.conf, so real
# entries always win. Compiled here, shipped without any compiler.
COPY <<EOF /tmp/fixuid.c
#define _GNU_SOURCE
#include <nss.h>
#include <pwd.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
static const char NAME[] = "e2e";
static const char HOME[] = "/tmp/worktreeview-e2e-home";
static const char SHELL[] = "/bin/sh";
static enum nss_status fill(struct passwd *pw, char *buf, size_t buflen, int *errnop) {
    size_t need = sizeof(NAME) + sizeof("x") + sizeof("worktreeview e2e runtime user") + sizeof(HOME) + sizeof(SHELL);
    if (buflen < need) { *errnop = ERANGE; return NSS_STATUS_TRYAGAIN; }
    char *cursor = buf;
    pw->pw_uid = geteuid();
    pw->pw_gid = getegid();
#define PLACE(field, value) do { memcpy(cursor, value, sizeof(value)); (field) = cursor; cursor += sizeof(value); } while (0)
    PLACE(pw->pw_name, NAME);
    PLACE(pw->pw_passwd, "x");
    PLACE(pw->pw_gecos, "worktreeview e2e runtime user");
    PLACE(pw->pw_dir, HOME);
    PLACE(pw->pw_shell, SHELL);
#undef PLACE
    *errnop = 0;
    return NSS_STATUS_SUCCESS;
}
enum nss_status _nss_fixuid_getpwuid_r(uid_t uid, struct passwd *pw, char *buf, size_t buflen, int *errnop) { (void)uid; return fill(pw, buf, buflen, errnop); }
enum nss_status _nss_fixuid_getpwnam_r(const char *name, struct passwd *pw, char *buf, size_t buflen, int *errnop) {
    if (strcmp(name, NAME) != 0) { *errnop = 0; return NSS_STATUS_NOTFOUND; }
    return fill(pw, buf, buflen, errnop);
}
EOF
RUN gcc -shared -fPIC -O2 -Wall -o /tmp/libnss_fixuid.so.2 /tmp/fixuid.c

RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    cargo install tauri-driver --version 2.0.6 --locked --root /tmp/tauri-driver

RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/app/src-tauri/target \
    VITE_E2E_PICKER_PATH=/tmp/worktreeview-e2e-selection \
    npm run tauri -- build --no-bundle \
      --config '{"build":{"beforeBuildCommand":"npm run build:e2e"}}' \
  && cp /app/src-tauri/target/release/worktreeview /tmp/worktreeview \
  && cp /app/src-tauri/target/release/worktreeview-server /tmp/worktreeview-server

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    dbus \
    git \
    libwebkit2gtk-4.1-0 \
    openssh-client \
    openssh-server \
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
COPY --from=build /tmp/libnss_fixuid.so.2 /usr/lib/x86_64-linux-gnu/libnss_fixuid.so.2
COPY --from=build /tmp/worktreeview-server /app/worktreeview-server

RUN sed -E -i 's/^(passwd:.*)$/\1 fixuid/' /etc/nsswitch.conf \
  && for command in cargo rustc cc gcc g++ make; do \
      if command -v "$command"; then exit 1; fi; \
    done \
  && ! dpkg-query -W -f='${Package}\n' | grep -Eq -- '-dev(:[^ ]+)?$' \
  && test ! -e /usr/local/cargo/registry \
  && test ! -e /usr/local/cargo/git \
  && test ! -e /app/src-tauri/target

ENV HOME=/tmp/worktreeview-e2e-home \
    XDG_DATA_HOME=/tmp/worktreeview-e2e-data \
    WORKTREEVIEW_DATA_DIR=/tmp/worktreeview-e2e-data \
    XDG_RUNTIME_DIR=/tmp/worktreeview-e2e-runtime \
    GDK_BACKEND=x11 \
    LIBGL_ALWAYS_SOFTWARE=1 \
    WEBKIT_DISABLE_DMABUF_RENDERER=1 \
    GIT_CONFIG_NOSYSTEM=1 \
    GIT_CONFIG_GLOBAL=/dev/null

CMD ["sh", "-c", "mkdir -m 700 \"$HOME\" \"$XDG_RUNTIME_DIR\" && exec dbus-run-session -- xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' node node_modules/@wdio/cli/bin/wdio.js run e2e/wdio.conf.mjs"]
