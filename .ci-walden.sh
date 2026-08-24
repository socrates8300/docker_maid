#!/usr/bin/env bash
echo "== ci-walden tenant toolchain probe =="
for t in bash git go node npm pnpm yarn cargo rustc make python3 docker; do
  if command -v "$t" >/dev/null 2>&1; then
    echo "PROBE $t: $(command -v $t) $(${t} --version 2>&1 | head -1)"
  else
    echo "PROBE $t: MISSING"
  fi
done
echo "JUDGE-P10-PROBE"
exit 0
