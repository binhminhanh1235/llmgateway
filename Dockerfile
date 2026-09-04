FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml ./
COPY src ./src
COPY ui ./ui
COPY adapters ./adapters
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/llmgateway /usr/local/bin/llmgateway
WORKDIR /app
EXPOSE 7331
ENTRYPOINT ["llmgateway"]
