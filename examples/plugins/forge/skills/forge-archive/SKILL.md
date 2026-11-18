---
description: forge stage archive - merge a verified change into the current specs and close it
argument_hint: <change id>
---
# forge · archive

Change: $ARGUMENTS

1. RunExt `forge flow archive <change>`. forge merges the delta into the current specs, marks the
   change's requirements done and moves the change to forge/archive/.
2. If it refuses because entries changed after the delta was written (another change was archived
   first), RunExt `forge spec get <id>` for each, send the delta again with `forge flow delta
   <change>` written against the current entries, and do what its `say` tells you (verify again
   with the forge-verify skill, then advance and archive).
3. Tell the user in two or three lines what was built and which spec entries are now current.
