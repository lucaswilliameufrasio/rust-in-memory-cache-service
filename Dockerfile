FROM rust:1.86 AS builder
COPY . .
RUN cargo build --release

FROM debian:buster-slim
COPY --from=builder ./target/release/cache-service ./target/release/docker
CMD ["/target/release/docker"]
