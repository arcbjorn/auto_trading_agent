# The web demo in a container, for a machine without a Rust toolchain. `make demo-web` on the host
# is the primary path (ADR-18); this is the fallback, and it serves the same page.
#
#   docker build -t auto-trading-agent .
#   docker run --rm -p 8080:8080 --env-file .env auto-trading-agent
#
# Build stage: the pinned toolchain compiles the workspace with the locked dependency set. protoc
# is vendored by the build script, so no system package is needed. Dependencies are compiled in a
# layer of their own, keyed on the manifests, so a source edit rebuilds only the crates.
FROM rust:1.88-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto
RUN cargo build --release --locked -p evals

# Runtime stage: the evals binary, the scenario files it grades against and the recorded reports
# it renders. CA certificates are needed for the model APIs over TLS (rustls-native-certs).
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /app --shell /usr/sbin/nologin demo
WORKDIR /app
COPY --from=build /src/target/release/evals /usr/local/bin/evals
COPY evals/cases ./evals/cases
COPY docs/results ./docs/results
RUN mkdir -p evals/out && chown -R demo /app
USER demo
EXPOSE 8080
# Inside the container the page must listen on every interface, or the published port has nothing
# behind it; the browser still reaches it as localhost, which is what the request boundary checks.
ENTRYPOINT ["evals", "web", "--addr", "0.0.0.0:8080"]
