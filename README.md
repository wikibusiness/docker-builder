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
