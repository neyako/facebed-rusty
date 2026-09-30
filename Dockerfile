# syntax=docker/dockerfile:1.7

# Built natively per arch (see .github/workflows); no cross-compile target.
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev pkgconfig
WORKDIR /build

# Cache deps separately from src changes.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/deps/facebed* target/release/facebed*

COPY src ./src
COPY assets ./assets
RUN touch src/main.rs && cargo build --release --locked

FROM scratch
WORKDIR /facebed
COPY --from=builder /build/target/release/facebed /facebed/facebed
# Non-root: uid/gid 65532 ("nobody" on most distros).
USER 65532:65532
EXPOSE 9812
ENTRYPOINT ["/facebed/facebed"]
CMD ["-c", "/facebed/config.yaml"]
