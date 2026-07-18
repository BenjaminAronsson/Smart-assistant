---
description: Reconcile docs and ADRs with merged code reality (drift loop)
---
Run the DRIFT LOOP (docs/11 §2): invoke the doc-syncer subagent over merges since the
last sync (or $ARGUMENTS).

Then verify its output: factual corrections applied to docs 02–09; decision-level
changes drafted as Proposed ADRs (not silently edited into prose); traceability table
(docs/01 §6) current; cross-references resolve; any code contradicting an Accepted ADR
surfaced as a BLOCKING finding for the human.

Commit doc corrections as a single `docs: sync` commit listing the reconciled features.
