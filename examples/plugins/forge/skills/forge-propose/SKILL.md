---
description: Start a forge change - agree on the requirement, record it, open a change (stage propose)
argument_hint: <what should be built>
---
# forge · propose

Request: $ARGUMENTS

This project keeps requirements, specs and changes in forge. You read and write them only with the
`forge` command through the RunExt tool; never touch files under forge/. `forge help` prints usage.

1. Make the requirement clear: what is wanted, why, what is out of scope, how we will know it
   works. Ask the user only about what you cannot reasonably decide; otherwise state your
   assumptions and go on.
2. Record it: RunExt `forge req add` with `input: {"title": …, "priority": "high|medium|low",
   "body": …}` (several at once: `input: {"items": [ … ]}`). It answers with the ids.
3. Open the change: RunExt `forge flow new` with `input: {"title": …, "reqs": ["R-…"], "body":
   "why, scope, assumptions"}`. It answers with the change id.
4. RunExt `forge flow advance <change id>` and do what its `say` tells you.
