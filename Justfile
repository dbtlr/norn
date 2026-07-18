set shell := ["bash", "-cu"]

default:
    @just --list

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

check:
    cargo check --workspace --locked

test:
    cargo test --workspace

build:
    cargo build --workspace

build-release:
    cargo build --workspace --release

install:
    @echo "no bin crate yet (ADR 0018 rewrite): the norn binary returns with the workspace skeleton + ported surfaces; use the pinned oracle release meanwhile" && exit 1

verify: fmt-check test

run *args:
    @echo "no bin crate yet (ADR 0018 rewrite): the norn binary returns with the workspace skeleton + ported surfaces; use the pinned oracle release meanwhile" && exit 1

dist-plan:
    @echo "no bin crate yet (ADR 0018 rewrite): the norn binary returns with the workspace skeleton + ported surfaces; use the pinned oracle release meanwhile" && exit 1

dist-build-local:
    cargo dist build

release version:
    sed -i.bak 's/^version = ".*"/version = "{{version}}"/' Cargo.toml && rm Cargo.toml.bak
    cargo check
    git add Cargo.toml Cargo.lock
    git commit -m "Bump workspace version to {{version}}"
    git tag -a v{{version}} -m "norn v{{version}}"

completions:
    @echo "no bin crate yet (ADR 0018 rewrite): the norn binary returns with the workspace skeleton + ported surfaces; use the pinned oracle release meanwhile" && exit 1

manpage:
    @echo "no bin crate yet (ADR 0018 rewrite): the norn binary returns with the workspace skeleton + ported surfaces; use the pinned oracle release meanwhile" && exit 1
