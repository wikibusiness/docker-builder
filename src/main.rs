mod cdn;
mod dockerfile;
mod docker_cache;
mod exec;
mod layer;
mod prof;
mod registry;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::Parser;
use futures::stream::{self, StreamExt, TryStreamExt};
use registry::{Reg, Ref};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser)]
enum Cli {
    /// Build and push a multi-arch image from a Dockerfile, or assemble one from a TOML spec
    Build {
        /// TOML spec (alternative to --dockerfile)
        spec: Option<PathBuf>,
        /// Dockerfile to build, unchanged: RUN steps run only when their content key is new (see exec.rs)
        #[arg(long)]
        dockerfile: Option<PathBuf>,
        /// Dockerfile target (default: the last stage)
        #[arg(long)]
        target: Option<String>,
        /// Build context of the Dockerfile
        #[arg(long, default_value = ".")]
        context: PathBuf,
        /// `K=V`; repeatable
        #[arg(long)]
        build_arg: Vec<String>,
        /// `host/repo` for the cached build states (default: `<dest repo>-cache`)
        #[arg(long)]
        cache_repo: Option<String>,
        #[arg(long, default_value = "linux/amd64,linux/arm64", value_delimiter = ',')]
        platforms: Vec<String>,
        /// Every platform of the image, when this run builds only some (--platforms), e.g. one run per CPU:
        /// each run pushes `<tag>-<arch>`, and the one that finds all of them pushes the multi-platform `<tag>`
        #[arg(long, value_delimiter = ',')]
        all_platforms: Vec<String>,
        #[arg(long, required = true)]
        dest: Vec<String>,
        /// Base image, overriding the spec: `<ref>` for every platform or `<os/arch>=<ref>`; repeatable
        #[arg(long)]
        base: Vec<String>,
        #[arg(long, default_value = "/tmp/imgbuild")]
        work: PathBuf,
        /// Image path copied to a host directory after the build (e.g. `/build=$RUNNER_TEMP/out`); repeatable
        #[arg(long, value_name = "IMAGE_PATH=HOST_DIR")]
        export: Vec<String>,
        /// Do the base-image and non-`late` layer work now, then wait for this file to exist before building `late` layers
        #[arg(long)]
        wait_for: Option<PathBuf>,
    },
    /// Exit 0 when every image reference exists in its registry
    Exists { refs: Vec<String> },
    /// Point DEST at the image SRC holds (same repository; an existing DEST is kept, as tags may be immutable)
    Tag { src: String, dest: String },
    /// Sync a directory to an S3-compatible bucket (aws s3 sync --size-only)
    Cdn {
        dir: PathBuf,
        #[arg(long)]
        bucket: String,
        #[arg(long)]
        prefix: String,
        #[arg(long)]
        marker: Option<String>,
        #[arg(long)]
        endpoint: String,
        #[arg(long, default_value = "auto")]
        region: String,
        #[arg(long, default_value = "max-age=604800")]
        cache_control: String,
        #[arg(long, default_value_t = 128)]
        concurrency: usize,
    },
}

#[derive(Deserialize)]
struct Spec {
    #[serde(default)]
    base: String,
    #[serde(default)]
    bases: BTreeMap<String, String>,
    platforms: Vec<String>,
    config: Option<Cfg>,
    layers: Vec<LayerSpec>,
}

impl Spec {
    fn base_for(&self, platform: &str) -> &str {
        self.bases.get(platform).unwrap_or(&self.base)
    }
}

