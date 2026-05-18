set shell := ["bash", "-cu"]

default:
    @just --list

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

check:
    cargo check

test:
    cargo test

build:
    cargo build -p vault-cli

build-release:
    cargo build -p vault-cli --release

install:
    cargo install --path crates/vault-cli

verify: fmt-check test

run *args:
    cargo run -q -p vault-cli -- {{args}}

fixture-documents root="fixtures/basic":
    cargo run -q -p vault-cli -- -C '{{root}}' docs list --format jsonl

fixture-links root="fixtures/basic":
    cargo run -q -p vault-cli -- -C '{{root}}' links list --format jsonl

fixture-unresolved root="fixtures/basic":
    cargo run -q -p vault-cli -- -C '{{root}}' links unresolved --format json

fixture-diagnostics root="fixtures/basic":
    cargo run -q -p vault-cli -- -C '{{root}}' validate --format jsonl

fixture-backlinks target="beta" root="fixtures/basic":
    cargo run -q -p vault-cli -- -C '{{root}}' links backlinks '{{target}}' --format jsonl

fixture-inspect target="alpha.md" root="fixtures/basic":
    cargo run -q -p vault-cli -- -C '{{root}}' docs inspect '{{target}}' --format json

dist-plan:
    cargo dist plan

dist-build-local:
    cargo dist build

release version:
    sed -i.bak 's/^version = ".*"/version = "{{version}}"/' Cargo.toml && rm Cargo.toml.bak
    cargo check
    git add Cargo.toml Cargo.lock
    git commit -m "Bump workspace version to {{version}}"
    git tag -a v{{version}} -m "vault-cli v{{version}}"

completions:
    mkdir -p target/completions
    cargo run -q -p vault-cli -- completions bash > target/completions/vault.bash
    cargo run -q -p vault-cli -- completions zsh  > target/completions/_vault
    cargo run -q -p vault-cli -- completions fish > target/completions/vault.fish

manpage:
    mkdir -p target/man
    cargo run -q -p vault-cli -- manpage > target/man/vault.1
