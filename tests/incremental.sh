#!/usr/bin/env bash
set -euo pipefail

: "${IMGBUILD:?path to imgbuild}" "${IMGBUILD_CI_IMAGE:?destination repository}" "${IMGBUILD_CACHE_REPO:?cache repository}"
arch=$(docker version --format '{{.Server.Arch}}')
root=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/imgbuild-incremental.XXXXXXXX")
context=$root/context
export IMGBUILD_CACHE_DIR=$root/cache IMGBUILD_INCREMENTAL=1 IMGBUILD_TRACE=1
containers=()
images=()
cleanup() {
  if [ -n "${containers[*]:-}" ]; then docker rm "${containers[@]}" >/dev/null 2>&1 || true; fi
  if [ -n "${images[*]:-}" ]; then docker image rm "${images[@]}" >/dev/null 2>&1 || true; fi
  rm -rf "$root"
}
trap cleanup EXIT
mkdir -p "$context" "$IMGBUILD_CACHE_DIR"
cat > "$context/Dockerfile" <<'EOF'
FROM busybox:latest AS build
COPY pnpm /usr/bin/pnpm
COPY input /input
RUN pnpm install
FROM scratch
COPY --from=build /output /output
EOF
cat > "$context/pnpm" <<'EOF'
#!/bin/sh
set -eu
[ "$1" = install ]
if [ -f /output/result ]; then echo reused-seed; else echo fresh-run; fi
mkdir -p /output
cp /input /output/result
EOF
chmod +x "$context/pnpm"

for n in 1 2 3; do
  value="${root##*/}-$n"
  printf '%s\n' "$value" > "$context/input"
  image="$IMGBUILD_CI_IMAGE:ci-$value-$arch"
  log="$root/build-$n.log"
  "$IMGBUILD" build --dockerfile "$context/Dockerfile" --context "$context" --work "$root/work-$n" \
    --platforms "linux/$arch" --dest "$image" --cache-repo "$IMGBUILD_CACHE_REPO" > "$log" 2>&1 \
    || { cat "$log"; exit 1; }
  expected=fresh-run
  if [ "$n" = 2 ]; then expected=reused-seed; fi
  grep -Eq "^#[0-9]+ [0-9.]+ $expected$" "$log" || { cat "$log"; exit 1; }
  seed=$(jq -er .image "$IMGBUILD_CACHE_DIR"/seeds/*)
  images+=("$seed" "$image")
  docker image inspect "$seed" >/dev/null
  if [ -n "$(docker ps -aq --filter "ancestor=$seed")" ]; then
    echo "imgbuild left a container holding seed image $seed" >&2
    exit 1
  fi
  docker pull -q "$image" >/dev/null
  container=$(docker create "$image" /unused)
  containers+=("$container")
  docker cp "$container:/output/result" "$root/result"
  test "$(< "$root/result")" = "$value"
  if [ "$n" = 2 ]; then docker image rm "$seed" >/dev/null; fi
  printf 'Build %s: %s, correct output, no seed container left behind\n' "$n" "$expected"
done
