FROM rust:1.96-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY release/public-key.hex release/public-key.hex
COPY src/ src/
COPY site/ site/
RUN cargo build --locked --release --no-default-features --features web --bin unbind-web

COPY dist/ /prebuilt/
RUN set -eu; \
    case "$(uname -m)" in x86_64) arch=amd64 ;; aarch64) arch=arm64 ;; *) exit 1 ;; esac; \
    mkdir /downloads; \
    native="unbind-linux-$arch"; \
    if [ ! -f "/prebuilt/$native" ]; then \
        cargo build --locked --release --bin unbind; \
        install -m 0755 target/release/unbind "/downloads/$native"; \
    fi; \
    for name in \
        unbind-linux-amd64 unbind-linux-arm64 \
        unbind-darwin-amd64 unbind-darwin-arm64 \
        unbind-windows-amd64.exe unbind-windows-arm64.exe; do \
        if [ -f "/prebuilt/$name" ]; then install -m 0755 "/prebuilt/$name" "/downloads/$name"; fi; \
    done; \
    cd /downloads; \
    sha256sum unbind-* > SHA256SUMS

FROM debian:bookworm-slim
ENV PORT=8080 \
    UNBIND_SITE_DIR=/srv/unbind/site \
    UNBIND_DOWNLOAD_DIR=/srv/unbind/downloads
COPY --from=build /build/target/release/unbind-web /usr/local/bin/unbind-web
COPY --from=build /downloads/ /srv/unbind/downloads/
COPY site/ /srv/unbind/site/
EXPOSE 8080
USER 10001:10001
ENTRYPOINT ["unbind-web"]
