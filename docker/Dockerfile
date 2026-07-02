# Reproducible Linux environment for building and exercising rdbg.
FROM rust:1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends lldb python3 \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add rust-analyzer rust-src

WORKDIR /src
COPY . .
RUN cargo build --release && install -m755 target/release/rdbg /usr/local/bin/rdbg

CMD ["bash"]
