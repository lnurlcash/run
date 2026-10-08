FROM rust:1-trixie AS builder
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release && cp target/release/lnurlcash-run /usr/local/bin/

FROM debian:trixie-slim
COPY --from=builder /usr/local/bin/lnurlcash-run /usr/local/bin/
RUN groupadd --gid 1000 run \
    && useradd --uid 1000 --gid run --no-create-home --shell /usr/sbin/nologin run \
    && mkdir /data && chown run:run /data
USER run
# the beta list: keep this volume
VOLUME /data
ENV DATA_DIR=/data LISTEN=0.0.0.0:8200 PUBLIC_URL=https://run.lnurlcash.com
EXPOSE 8200
ENTRYPOINT ["/usr/local/bin/lnurlcash-run"]
