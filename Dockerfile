# syntax=docker/dockerfile:1
FROM rust:1.93.1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY contracts ./contracts
COPY migrations ./migrations
COPY src ./src
COPY examples ./examples
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release --bin tinkoff-bot && \
    cp target/release/tinkoff-bot /usr/local/bin/tinkoff-bot

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl python3 postgresql-client procps tini && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /usr/local/bin/tinkoff-bot /usr/local/bin/tinkoff-bot
COPY scripts ./scripts
COPY examples/configs ./examples/configs
COPY tests/test_sandbox_*.py ./tests/
COPY docker/entrypoint.py ./docker/entrypoint.py
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 \
    CAMPAIGN_WORK_DIR=/data TINKOFF_BOT_BINARY=/usr/local/bin/tinkoff-bot
EXPOSE 8765
ENTRYPOINT ["/usr/bin/tini", "--", "python3", "/app/docker/entrypoint.py"]
CMD ["python3", "scripts/container-stack.py"]
