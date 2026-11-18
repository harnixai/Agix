---
description: forge stage spec - write the spec entries a change adds, modifies or removes, with acceptance criteria
argument_hint: <change id>
---
# forge · spec

Change: $ARGUMENTS

Work only through the `forge` command (RunExt); never touch files under forge/.

1. RunExt `forge flow show <change>`, `forge req get <id>` for its requirements, and
   `forge spec list` for the current specs (`forge spec get <id>` for one you may modify).
2. Write the delta with RunExt `forge flow delta <change>` and
   `input: {"add": [{"capability": "<lowercase-word>", "title": …, "body": …, "criteria": [ … ]}],
   "modify": [{"id": "S-…", …changed fields}], "remove": ["S-…"]}`.
   One entry per behaviour the user can observe. Each criterion is one concrete, checkable
   statement (an input and the output or effect it must give) that a test can show. Sending the
   delta again replaces it; forge assigns the ids.
3. RunExt `forge flow advance <change>` and do what its `say` tells you.
