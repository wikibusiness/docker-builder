#!/usr/bin/env bash
set -euo pipefail

: "${IMGBUILD:?path to imgbuild}" "${IMGBUILD_CI_IMAGE:?destination repository}" "${IMGBUILD_CACHE_REPO:?cache repository}"
root=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/imgbuild-cache-scope.XXXXXXXX")
context="$root/context"
images=()
cleanup() {
  if [ "${#images[@]}" -gt 0 ]; then docker image rm "${images[@]}" >/dev/null 2>&1 || true; fi
  rm -rf "$root"
}
trap cleanup EXIT
mkdir "$context"
export IMGBUILD_CACHE_DIR="$root/meta"
arch=$(docker version --format '{{.Server.Arch}}')
cat > "$context/Dockerfile" <<EOF
FROM busybox:latest AS build
COPY input /input
RUN --mount=type=cache,id=${root##*/},target=/cache sh -c 'cat /cache/tag 2>/dev/null > /result || echo fresh > /result; cp /input /cache/tag'
FROM scratch
COPY --from=build /result /result
EOF

check() {
  local scope=$1 tag=$2 value=$3 expected=$4 image log
  shift 4
  image="$IMGBUILD_CI_IMAGE:${root##*/}-$tag"
  log="$root/$tag.log"
  printf '%s\n' "$value" > "$context/input"
  "$@" "$IMGBUILD" build --dockerfile "$context/Dockerfile" --context "$context" --work "$root/work" \
    --platforms "linux/$arch" --dest "$image" --cache-repo "$IMGBUILD_CACHE_REPO" > "$log" 2>&1 \
    || { cat "$log"; exit 1; }
  docker pull -q "$image" >/dev/null
  images+=("$image")
  local cid actual
  cid=$(docker create "$image" /unused)
  actual=$(docker cp "$cid:/result" - | tar -xOf -)
  docker rm "$cid" >/dev/null
  test "$actual" = "$expected"
  printf '%s (%s): result=%s\n' "$tag" "$scope" "$actual"
}

check a first same fresh env IMGBUILD_CACHE_SCOPE=a
check a exact same fresh env IMGBUILD_CACHE_SCOPE=a
check b independent same fresh env IMGBUILD_CACHE_SCOPE=b
check a warm-a only-a same env IMGBUILD_CACHE_SCOPE=a
check b warm-b only-b same env IMGBUILD_CACHE_SCOPE=b
check a still-a end-a only-a env IMGBUILD_CACHE_SCOPE=a
check legacy unscoped separate fresh env -u IMGBUILD_CACHE_SCOPE
if env IMGBUILD_CACHE_SCOPE= "$IMGBUILD" build --dockerfile "$context/Dockerfile" --context "$context" \
  --platforms "linux/$arch" --dest "$IMGBUILD_CI_IMAGE:${root##*/}-invalid" --cache-repo "$IMGBUILD_CACHE_REPO" > "$root/invalid.log" 2>&1; then exit 1; fi
grep -F 'IMGBUILD_CACHE_SCOPE must not be empty' "$root/invalid.log"
