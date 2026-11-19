use std::process::Command;

pub const RUN_LABEL: &str = "io.ocean.imgbuild.cache=run";
pub const LABEL_KEY: &str = "io.ocean.imgbuild.cache";
pub const IMAGE_LABEL: &str = "io.ocean.imgbuild.cache=image";

pub fn command(daemon: Option<&str>) -> Command {
    let mut cmd = Command::new("docker");
    if let Some(host) = daemon {
        cmd.env("DOCKER_HOST", host);
    }
    cmd
}

pub fn prune(daemon: Option<&str>, low_disk: bool) {
    // Docker ages containers from creation, not exit. Keep a full day for RUN and layer extraction.
    for (kind, args, label, age) in [
        ("container", ["prune", "-f"], RUN_LABEL, "24h"),
        (
            "image",
            ["prune", "-af"],
            IMAGE_LABEL,
            if low_disk { "1h" } else { "24h" },
        ),
    ] {
        let mut cmd = command(daemon);
        cmd.arg(kind).args(args);
        match prune_matching(cmd, label, age) {
            Ok(out) if out.status.success() => {
                if let Some(total) = String::from_utf8_lossy(&out.stdout).lines().last()
                    && total.starts_with("Total reclaimed space:")
                    && !total.ends_with(" 0B")
                {
                    eprintln!("imgbuild: Docker {kind} cache: {total}");
                }
            }
            Ok(out) => eprintln!(
                "imgbuild: Docker {kind} cache cleanup failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(err) => eprintln!("imgbuild: Docker {kind} cache cleanup failed: {err}"),
        }
    }
}

fn prune_matching(
    mut cmd: Command,
    label: &str,
    age: &str,
) -> std::io::Result<std::process::Output> {
    cmd.args([
        "--filter",
        &format!("label={label}"),
        "--filter",
        &format!("until={age}"),
    ])
    .output()
}

pub fn remove_image(daemon: Option<&str>, image: &str) -> bool {
    command(daemon)
        .args(["image", "rm", image])
        .output()
        .is_ok_and(|out| {
            out.status.success()
                || String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .any(|line| line.ends_with(&format!("No such image: {image}")))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn test_id() -> u128 {
        UNIX_EPOCH.elapsed().unwrap().as_nanos()
    }

    struct Seed(String);

    impl Seed {
        fn start(test: &str) -> Self {
            let name = format!("imgbuild-test-{test}-{}", test_id());
            let image = std::env::var("IMGBUILD_TEST_DOCKER_IMAGE")
                .unwrap_or_else(|_| "debian:bookworm-slim".into());
            let out = command(None)
                .args([
                    "run",
                    "--pull=never",
                    "--rm",
                    "-d",
                    "--name",
                    &name,
                    &image,
                    "sleep",
                    "300",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            Self(name)
        }

        fn exists(&self) -> bool {
            command(None)
                .args(["container", "inspect", &self.0])
                .output()
                .unwrap()
                .status
                .success()
        }
    }

    impl Drop for Seed {
        fn drop(&mut self) {
            let _ = command(None).args(["rm", "-f", &self.0]).output();
        }
    }

    #[test]
    #[ignore = "needs a local Linux Docker daemon with IMGBUILD_TEST_DOCKER_IMAGE already pulled"]
    fn daemon_prune_only_removes_expired_managed_stopped_containers() {
        let image = std::env::var("IMGBUILD_TEST_DOCKER_IMAGE")
            .unwrap_or_else(|_| "debian:bookworm-slim".into());
        let label = format!("io.ocean.imgbuild.test={}", test_id());
        let start = |name, managed, active| {
            let seed = Seed(format!("imgbuild-test-{name}-{}", test_id()));
            let mut cmd = command(None);
            if active {
                cmd.args(["run", "-d"]);
            } else {
                cmd.arg("create");
            }
            cmd.args(["--name", &seed.0]);
            if managed {
                cmd.args(["--label", &label]);
            }
            let out = cmd.args([&image, "sleep", "30"]).output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            seed
        };
        let expired = start("expired", true, false);
        let unrelated = start("unrelated", false, false);
        let active = start("active", true, true);
        let cutoff = command(None)
            .args([
                "container",
                "inspect",
                "--format",
                "{{.Created}}",
                &active.0,
            ])
            .output()
            .unwrap();
        assert!(cutoff.status.success());
        let fresh = start("fresh", true, false);
        let mut cmd = command(None);
        cmd.args(["container", "prune", "-f"]);
        let out =
            prune_matching(cmd, &label, String::from_utf8_lossy(&cutoff.stdout).trim()).unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!expired.exists());
        assert!(unrelated.exists() && active.exists() && fresh.exists());
    }

    #[test]
    #[ignore = "needs a local Linux Docker daemon with IMGBUILD_TEST_DOCKER_IMAGE already pulled"]
    fn image_deletion_retries_while_a_container_pins_it() {
        let pin = Seed::start("image-pin");
        let image = format!("imgbuild-test-image:{}", test_id());
        let out = command(None)
            .args(["commit", &pin.0, &image])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pin_image = Seed(format!("{}-ref", pin.0));
        let out = command(None)
            .args(["create", "--name", &pin_image.0, &image, "true"])
            .output()
            .unwrap();
        let pinned = out.status.success() && !remove_image(None, &image);
        drop(pin_image);
        assert!(remove_image(None, &image));
        assert!(pinned, "image was removed while still referenced");
        assert!(
            remove_image(None, &image),
            "already-removed images should not be retried"
        );
    }
}
