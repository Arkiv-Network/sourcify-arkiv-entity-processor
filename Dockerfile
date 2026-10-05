# Pipeline image: Bun + DuckDB source stage and the arkiv-entity-processor binary. See README.md.
FROM rust:1.90.0-slim-trixie AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM oven/bun:1.4.2-slim@sha256:debbe76858f2e398d2937c1eceeb82c571ac1fcd78aadf00e9634578ac2b5ef7
USER root
RUN apt-get update && apt-get install -y --no-install-recommends bash ca-certificates libstdc++6 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile --production
COPY source ./source
COPY pipeline ./pipeline
COPY --from=builder /build/target/release/arkiv-entity-processor /usr/local/bin/arkiv-entity-processor
ARG REVISION=
ENV APP_DIR=/app APP_REVISION=$REVISION
USER 1000:1000
ENTRYPOINT ["bash", "/app/pipeline/run.sh"]
CMD ["run"]
