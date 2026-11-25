# imgbuild

Build and push multi-platform container images from Dockerfiles or TOML specs. Image layers are pushed directly to an OCI registry. Docker is required for Dockerfile `RUN` steps.

## Build and run

Build with stable Rust on Linux:

```sh
cargo build --locked --release
./target/release/imgbuild --help
```

For a quick test, start a disposable registry and build an image for the Docker daemon's architecture:

```sh
docker run --rm -d --name imgbuild-registry -p 127.0.0.1:5000:5000 registry:3
./target/release/imgbuild build --dockerfile Dockerfile --context . \
  --platforms "linux/$(docker version --format '{{.Server.Arch}}')" \
  --dest localhost:5000/example/image:test
docker stop imgbuild-registry
```

Use a separate `--work` directory per concurrent invocation. Only build Dockerfiles you trust.

## Live staging pulls

A pull that starts before a push ends cannot get anything: a registry refuses a manifest whose layers are missing. With
`IMGBUILD_STREAM_URL` set to a staging registry that implements `/_stream/` (comma separated; a name that resolves to
several addresses is one target per address, since every replica that serves pulls keeps its own copies), the final
image's push also

- announces its manifest as soon as every layer digest is known, before any layer is uploaded
  (`PUT /_stream/manifest/<repo>/<tag or digest>`), and
- copies each layer it uploads (`PUT /_stream/blob/<repo>/<digest>`), so the registry serves the layer to a pull while
  the builder is still uploading it.

With `IMGBUILD_EARLY_RUN=1`, completed final-stage `RUN` and `COPY` layers can upload in the background while subsequent steps run (up to two early uploads per build). A normal pull can start only once the final manifest is known, but it can overlap any uploads still in progress.

The destination registry stays the source of truth. Live copy failures are logged and don't fail a build. Live writes use the push credentials.

## Cache scope

On a shared builder, set `IMGBUILD_CACHE_SCOPE` in the trusted job launcher before `imgbuild build` to keep Dockerfile cache mounts, step results and incremental seeds distinct. Include the trust domain, toolchain and build target in the scope, and keep it stable across builds that should reuse cache. Do not take it from Dockerfile arguments or other job-controlled input. Unset preserves existing cache names and keys. This does not isolate daemon or registry access; jobs must not have access to either control plane.

## Releases

Release Please opens a version and changelog PR from Conventional Commits on
`main`. Merging that PR creates the GitHub release and publishes a versioned
image to GHCR for Linux amd64 and arm64. Pushing a matching version tag on
`main` also publishes the image. After the first publish, set the GHCR package
visibility to **Public** to allow anonymous pulls.

```sh
docker run --rm ghcr.io/wikibusiness/docker-builder:v0.1.0 --help
```

Dockerfile `RUN` steps need access to a local Linux Docker daemon and its
storage. The container image does not start a Docker daemon.
