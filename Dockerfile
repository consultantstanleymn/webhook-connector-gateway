# Build stage
FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake build-essential \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin gateway --bin mock-downstream

# Runtime stage: no shell tools beyond the base, non-root user
FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home /data gateway && mkdir /data && chown gateway /data
COPY --from=build /src/target/release/gateway /usr/local/bin/gateway
COPY --from=build /src/target/release/mock-downstream /usr/local/bin/mock-downstream
USER gateway
WORKDIR /data
ENV GATEWAY_CONFIG=/etc/gateway/gateway.toml
EXPOSE 8080
ENTRYPOINT ["gateway"]
CMD ["serve"]
