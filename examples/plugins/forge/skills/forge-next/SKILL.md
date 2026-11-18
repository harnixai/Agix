---
description: Continue a forge change - find its current stage and run that stage's skill
argument_hint: <change id>
---
# forge · next

Change: $ARGUMENTS

1. RunExt `forge flow next <change>` (without a change id: `forge flow list` first and pick the one
   that is not done; ask the user if several are open).
2. It names the stage the change is at and its `skill`. The user asked to continue, so call the
   Skill tool with that skill and the change id as `args`, whatever `continue` says, and do the
   stage.
