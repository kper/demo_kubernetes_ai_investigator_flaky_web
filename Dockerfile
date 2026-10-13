FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /src/target/release/flaky-web /usr/local/bin/flaky-web
USER 65534
EXPOSE 8080
CMD ["flaky-web"]
