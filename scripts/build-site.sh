#!/usr/bin/env sh
# Builds the website (landing page + documentation) into target/site.
# Requires mdBook 0.4: cargo install mdbook --version 0.4.52 --locked
set -eu
cd "$(dirname "$0")/.."
rm -rf target/site
mkdir -p target/site
cp website/index.html website/style.css website/favicon.svg target/site/
mdbook build docs
touch target/site/.nojekyll
echo "site built in target/site (open target/site/index.html)"
