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

release:
    cargo build -p vault-cli --release

install:
    cargo install --path crates/vault-cli

verify: fmt-check test

run *args:
    cargo run -q -p vault-cli -- {{args}}

fixture-documents root="fixtures/basic":
    cargo run -q -p vault-cli -- graph documents --root '{{root}}' --format jsonl

fixture-links root="fixtures/basic":
    cargo run -q -p vault-cli -- graph links --root '{{root}}' --format jsonl

fixture-unresolved root="fixtures/basic":
    cargo run -q -p vault-cli -- graph unresolved --root '{{root}}' --format json

fixture-diagnostics root="fixtures/basic":
    cargo run -q -p vault-cli -- graph diagnostics --root '{{root}}' --format jsonl

fixture-backlinks target="beta" root="fixtures/basic":
    cargo run -q -p vault-cli -- graph backlinks '{{target}}' --root '{{root}}' --format jsonl

fixture-inspect target="alpha.md" root="fixtures/basic":
    cargo run -q -p vault-cli -- graph inspect '{{target}}' --root '{{root}}' --format json

fixture-build-cache cache="/tmp/vault-cli-cache" root="fixtures/basic":
    cargo run -q -p vault-cli -- graph build --root '{{root}}' --cache '{{cache}}' --format json
