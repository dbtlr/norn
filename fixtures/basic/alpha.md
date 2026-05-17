---
title: Alpha
aliases:
  - First Note
status: draft
---

# Alpha

This links to [[beta|Beta Note]] and [[missing]].

Same-note references should resolve headings [[#Alpha]] and blocks [[#^alpha-block]].

Same-note missing references should report missing anchors [[#Missing Same Heading]] and missing blocks [[#^missing-same-block]].

This also links to a block [[beta#^block-a]].

These references should report missing anchors [[beta#Missing Heading]] and missing blocks [[beta#^missing-block]].

This embeds ![[gamma]] and points at [Delta](folder/delta.md#Delta-Heading).

Attachment embeds should resolve when files exist ![[Assets/diagram.png]].

Markdown image links should also resolve local files ![Picture](Assets/pic.png).

This ambiguous link points at [[duplicate]].

This path-qualified link should ignore case [[Other/Duplicate]].

Markdown links can be encoded [with ext](Markdown%20Target.md), [without ext](Markdown%20Target), and [with anchor](Markdown%20Target.md#Encoded%20Heading).

Inline code should not create a graph edge: `[[inline-example]]`.

```markdown
[[fenced-example]]
```

Local block target. ^alpha-block
