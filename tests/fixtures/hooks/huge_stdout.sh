#!/bin/sh
cat > /dev/null
head -c 67108864 /dev/zero | tr '\0' 'x'
