# syntax=docker/dockerfile:1
#
# Koma-1: one static binary in an empty image.
#   docker build -t koma .
#   docker run --read-only --cap-drop ALL -p 8080:8080 koma serve

FROM rust:1.97-alpine AS build
WORKDIR /src
COPY . .
# The rust:alpine image ships musl-dev and gcc. On Alpine the native target is
# musl, and Rust links musl statically, so the binary needs nothing at runtime.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release --package koma \
 && cp target/release/koma /koma \
 && mkdir /data

FROM scratch
COPY --from=build /koma /koma
# An empty /data owned by koma's user: a named volume mounted there starts
# with this owner, so `-v koma-data:/data` is writable without a chown.
COPY --from=build --chown=65532:65532 /data /data
# Non-root. 65532 is the conventional "nonroot" id of distroless images.
USER 65532:65532
ENV KOMA_LISTEN=0.0.0.0:8080
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --start-interval=1s \
    CMD ["/koma", "healthcheck"]
ENTRYPOINT ["/koma"]
CMD ["serve"]
