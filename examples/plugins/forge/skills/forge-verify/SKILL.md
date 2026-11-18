---
description: forge stage verify - check every acceptance criterion of a change and record the results
argument_hint: <change id>
---
# forge · verify

Change: $ARGUMENTS

Work only through the `forge` command (RunExt) for forge data; never touch files under forge/.

1. Run the project's tests yourself and keep a short summary of the outcome.
2. Use the Agent tool with `subagent_type: "forge-verifier"`; give it the change id and the test
   summary. It answers with `{"results": [{"spec", "criterion", "pass", "note"}, …]}`.
3. RunExt `forge flow verify <change>` with that object as `input`.
4. If it reports failures, the change is back at apply: fix the code, check the tests, then
   RunExt `forge flow advance <change>` and verify again. When every criterion passed,
   RunExt `forge flow advance <change>` and do what its `say` tells you.
