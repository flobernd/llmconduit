# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release

FROM gcr.io/distroless/cc-debian12:nonroot

ENV HOME=/home/nonroot \
    XDG_CONFIG_HOME=/home/nonroot/.config \
    LLMCONDUIT_BIND_ADDR=0.0.0.0:4000 \
    RUST_LOG=info

COPY --from=builder /app/target/release/llmconduit /usr/local/bin/llmconduit

EXPOSE 4000

ENTRYPOINT ["/usr/local/bin/llmconduit"]
CMD ["start"]
