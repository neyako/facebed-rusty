# syntax=docker/dockerfile:1.7

FROM --platform=$BUILDPLATFORM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev pkgconfig
WORKDIR /build

# Cache deps separately from src changes.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
 && cargo build --release \
 && rm -rf src target/release/deps/facebed* target/release/facebed*

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM scratch
WORKDIR /facebed
COPY --from=builder /build/target/release/facebed /facebed/facebed
COPY assets /facebed/assets
# Non-root: uid/gid 65532 ("nobody" on most distros).
USER 65532:65532
EXPOSE 9812
ENTRYPOINT ["/facebed/facebed"]
CMD ["-c", "/facebed/config.yaml"]