#[derive(Deserialize, Default)]
struct Cfg {
    workdir: Option<String>,
    user: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    cmd: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    expose: Vec<u16>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct LayerSpec {
    src: PathBuf,
    dest: String,
    chown: Option<String>,
    platform: Option<String>,
    key: Option<String>,
    #[serde(default)]
    late: bool,
    #[serde(default)]
    exclude: Vec<String>,
}

struct Built {
    spec_idx: usize,
    layer: layer::Layer,
    cached: bool,
}

pub fn rfc3339(epoch: u64) -> String {
    let (days, secs) = ((epoch / 86400) as i64, epoch % 86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

fn oci_layer_type(t: &str) -> String {
    match t {
        "application/vnd.docker.image.rootfs.diff.tar.gzip" => "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.docker.image.rootfs.diff.tar.zstd" => "application/vnd.oci.image.layer.v1.tar+zstd",
        t => t,
    }
    .to_string()
}

fn sha(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

struct Ctx {
    spec: Arc<Spec>,
    dest: Reg,
    dest_repo: String,
    epoch: u64,
}

async fn ensure_blob(ctx: &Ctx, digest: &str, src: Option<(&Reg, &str)>, file: Option<&PathBuf>) -> Result<bool> {
    if ctx.dest.blob_exists(&ctx.dest_repo, digest).await? {
        return Ok(false);
    }
    if let Some((sreg, srepo)) = src {
        if sreg.host == ctx.dest.host && ctx.dest.mount(&ctx.dest_repo, digest, srepo).await? {
            return Ok(true);
        }
        let data = sreg.get_blob(srepo, digest).await?;
        ctx.dest.put_blob(&ctx.dest_repo, digest, data).await?;
    } else if let Some(f) = file {
        ctx.dest.put_blob(&ctx.dest_repo, digest, Bytes::from(tokio::fs::read(f).await?)).await?;
    }
    Ok(true)
}

struct Prep {
    layers: Vec<Value>,
    cfg: Value,
}

async fn prep_platform(ctx: &Ctx, platform: &str, t0: Instant) -> Result<Prep> {
    let lap = |what: &str| eprintln!("  {platform}: {what} [{:.2}s]", t0.elapsed().as_secs_f64());
    let base = Ref::parse(ctx.spec.base_for(platform))?;
    let breg = Reg::new(&base.host);
    let (body, ct, _) = breg.manifest(&base.repo, &base.reference).await?;
    let mut m: Value = serde_json::from_slice(&body)?;
    if ct.contains("index") || ct.contains("manifest.list") {
        let (os, arch) = platform.split_once('/').context("platform os/arch")?;
        let d = m["manifests"].as_array().context("manifests")?.iter()
            .find(|e| e["platform"]["os"] == os && e["platform"]["architecture"] == arch)
            .with_context(|| format!("base has no {platform}"))?["digest"].as_str().unwrap().to_string();
        let (b, _, _) = breg.manifest(&base.repo, &d).await?;
        m = serde_json::from_slice(&b)?;
    }
    lap("base manifest");
    let cfg_digest = m["config"]["digest"].as_str().context("config digest")?;
    let cfg: Value = serde_json::from_slice(&breg.get_blob(&base.repo, cfg_digest).await?)?;

    let layers: Vec<Value> = m["layers"].as_array().context("layers")?.iter()
        .map(|l| json!({"mediaType": oci_layer_type(l["mediaType"].as_str().unwrap()), "digest": l["digest"], "size": l["size"]}))
        .collect();

    let same = breg.host == ctx.dest.host && base.repo == ctx.dest_repo;
    if !same {
        stream::iter(layers.iter().map(|l| l["digest"].as_str().unwrap().to_string()))
            .map(|d| { let b = &breg; let base = &base; async move { ensure_blob(ctx, &d, Some((b, &base.repo)), None).await } })
            .buffer_unordered(8).try_collect::<Vec<_>>().await?;
    }
    lap("base layers");
    Ok(Prep { layers, cfg })
}

fn for_platform<'a>(ctx: &Ctx, platform: &str, built: &'a [Built]) -> Vec<&'a Built> {
    built.iter().filter(|b| ctx.spec.layers[b.spec_idx].platform.as_deref().is_none_or(|p| p == platform)).collect()
}

/// Pushes each distinct layer once, so layers shared by several platforms are not uploaded in parallel twice.
async fn push_layers(ctx: &Ctx, built: &[Built]) -> Result<()> {
    let unique: BTreeMap<&str, &Built> = built.iter().map(|b| (b.layer.digest.as_str(), b)).collect();
    stream::iter(unique.into_values())
        .map(|b| async move { ensure_blob(ctx, &b.layer.digest, None, Some(&b.layer.path)).await })
        .buffer_unordered(8).try_collect::<Vec<_>>().await?;
    Ok(())
}

async fn finish_platform(ctx: &Ctx, platform: &str, prep: Prep, early: &[Built], late: &[Built], t0: Instant) -> Result<(Bytes, String, Value)> {
    let lap = |what: &str| eprintln!("  {platform}: {what} [{:.2}s]", t0.elapsed().as_secs_f64());
    let (os, arch) = platform.split_once('/').context("platform os/arch")?;
    let Prep { mut layers, mut cfg } = prep;
    let mut mine: Vec<&Built> = for_platform(ctx, platform, early).into_iter().chain(for_platform(ctx, platform, late)).collect();
    mine.sort_by_key(|b| b.spec_idx);
    let created = rfc3339(ctx.epoch);
    for b in &mine {
        layers.push(json!({"mediaType": "application/vnd.oci.image.layer.v1.tar+zstd", "digest": b.layer.digest, "size": b.layer.size}));
        cfg["rootfs"]["diff_ids"].as_array_mut().context("diff_ids")?.push(json!(b.layer.diff_id));
        let ls = &ctx.spec.layers[b.spec_idx];
        let h = cfg["history"].as_array_mut();
        if let Some(h) = h {
            h.push(json!({"created": created, "created_by": format!("imgbuild COPY {} /{}", ls.src.display(), ls.dest.trim_start_matches('/'))}));
        }
    }
    cfg["created"] = json!(created);
    if let Some(c) = &ctx.spec.config {
        let o = &mut cfg["config"];
        if let Some(v) = &c.workdir { o["WorkingDir"] = json!(v); }
        if let Some(v) = &c.user { o["User"] = json!(v); }
        if let Some(v) = &c.cmd { o["Cmd"] = json!(v); }
        if let Some(v) = &c.entrypoint { o["Entrypoint"] = json!(v); }
        if !c.env.is_empty() {
            let mut env: Vec<String> = o["Env"].as_array().map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect()).unwrap_or_default();
            for e in &c.env {
                let k = e.split('=').next().unwrap();
                env.retain(|x| x.split('=').next() != Some(k));
                env.push(e.clone());
            }
            o["Env"] = json!(env);
        }
        for p in &c.expose { o["ExposedPorts"][format!("{p}/tcp")] = json!({}); }
        for (k, v) in &c.labels { o["Labels"][k] = json!(v); }
    }
    let cfg_bytes = Bytes::from(serde_json::to_vec(&cfg)?);
    let cfg_digest = sha(&cfg_bytes);
    ctx.dest.put_blob(&ctx.dest_repo, &cfg_digest, cfg_bytes.clone()).await?;
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": cfg_digest, "size": cfg_bytes.len()},
        "layers": layers,
    });
    let mb = Bytes::from(serde_json::to_vec(&manifest)?);
    let md = sha(&mb);
    lap("config");
    let desc = json!({"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": md, "size": mb.len(), "platform": {"os": os, "architecture": arch}});
    Ok((mb, md, desc))
}

