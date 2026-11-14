#!/bin/sh
cat > /dev/null
(sleep 30 > /dev/null 2>&1 &)
echo '{"decision": "block", "message": "FORKED-AND-BLOCKED"}'
