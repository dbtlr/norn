---
name: Feature request
about: Suggest a new capability or behavior change for vault-cli
title: "Feature: "
labels: ["enhancement"]
assignees: []
---

## Problem

The user-facing problem you are trying to solve. Describe the workflow you wish you had, not the implementation.

## Proposed solution

The shape of the feature, ideally a sketch of the CLI surface (command name, flags, expected output). For changes to JSON output, show before/after.

```bash
# Example:
vault <new-subcommand> --flag <value>
```

## Alternatives considered

Workarounds, similar features in other tools, or other shapes you considered. Useful context even if you ultimately rejected them.

## Compatibility

- Does this change existing output (JSON, JSONL, table) or existing exit codes?
- Does it require new config in `.vault/config.yaml`?
- Does it intersect with the repair plan schema?

## Additional context

Related issues, prior art, your use case.
