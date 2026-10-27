# ---- build ----
FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake pkg-config build-essential && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

# ---- run ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 cookie && mkdir -p /data/reports && chown -R cookie /data
COPY --from=build /src/target/release/cookie-web /usr/local/bin/cookie-web
USER cookie
WORKDIR /data
ENV COOKIE_HOST=0.0.0.0 COOKIE_PORT=8765 COOKIE_REPORTS_DIR=/data/reports
EXPOSE 8765
ENTRYPOINT ["cookie-web"]
CMD ["serve"]
