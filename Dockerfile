# ORBIT runtime image.
#   docker build -t orbit .
#   docker run -p 7233:7233 -e ORBIT_TOKEN=change-me -v orbit-data:/data orbit
# The server refuses to listen on 0.0.0.0 without ORBIT_TOKEN (or --insecure).
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p orbit-lang && strip target/release/orbit

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data orbit && mkdir -p /data && chown orbit /data
COPY --from=build /src/target/release/orbit /usr/local/bin/orbit
USER orbit
WORKDIR /data
VOLUME ["/data"]
EXPOSE 7233 7234
ENV ORBIT_DB=/data/orbit.db ORBIT_ADDR=0.0.0.0:7233
ENTRYPOINT ["orbit"]
CMD ["server"]
