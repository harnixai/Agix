---
description: forge stage apply - implement a change's plan step by step, checking each item off
argument_hint: <change id>
---
# forge · apply

Change: $ARGUMENTS

Work only through the `forge` command (RunExt) for forge data; never touch files under forge/.

1. RunExt `forge flow show <change>` for the plan and the criteria in its `delta`.
2. For each plan item in order: write the code, then RunExt `forge flow check <change> <P#>`
   (several done at once: `forge flow check <change> P1 P2 P3`).
3. Build and run the tests until they pass; every criterion should be shown by a test.
4. RunExt `forge flow advance <change>` and do what its `say` tells you.
