# Changelog

## [0.2.0](https://github.com/wikibusiness/docker-builder/compare/v0.1.0...v0.2.0) (2026-11-26)


### Features

* add IMGBUILD_CACHE_SCOPE to separate build caches ([#5](https://github.com/wikibusiness/docker-builder/issues/5)) ([6a78080](https://github.com/wikibusiness/docker-builder/commit/6a780808f6fd5ce9e45a1ad8a0ff9cac72ebb6d0))
* add opt-in per-layer timing events (IMGBUILD_LAYER_TIMING) ([33467ce](https://github.com/wikibusiness/docker-builder/commit/33467ceb63a9c8d1d03b73066b360ba4383ed8d6))
* allow selected warmup slots to fetch independently ([d6187ef](https://github.com/wikibusiness/docker-builder/commit/d6187efaf2052a04852f267d963b67847f6f284e))
* announce a staging-only image prefix for Kubernetes image volumes ([3456ded](https://github.com/wikibusiness/docker-builder/commit/3456dedb1a5209bdb8fa9655b0231527202425d7))
* announce ordered staging-only image prefixes for each completed layer group ([69fdc8e](https://github.com/wikibusiness/docker-builder/commit/69fdc8e34287d95b9de77f63359d366de3ae8c66))
* announce ordered staging-only image prefixes for Kubernetes image volumes ([8e7c03c](https://github.com/wikibusiness/docker-builder/commit/8e7c03cea7e6f7294aec8face8c963835e420a29))
* copy the pushed image to staging registries while it uploads ([#6](https://github.com/wikibusiness/docker-builder/issues/6)) ([02a8f0f](https://github.com/wikibusiness/docker-builder/commit/02a8f0f175afe5d6cf70818987d54c94c7303d77))
* emit opt-in per-layer timing events with IMGBUILD_LAYER_TIMING ([72e5dca](https://github.com/wikibusiness/docker-builder/commit/72e5dcadcad3f90471aecece73d5e7f37144cb4a))
* export Dockerfile stages before the final image is ready ([00828a9](https://github.com/wikibusiness/docker-builder/commit/00828a94948a3a35bd28f72faa779d5eb8454eb4))
* let selected warmup slots fetch independently ([624d2ed](https://github.com/wikibusiness/docker-builder/commit/624d2ed321aa0c6c77c823149abfd39364b95306))


### Bug Fixes

* deduplicate repeated build platforms ([ec2f2c4](https://github.com/wikibusiness/docker-builder/commit/ec2f2c453a21d3c9768d17fe4833e35086ef8456))
* export build files directly to the requested directory ([93c5d8e](https://github.com/wikibusiness/docker-builder/commit/93c5d8e86a543b5353c14f3dfc50a66ad672dfe7))
* export build files directly to the requested directory ([6af090b](https://github.com/wikibusiness/docker-builder/commit/6af090b384001106fcf5dc363d0284fb81cb7343))
* repair same-size CDN asset mismatches before release ([02d9fe9](https://github.com/wikibusiness/docker-builder/commit/02d9fe9219cd088b5417e0a49d50fd21319d9d73))
* verify streamed CDN assets during final sync ([49b9c3e](https://github.com/wikibusiness/docker-builder/commit/49b9c3ef91c2db472dd1c1efaf9ba264dc89e412))

## 0.1.0 (2026-11-19)


### Features

* publish versioned multi-platform Docker images with Release Please ([7b8ecd3](https://github.com/wikibusiness/docker-builder/commit/7b8ecd3a439e6ab20b5a184929339cbf3966c93f))
