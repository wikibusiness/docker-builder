#!/usr/bin/env bash
set -euo pipefail

: "${IMGBUILD:?path to imgbuild}" "${IMGBUILD_CI_IMAGE:?destination repository}" "${IMGBUILD_CACHE_REPO:?cache repository}"
root=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/imgbuild-timing.XXXXXXXX")
trap 'rm -rf "$root"' EXIT
mkdir "$root/context"
printf 'FROM scratch\nCOPY first /first\nCOPY second /second\n' > "$root/context/Dockerfile"
printf '%s first\n' "${root##*/}" > "$root/context/first"
printf '%s second\n' "${root##*/}" > "$root/context/second"
export IMGBUILD_LAYER_TIMING=1
"$IMGBUILD" build --dockerfile "$root/context/Dockerfile" --context "$root/context" \
  --work "$root/work" --cache-repo "$IMGBUILD_CACHE_REPO" --platforms linux/arm64 \
  --dest "$IMGBUILD_CI_IMAGE:${root##*/}" 2>&1 | tee "$root/build.log"
awk '/^layer-timing / {sub(/^layer-timing /, ""); print}' "$root/build.log" | jq -es '
  sort_by(.digest) | group_by(.digest) | length == 2 and all(.[];
    sort_by(.start_unix_ms) as $events |
    ($events | length) == 2 and $events[0].op == "build" and $events[0].result == "ok" and
    ($events[1] | (.op == "push" and .result == "ok") or (.op == "reuse" and .result == "mounted")) and
    all(.[]; .start_unix_ms > 0 and .end_unix_ms >= .start_unix_ms) and
    $events[1].start_unix_ms >= $events[0].end_unix_ms)'
