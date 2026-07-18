---
title: "0014 — Atomic cache publication over staged writer chunks"
description: "Bulk chunks prepare cache state privately; one terminal transaction publishes the relational projection, and each structured request stays on one read snapshot."
---

# 0014 — Atomic cache publication over staged writer chunks

ADR 0013 correctly chose generational request binding, WAL readers, and a two-class writer queue, but its claim that visible per-file cache commits are honest was false for globally resolved relations. A file chunk can expose a new document while unchanged link rows still encode the old graph.

Bulk writer chunks therefore prepare job-scoped state in private TEMP staging tables and publish the complete relational projection in one terminal main-database transaction. Structured tool calls hold one SQLite read snapshot for their full multi-statement lifetime. Chunk boundaries remain liveness-preemption seams but are never visibility seams.

Exact Markdown remains source-layer state. `.raw` is removed; `get --format markdown` and its MCP equivalent read one source file directly and do not participate in the relational snapshot contract.

Whole-increment transactions were rejected because they discard liveness preemption. Shadow databases and row epochs were rejected as materially larger generation/schema models. The accepted cost is a staging schema, explicit read-transaction cleanup, and an indivisible terminal publish. A release-mode probe against a real-world vault (1,513 documents) measured about 15 ms for the normal one-document case and 138 ms for an artificial all-document publication: typical work stays inside the 50 ms chunk target, while vault-wide mutations can exceed it. That excess is by design, not a violation of ADR 0013's budget: the 50 ms target governs *chunk-boundary preemption* between bulk chunks, while the terminal publish is the indivisible visibility point — preempting it would give up atomic publication. The control-plane writer sequence advances at terminal completion, so an active publish reads as busy and only one stalled beyond the service stall budget reads as hung (ADR 0013's contract, unchanged). An O(1) terminal switch would require a persistent double-buffered cache layout.
