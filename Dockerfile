# Guest sandbox image for agent_web.
#
# A locked-down container to hand out to guests: the app runs inside, sees only a
# mounted /workspace and /chats, and gets its provider key from the environment
# — never the host's real .env. Combined with the built-in access gate
# (CWI_AUTH=1, baked in below) and the run flags in run-guest.ps1
# (--read-only --cap-drop ALL --security-opt no-new-privileges), a guest can use
# the agent without reaching the host.
#
# Build:  docker build -t agent-web:guest .
# Run:    see run-guest.ps1

# ---- build: static musl binary (rust:alpine targets musl by default) ----
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release

# ---- runtime: minimal Alpine, non-root, only the binary + frontend ----
FROM alpine:3.20
RUN adduser -D -u 10001 agent \
    && mkdir -p /chats /workspace \
    && chown -R agent:agent /chats /workspace
WORKDIR /app
COPY --from=build /src/target/release/agent_web ./agent_web
COPY static ./static
USER agent
# Guests always authenticate (access gate on); native engine; bind on all
# interfaces inside the container (published only to host loopback by the run
# script). Storage + workspace are mount points, provided at run time.
ENV CWI_NO_MENU=1 \
    CWI_ENGINE=native \
    CWI_AUTH=1 \
    CWI_BIND=0.0.0.0:8787 \
    CWI_WORKSPACE=/workspace \
    CLAUDE_CONFIG_DIR=/chats
EXPOSE 8787
ENTRYPOINT ["/app/agent_web"]
