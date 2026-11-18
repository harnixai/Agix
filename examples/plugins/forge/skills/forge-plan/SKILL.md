---
description: forge stage plan - break a change into ordered implementation steps
argument_hint: <change id>
---
# forge · plan

Change: $ARGUMENTS

Work only through the `forge` command (RunExt); never touch files under forge/.

1. RunExt `forge flow show <change>`: its `delta` holds the spec entries and their criteria.
2. Look at the project as far as you need to decide how to build it.
3. RunExt `forge flow plan <change>` with `input: {"items": ["step", …], "design": "optional notes"}`.
   A handful of steps, each a piece of work you can finish and check off; include writing the
   tests that show the criteria.
4. RunExt `forge flow advance <change>` and do what its `say` tells you.
