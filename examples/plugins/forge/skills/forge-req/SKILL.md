---
description: Manage forge requirements outside a change - record, list, update status, check spec coverage
argument_hint: <what to do with the requirements>
---
# forge · requirements

Request: $ARGUMENTS

Requirements live in forge; use the `forge` command through RunExt, never files under forge/.

- `forge req list [--status s] [--priority p]`, `forge req get <id>`
- `forge req add` with `input: {"title", "priority", "body"}` or `input: {"items": [ … ]}`
- `forge req update <id> [--status proposed|accepted|rejected|done] [--priority p]`, new text in `input.body`
- `forge spec coverage`: accepted requirements without specs, specs without requirements,
  criteria never verified

Several requirements go in one `forge req add` with `items`. To change many at once, one RunCode program
can loop over `tools.RunExt({command})` when `RunExt(forge req:*)` is allowed; otherwise call RunExt per item.
