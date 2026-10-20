# Immutable multi-architecture base manifests; apt archives frozen to 2026-10-20.
FROM rust:1.95.0-slim-bookworm@sha256:d7482085ff5b415f84dba5647ae71606650bdef00db7aeb69f4b3d170c3e4082 AS builder
RUN rm -f /etc/apt/sources.list.d/debian.sources && printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20261020T000000Z bookworm main\n' > /etc/apt/sources.list \
 && apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps ./apps
RUN cargo build --locked --release -p remotevnc-backend -j 1

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
RUN rm -f /etc/apt/sources.list.d/debian.sources && printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20261020T000000Z bookworm main\n' > /etc/apt/sources.list \
 && apt-get update && apt-get install -y --no-install-recommends ca-certificates python3 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 10001 remotevnc && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin remotevnc \
 && install -d -m0700 -o 10001 -g 10001 /data
COPY --from=builder /build/target/release/remotevnc-backend /usr/local/bin/remotevnc-backend
COPY packaging/backend/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY packaging/backend/maintenance.py /usr/local/bin/maintenance.py
USER 10001:10001
ENV REMOTEVNC_DATABASE=/data/backend.db REMOTEVNC_BIND=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/bin/sh", "/usr/local/bin/entrypoint.sh"]
