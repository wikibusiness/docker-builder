#!/usr/bin/env bash
set -euo pipefail

: "${IMGBUILD:?path to imgbuild}" "${IMGBUILD_CI_IMAGE:?destination repository}" "${IMGBUILD_CACHE_REPO:?cache repository}"
root=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/imgbuild-export.XXXXXXXX")
cleanup() {
  if [[ -n ${build_pid:-} ]]; then kill "$build_pid" 2>/dev/null || true; fi
  rm -rf "$root"
}
trap cleanup EXIT
mkdir "$root/context"
printf '%s\n' "${root##*/}" > "$root/context/client.txt"
printf '%s\n' "${root##*/}" > "$root/context/map.txt"
source_maps="$IMGBUILD_CI_IMAGE:${root##*/}-maps"
printf '%s\n' 'FROM scratch' 'COPY map.txt /build/main.js.map' > "$root/context/maps.Dockerfile"
printf '%s\n' \
  'FROM scratch AS client' 'COPY client.txt /build/client/main.js' \
  "FROM $source_maps" 'COPY --from=client /build/client /build/client' > "$root/context/Dockerfile"
common=(--context "$root/context" --cache-repo "$IMGBUILD_CACHE_REPO" --platforms linux/amd64)
IMGBUILD_WAIT_FOR_IMAGES=30 "$IMGBUILD" build "${common[@]}" \
  --dockerfile "$root/context/Dockerfile" --work "$root/work" \
  --export "client:/build/client=$root/early" --export "/build=$root/final" \
  --dest "$IMGBUILD_CI_IMAGE:${root##*/}" > "$root/build.log" 2>&1 &
build_pid=$!
for ((i=0; i<100; i++)); do
  if [[ -f "$root/early/main.js" ]]; then break; fi
  sleep 0.1
done
cmp "$root/context/client.txt" "$root/early/main.js"
test ! -e "$root/final"
"$IMGBUILD" build "${common[@]}" --dockerfile "$root/context/maps.Dockerfile" \
  --work "$root/maps-work" --dest "$source_maps"
wait "$build_pid" || { tail -100 "$root/build.log"; exit 1; }
build_pid=
cmp "$root/early/main.js" "$root/final/client/main.js"
cmp "$root/context/map.txt" "$root/final/main.js.map"
