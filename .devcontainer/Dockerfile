# Development container for ORBIT: Rust plus the tools the tests use.
# Node, Python, Go, Java and .NET come from devcontainer features.
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends postgresql postgresql-client openssl xvfb \
    && rm -rf /var/lib/apt/lists/*
