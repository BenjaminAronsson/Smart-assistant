---
description: Draft a new Architecture Decision Record for human acceptance
---
Draft an ADR for: $ARGUMENTS

1. Check docs/adr/README.md for an existing ADR covering this — if one exists, this is
   either a supersession (new ADR referencing the old) or not needed.
2. Write the record in context -> decision -> consequences form, numbered next in
   sequence, **status: Proposed**. Consequences must include at least one honest cost
   and a revisit trigger. Reference the requirements/sections it affects.
3. List every doc section that will need updating if accepted (do NOT update them yet).
4. STOP and present to the human. Only a human flips status to Accepted. On acceptance,
   apply the listed doc updates via /sync-docs and reference the ADR in the commit.
