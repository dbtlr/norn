---
title: "Architecture decision records"
description: "How the ADR corpus works: numbering, amendment convention, linking conventions, and the reading order for the current architecture."
---

# Architecture decision records

Numbered, append-only records of the hard-to-reverse decisions that shape norn. Each ADR states the decision, the context that forced it, the trade accepted, and the conditions under which it flips. Later reversals never rewrite history: a decision that changes gains a dated `## Amendment — YYYY-MM-DD: …` section naming the ADR that supersedes it, so the reasoning chain stays reconstructible.

This directory is the authoritative home of the corpus. References of the form `[0016](./0016-….md)` link between records; italicized titles refer to internal design notes that are not part of the public corpus.

Reading order for the current architecture: [0018](./0018-greenfield-rewrite-oracle-parity.md) (how the rewrite executes) ← [0017](./0017-registered-vaults-summoned-owners.md) (the registered-vault architecture) ← everything it amends. The project glossary lives at [../glossary.md](../glossary.md).
