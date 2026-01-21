FROM rust:1.92.0-bullseye AS builder

WORKDIR /build

RUN rustup target add x86_64-unknown-linux-musl
RUN rustup target add x86_64-unknown-linux-gnu

RUN apt update -y && \
  apt install -y musl-tools clang llvm

COPY . .

RUN cargo build --release --target=x86_64-unknown-linux-gnu
RUN cargo build --release --target=x86_64-unknown-linux-musl

FROM debian:bullseye-slim

RUN apt update -y && \
  apt install -y --no-install-recommends wget

RUN apt clean
RUN rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/sessiondriver /usr/local/bin/sessiondriver-musl
COPY --from=builder /build/target/x86_64-unknown-linux-gnu/release/sessiondriver /usr/local/bin/sessiondriver-gnu

HEALTHCHECK --interval=15s --timeout=10s --retries=3 CMD wget --spider -q http://127.0.0.1:4444/status || exit 1

EXPOSE 4444

ENTRYPOINT ["/usr/local/bin/sessiondriver-gnu"]

# # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # #
#                                                                       #
#     docker build -t goslar/sessiondriver:latest -f ./Dockerfile .     #
#                                                                       #
# # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # # #