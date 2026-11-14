#!/bin/sh
payload=$(cat)
case "$payload" in
  *FORBIDDEN*) echo '{"decision": "block", "message": "REFUSED-BY-HOOK"}' ;;
  *) echo '{"updated_input": "REWRITTEN-BY-HOOK"}' ;;
esac
