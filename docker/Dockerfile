# Runtime image for the buildkit-controller binary.
# COPY-only: `cargo xtask image` compiles the binary beforehand (see
# xtask/src/image.rs) and passes it in the build context, so nothing here
# needs the network beyond the base image. The builder image is pinned in
# docker/builder-image; bump the two digests together.
# gcr.io/distroless/cc-debian13:nonroot (Debian 13, glibc 2.41, same as the rust trixie builder)
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:b7e81cb1af38d9345680e68bcf564d388f96304e25222030fc8c051faf8565b3
COPY --chown=65532:65532 buildkit-controller /usr/local/bin/buildkit-controller
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/buildkit-controller"]