#[tokio::main]
async fn main() -> Result<()> {
    let epoch: u64 = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|v| v.parse().ok()).unwrap_or(1_704_067_200);
    let (spec_path, dest_args, base_args, work_dir, wait_for) = match Cli::parse() {
        Cli::Build { dockerfile: Some(dockerfile), spec: None, target, context, build_arg, cache_repo, platforms, all_platforms, dest, base, work, export, wait_for: None } => {
            anyhow::ensure!(base.is_empty(), "--base only applies to a TOML spec");
            anyhow::ensure!(platforms.iter().all(|p| all_platforms.is_empty() || all_platforms.contains(p)), "--all-platforms must include every --platforms");
            let export = export.iter().map(|s| {
                let (from, to) = s.split_once('=').context("--export requires IMAGE_PATH=HOST_DIR")?;
                Ok((from.to_string(), PathBuf::from(to)))
            }).collect::<Result<Vec<_>>>()?;
            let digest = exec::build(exec::Opts { dockerfile, context, target, platforms, dests: dest, cache: cache_repo, build_args: build_arg, work, epoch, all_platforms, export }).await?;
            println!("{digest}");
            return Ok(());
        }
        Cli::Build { spec: Some(spec), dockerfile: None, dest, base, work, wait_for, .. } => (spec, dest, base, work, wait_for),
        Cli::Build { .. } => bail!("pass either a spec file or --dockerfile (--wait-for is for specs only)"),
        Cli::Exists { refs } => {
            let mut missing = false;
            for r in &refs {
                let r = Ref::parse(r)?;
                let found = Reg::new(&r.host).manifest_exists(&r.repo, &r.reference).await?;
                eprintln!("{} {}:{}", if found { "found  " } else { "missing" }, r.repo, r.reference);
                missing |= !found;
            }
            std::process::exit(missing as i32);
        }
        Cli::Tag { src, dest } => {
            let (s, d) = (Ref::parse(&src)?, Ref::parse(&dest)?);
            anyhow::ensure!(s.host == d.host && s.repo == d.repo, "tag: {src} and {dest} must be in the same repository");
            let reg = Reg::new(&s.host);
            let body = reg.manifest_opt(&s.repo, &s.reference).await?.with_context(|| format!("{src} not found"))?;
            let mt = serde_json::from_slice::<serde_json::Value>(&body)?["mediaType"].as_str().context("manifest without mediaType")?.to_string();
            match reg.put_manifest(&d.repo, &d.reference, &mt, body).await {
                Ok(()) => eprintln!("tagged {dest}"),
                Err(e) => {
                    anyhow::ensure!(reg.manifest_exists(&d.repo, &d.reference).await?, e);
                    eprintln!("tag {} already exists, kept", d.reference);
                }
            }
            return Ok(());
        }
        Cli::Cdn { dir, bucket, prefix, marker, endpoint, region, cache_control, concurrency } => {
            return cdn::sync(cdn::Args { dir, bucket, prefix, marker, endpoint, region, cache_control, concurrency }).await;
        }
    };
    let t0 = Instant::now();
    let mut spec = toml::from_str::<Spec>(&std::fs::read_to_string(&spec_path).context("read spec")?)?;
    for b in base_args {
        match b.split_once('=').filter(|(p, _)| p.contains('/')) {
            Some((platform, r)) => { spec.bases.insert(platform.to_string(), r.to_string()); }
            None => spec.base = b,
        }
    }
    for p in &spec.platforms {
        anyhow::ensure!(!spec.base_for(p).is_empty(), "no base image for {p}: set `base` in the spec or pass --base");
    }
    let dests: Vec<Ref> = dest_args.iter().map(|d| Ref::parse(d)).collect::<Result<_>>()?;
    let d0 = dests[0].clone();

    let work = Arc::new(work_dir.clone());
    let spec = Arc::new(spec);
    let build_layers = |late: bool| {
        let handles: Vec<_> = (0..spec.layers.len()).filter(|&i| spec.layers[i].late == late).map(|i| {
            let (spec, work) = (spec.clone(), work.clone());
            tokio::task::spawn_blocking(move || -> Result<Built> {
                let ls = &spec.layers[i];
                let (uid, gid) = ls.chown.as_deref().and_then(|c| c.split_once(':')).map(|(u, g)| (u.parse().unwrap_or(0), g.parse().unwrap_or(0))).unwrap_or((0, 0));
                let (layer, cached) = layer::build_cached(&layer::Source { key: ls.key.as_deref(), src: &ls.src, dest: &ls.dest, uid, gid, epoch, exclude: &ls.exclude }, &work)?;
                Ok(Built { spec_idx: i, layer, cached })
            })
        }).collect();
        async move {
            let mut built = vec![];
            for h in handles { built.push(h.await??); }
            Ok::<_, anyhow::Error>(built)
        }
    };
    let report = |built: &[Built]| {
        for b in built {
            let ls = &spec.layers[b.spec_idx];
            eprintln!("layer {:<28} {:>8.1} MB  {}  {}", ls.dest, b.layer.size as f64 / 1e6, &b.layer.digest[..19], if b.cached { "cached" } else { "built" });
        }
    };
    let early = build_layers(false).await?;
    report(&early);
    eprintln!("[{:>5.2}s] layers built", t0.elapsed().as_secs_f64());

    let platforms = spec.platforms.clone();
    let ctx = Ctx { spec: spec.clone(), dest: Reg::new(&d0.host), dest_repo: d0.repo.clone(), epoch };
    let preps = stream::iter(platforms.iter()).map(|p| prep_platform(&ctx, p, t0)).buffered(4).try_collect::<Vec<_>>().await?;
    push_layers(&ctx, &early).await?;
    eprintln!("[{:>5.2}s] base and early layers pushed", t0.elapsed().as_secs_f64());

    if let Some(f) = &wait_for {
        while !f.exists() { tokio::time::sleep(std::time::Duration::from_millis(50)).await; }
        eprintln!("[{:>5.2}s] {} present", t0.elapsed().as_secs_f64(), f.display());
    }
    let late = build_layers(true).await?;
    report(&late);
    let configs = stream::iter(platforms.iter().zip(preps))
        .map(|(p, prep)| finish_platform(&ctx, p, prep, &early, &late, t0))
        .buffered(4).try_collect::<Vec<_>>();
    let ((), res) = tokio::try_join!(push_layers(&ctx, &late), configs)?;
    eprintln!("[{:>5.2}s] late layers pushed", t0.elapsed().as_secs_f64());
    stream::iter(&res)
        .map(|(mb, md, _)| ctx.dest.put_manifest(&ctx.dest_repo, md, "application/vnd.oci.image.manifest.v1+json", mb.clone()))
        .buffer_unordered(4).try_collect::<Vec<_>>().await?;
    eprintln!("[{:>5.2}s] platform images pushed", t0.elapsed().as_secs_f64());

    let mut descs: Vec<Value> = res.iter().map(|(_, _, d)| d.clone()).collect();
    descs.sort_by_key(|d| d["platform"]["architecture"].to_string());
    let index = Bytes::from(serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": descs}))?);
    let index_digest = sha(&index);
    for d in &dests {
        if d.repo != ctx.dest_repo { bail!("all --dest must share a repo"); }
        if let Err(e) = ctx.dest.put_manifest(&d.repo, &d.reference, "application/vnd.oci.image.index.v1+json", index.clone()).await {
            // Tags are immutable in Harbor: an existing one is refused.
            anyhow::ensure!(ctx.dest.manifest_exists(&d.repo, &d.reference).await?, e);
            eprintln!("tag {} already exists, kept", d.reference);
        }
    }
    println!("{index_digest}");
    eprintln!("[{:>5.2}s] done", t0.elapsed().as_secs_f64());
    Ok(())
}
