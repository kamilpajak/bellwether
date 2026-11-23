# newsvault and modelgate as one OCI image (S7-D10, option C): docs/design/architecture.md section
# 11.5 step 7; modelgate is run with `--entrypoint /usr/bin/tini` (deploy/systemd/modelgate-shadow).
# Both stages are Ubuntu 24.04, whose system SQLite (3.45) is the one CI's runner links: no
# `bundled` SQLite before step 8. The tag is dated and the digest pins it.
ARG UBUNTU=ubuntu:noble-20261024.1@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852

FROM ${UBUNTU} AS build
ARG RUSTUP_VERSION=1.29.3
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev libsqlite3-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*
# Checksums from static.rust-lang.org/rustup/archive/1.29.3/<target>/rustup-init.sha256.
RUN set -eu; \
    target="$(uname -m)-unknown-linux-gnu"; \
    case "$target" in \
      x86_64-unknown-linux-gnu) sum=b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55 ;; \
      aarch64-unknown-linux-gnu) sum=9f608c142e0e38a78957d0661ba1dfaf0b6fb37184cfaa4b9a0aa57af6184282 ;; \
      *) echo "no rustup-init checksum for $target" >&2; exit 1 ;; \
    esac; \
    curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/rustup-init \
      "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${target}/rustup-init"; \
    echo "$sum  /tmp/rustup-init" | sha256sum -c -; \
    chmod +x /tmp/rustup-init; \
    /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain none; \
    rm /tmp/rustup-init
ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
# The toolchain ci.yml installs, from its one home; its own layer, so a source change reuses it.
COPY rust-toolchain.toml .
RUN rustup toolchain install
COPY . .
RUN cargo build --locked --release -p newsvault -p modelgate

FROM ${UBUNTU}
RUN apt-get update \
 && apt-get install -y --no-install-recommends libsqlite3-0 tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin newsvault
COPY --from=build /src/target/release/newsvault /src/target/release/modelgate /usr/local/bin/
COPY providers/open-web-feed/hosts/open_web_wires.hosts.json \
     providers/open-web-feed/rules/open-web-feed.rules.json \
     /usr/share/bellwether/
COPY services/modelgate/questions/ripple-shadow-1.json /usr/share/bellwether/questions/
ENV BELLWETHER_VAULT=/vault
USER 10001
# tini as PID 1, so a SIGTERM from `docker stop` or the kubelet reaches newsvault.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/newsvault"]
CMD ["schema"]
