---
description: Start or continue the current milestone loop
---
Model check first: milestone decomposition is judgment work — if the current session is
not on the strongest available model (Fable 5, else Opus), tell the owner to switch with
/model before proceeding.

Run the MILESTONE LOOP (docs/11 §2):

1. Read docs/08 §1 and docs/milestones/ (create the dir if absent). Determine the first
   milestone whose exit evidence is not demonstrated. If a feature list for it exists in
   docs/milestones/M<N>-features.md, continue from the first unchecked feature.
2. If no feature list exists: decompose the milestone into an ordered list of vertical
   features (each sized for one session), with FR/NFR references and dependencies.
   Write it to docs/milestones/M<N>-features.md as a checklist. STOP and present it for
   human approval — do not begin implementation until approved.
3. For each approved feature in order, run the /feature workflow. Check items off in the
   feature list as PRs merge.
4. When all features are checked, tell the human it is time for /gate. Never run the
   gate implicitly.

Constraints: never work on two milestones at once; never pull a future milestone's
feature forward without an approved list change; scope creep is the top project risk.
$ARGUMENTS
