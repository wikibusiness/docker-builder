#!/usr/bin/env bash
set -euo pipefail

: "${IMGBUILD:?path to imgbuild}" "${IMGBUILD_CI_IMAGE:?destination repository}" "${IMGBUILD_CACHE_REPO:?cache repository}"
root=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/imgbuild-platforms.XXXXXXXX")
trap 'rm -rf "$root"' EXIT
mkdir "$root/context"
printf 'FROM scratch\nCOPY payload /payload\n' > "$root/context/Dockerfile"
printf '%s\n' "${root##*/}" > "$root/context/payload"
ref="$IMGBUILD_CI_IMAGE:${root##*/}"
manifest="http://${IMGBUILD_CI_IMAGE%%/*}/v2/${IMGBUILD_CI_IMAGE#*/}/manifests/${root##*/}"
accept='Accept: application/vnd.docker.distribution.manifest.list.v2+json'

build() {
  "$IMGBUILD" build --dockerfile "$root/context/Dockerfile" --context "$root/context" \
    --work "$root/work" --cache-repo "$IMGBUILD_CACHE_REPO" "$@"
}
index() {
  curl -fsS -H "$accept" "$1" | jq -ce '
    .manifests | map(.platform.os + "/" + .platform.architecture)
    | select(. == ["linux/arm64", "linux/amd64"])'
}

all=(--all-platforms 'linux/arm64,linux/amd64' --all-platforms 'linux/arm64,linux/amd64')
build --platforms linux/arm64,linux/arm64 "${all[@]}" --dest "$ref-split"
test "$(curl -sS -o /dev/null -w '%{http_code}' -H "$accept" "$manifest-split")" = 404
build --platforms linux/amd64 "${all[@]}" --dest "$ref-split" --export "/payload=$root/export"
cmp "$root/context/payload" "$root/export/payload"
index "$manifest-split"
build --platforms linux/arm64,linux/amd64,linux/arm64 --dest "$ref-full"
index "$manifest-full"

build --platforms linux/amd64 --dest "$ref-index"
curl -fsS -H "$accept" "$manifest-index" | jq -e '.manifests | length == 1'
digest=$(build --platforms linux/amd64 --single-manifest --dest "$ref-direct")
body=$(curl -fsS -H 'Accept: application/vnd.oci.image.manifest.v1+json' "$manifest-direct")
jq -e '.mediaType == "application/vnd.oci.image.manifest.v1+json" and (.layers | length == 1)' <<<"$body"
test "$digest" = "sha256:$(printf %s "$body" | sha256sum | cut -d' ' -f1)"
if build --platforms linux/amd64,linux/arm64 --single-manifest --dest "$ref-invalid"; then
  exit 1
fi
