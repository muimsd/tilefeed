FROM rust:1.82-bookworm AS builder

# Install protobuf compiler (needed by prost-build)
RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    tippecanoe \
    awscli \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/tilefeed /usr/local/bin/tilefeed

WORKDIR /data
ENTRYPOINT ["tilefeed"]
CMD ["run"]
