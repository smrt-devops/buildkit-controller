//! `cargo xtask image` builds a Linux container image of `buildkit-controller`
//! for the host architecture. It needs no network access beyond the host's
//! cached crates and local base images.
//!
//! The task vendors sources on the host (`cargo vendor`) and compiles them
//! offline inside the official `rust` image that matches `rust-toolchain.toml`.
//! A named volume keeps the target directory, so rebuilds are incremental. The
//! COPY-only `docker/Dockerfile` then packages the binary.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::root;

/// Named Docker volume holding the in-container target directory.
const TARGET_VOLUME: &str = "buildkit-controller-image-target";
/// Binary name produced by the server crate.
const BINARY: &str = "buildkit-controller";

/// Holds `target/xtask-host.lock` until dropped. The image build (`target/image`
/// and the cargo volume), the worker image and the gateway download
/// (`target/gateway`) are shared by every task run from this checkout, so a
/// parallel run waits here. The lock ends with the process at worst.
pub(crate) fn host_lock() -> Result<std::fs::File> {
    let path = root().join("target/xtask-host.lock");
    std::fs::create_dir_all(root().join("target"))?;
    let lock = std::fs::File::create(&path)?;
    lock.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(lock)
}

/// Run a command and fail with its status if it does not succeed.
fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed with {status}");
    }
    Ok(())
}

/// Builder image for the pinned toolchain channel, read from
/// `docker/builder-image` (`rust:<major>.<minor>-trixie@sha256:<digest>`).
/// Trixie keeps the build glibc equal to the distroless debian13 runtime. The
/// recorded tag must match the channel, so a toolchain bump cannot silently
/// keep building on a stale image.
fn rust_image(toolchain_toml: &str, pinned: &str) -> Result<String> {
    let channel = toolchain_toml
        .lines()
        .find_map(|l| l.trim().strip_prefix("channel"))
        .and_then(|l| l.split('"').nth(1))
        .context("no channel in rust-toolchain.toml")?;
    let mut parts = channel.split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        bail!("unexpected toolchain channel {channel:?}");
    };
    let tag = format!("rust:{major}.{minor}-trixie");
    let pinned = pinned.trim();
    if pinned
        .strip_prefix(&tag)
        .is_some_and(|d| d.starts_with("@sha256:"))
    {
        Ok(pinned.to_owned())
    } else {
        bail!("docker/builder-image must be {tag}@sha256:<digest>, found {pinned:?}")
    }
}

/// Vendor all dependencies unless the vendor directory is newer than `Cargo.lock`.
fn vendor(root: &Path, work: &Path) -> Result<()> {
    let vendor = work.join("vendor");
    let stamp = work.join("vendor.stamp");
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let fresh = match (mtime(&stamp), mtime(&root.join("Cargo.lock"))) {
        (Some(s), Some(l)) => s >= l && vendor.is_dir(),
        _ => false,
    };
    if fresh {
        return Ok(());
    }
    run(Command::new("cargo")
        .current_dir(root)
        .args(["vendor", "--locked", "--versioned-dirs"])
        .arg(&vendor)
        .stdout(std::process::Stdio::null()))?;
    std::fs::write(&stamp, b"").context("writing vendor stamp")
}

/// The target volume of this checkout. Cargo judges freshness by mtime, so with
/// a volume shared by several worktrees, another checkout's later build can
/// make this checkout's edits look up to date and ship a stale binary. One
/// volume per checkout path avoids that, and also avoids concurrent builds
/// sharing a volume.
fn target_volume(root: &Path) -> String {
    let hash = crate::kube::fnv1a(root.as_os_str().as_encoded_bytes());
    format!("{TARGET_VOLUME}-{hash:08x}")
}

/// Label on each per-checkout target volume: the checkout's absolute path.
const CHECKOUT_LABEL: &str = "buildkit-controller.checkout";

/// Create this checkout's target volume with its label. `docker volume create`
/// never relabels an existing volume. When nothing uses an unlabelled volume
/// (one from before the label existed), this recreates it. Otherwise it prints
/// a hint to remove the volume by hand.
fn ensure_volume(root: &Path) -> Result<()> {
    let name = target_volume(root);
    let label = Command::new("docker")
        .args(["volume", "inspect", "--format"])
        .arg(format!("{{{{index .Labels \"{CHECKOUT_LABEL}\"}}}}"))
        .arg(&name)
        .output()
        .context("running docker volume inspect")?;
    let have = String::from_utf8_lossy(&label.stdout);
    let have = have.trim();
    if label.status.success() && (have.is_empty() || have == "<no value>") {
        let removed = Command::new("docker")
            .args(["volume", "rm", &name])
            .output()
            .is_ok_and(|o| o.status.success());
        if !removed {
            eprintln!("volume {name} is unlabelled and in use; once idle: docker volume rm {name}");
        }
    }
    run(Command::new("docker")
        .args(["volume", "create", "--label"])
        .arg(format!("{CHECKOUT_LABEL}={}", root.display()))
        .arg(&name)
        .stdout(std::process::Stdio::null()))
}

/// Pick which of `volumes` to remove. Each entry is the volume name and its
/// checkout label, which is empty when the volume is unlabelled. This removes
/// the old shared volume and labelled volumes whose checkout no longer exists.
/// It keeps unlabelled hashed volumes because their checkout is unknown.
fn prunable(volumes: &[(String, String)], exists: impl Fn(&Path) -> bool) -> Vec<String> {
    volumes
        .iter()
        .filter(|(name, label)| {
            name == TARGET_VOLUME
                || (name.starts_with(&format!("{TARGET_VOLUME}-"))
                    && !label.is_empty()
                    && !exists(Path::new(label)))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Remove orphaned target volumes. Never removes one whose checkout still exists.
pub(crate) fn prune() -> Result<()> {
    let out = Command::new("docker")
        .args(["volume", "ls", "--format"])
        .arg(format!(
            "{{{{.Name}}}}\t{{{{.Label \"{CHECKOUT_LABEL}\"}}}}"
        ))
        .output()
        .context("running docker volume ls")?;
    if !out.status.success() {
        bail!("docker volume ls failed with {}", out.status);
    }
    let volumes: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let (n, v) = l.split_once('\t').unwrap_or((l, ""));
            (n.to_owned(), v.trim().to_owned())
        })
        .collect();
    for name in prunable(&volumes, Path::exists) {
        match Command::new("docker")
            .args(["volume", "rm", &name])
            .output()
        {
            Ok(o) if o.status.success() => println!("removed {name}"),
            _ => eprintln!("could not remove {name} (in use?)"),
        }
    }
    Ok(())
}

/// Whether the host builds the image's own platform (linux/amd64), so cargo can compile
/// into the workspace target dir, which CI caches, instead of a throwaway docker volume.
fn native_host(os: &str, arch: &str) -> bool {
    os == "linux" && arch == "x86_64"
}

/// Compile the release binary on the host and copy it to `out`. The runtime base image
/// (glibc 2.41) is newer than the runner's (2.39), so the binary runs there.
fn compile_native(root: &Path, out: &Path) -> Result<()> {
    run(Command::new("cargo").current_dir(root).args([
        "build",
        "--release",
        "--locked",
        "-p",
        "buildkit-controller",
        "--bin",
        BINARY,
    ]))?;
    std::fs::copy(root.join("target/release").join(BINARY), out.join(BINARY))
        .context("copying the native binary")?;
    Ok(())
}

/// Compile the release binary in the rust image and copy it to `out`.
fn compile(root: &Path, work: &Path, image: &str, out: &Path) -> Result<()> {
    ensure_volume(root)?;
    // Same layout that `cargo vendor` prints, with container paths.
    std::fs::write(
        work.join("cargo-config.toml"),
        "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"/work/vendor\"\n",
    )?;
    let mount = |src: &Path, dst: &str, mode: &str| format!("{}:{dst}:{mode}", src.display());
    // rust-toolchain.toml decides the toolchain: rustup installs the pinned
    // version when the image ships a different one, which needs the network.
    // Local-only fallback for an offline Docker: set BK_IMAGE_PREINSTALLED=1 to
    // build with the toolchain the image ships instead (CI never sets it).
    let pick = if preinstalled() {
        "export RUSTUP_TOOLCHAIN=\"$(rustup toolchain list | head -n1 | cut -d' ' -f1)\"; "
    } else {
        ""
    };
    let script = format!(
        "set -eu; {pick}\
         cargo build --release --locked --offline -p buildkit-controller --bin {BINARY} \
           --config /work/cargo-config.toml; \
         cp /target/release/{BINARY} /out/{BINARY}"
    );
    run(Command::new("docker")
        .args(["run", "--rm", "-w", "/src"])
        .args([
            "-e",
            "CARGO_NET_OFFLINE=true",
            "-e",
            "CARGO_TARGET_DIR=/target",
        ])
        .args(["-v", &mount(root, "/src", "ro")])
        .args(["-v", &mount(work, "/work", "ro")])
        .args(["-v", &mount(out, "/out", "rw")])
        .args(["-v", &format!("{}:/target", target_volume(root))])
        .args([image, "sh", "-c", &script]))
}

/// The tag every suite's manifests and chart values name the controller by.
pub(crate) const CONTROLLER_IMAGE: &str = "buildkit-controller:dev";
/// Repository of the content-tagged controller images.
const REPOSITORY: &str = "buildkit-controller";
/// Image label holding the SHA-256 of the inputs the image was built from.
const INPUTS_LABEL: &str = "buildkit-controller.inputs";

/// Repository-relative files and directories the controller image is built
/// from. Every workspace manifest counts, because cargo reads them all.
const INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "xtask/Cargo.toml",
    // The compile flags live here.
    "xtask/src/image.rs",
    "docker",
    "crates",
];

/// Every file under `path`, or `path` itself when it is a file.
fn files_under(path: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    for entry in std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        files_under(&entry?.path(), out)?;
    }
    Ok(())
}

/// SHA-256 over the paths and contents of the image's inputs and the
/// `BK_IMAGE_PREINSTALLED` setting, which changes the toolchain. Test
/// directories are left out: they never reach the binary.
pub(crate) fn inputs_hash(root: &Path, preinstalled: bool) -> Result<String> {
    let mut files = Vec::new();
    for rel in INPUTS {
        files_under(&root.join(rel), &mut files)?;
    }
    let mut rels: Vec<(String, std::path::PathBuf)> = files
        .into_iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(root).ok()?.to_string_lossy().into_owned();
            let in_tests = rel.split('/').any(|c| c == "tests" || c == "target");
            (!in_tests).then_some((rel, p))
        })
        .collect();
    rels.sort();
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(&[u8::from(preinstalled)]);
    for (rel, path) in rels {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        ctx.update(rel.as_bytes());
        ctx.update(&[0]);
        ctx.update(&(bytes.len() as u64).to_le_bytes());
        ctx.update(&bytes);
    }
    Ok(crate::gateway::hex(ctx.finish().as_ref()))
}

/// Whether `BK_IMAGE_PREINSTALLED` selects the toolchain the build image ships.
fn preinstalled() -> bool {
    std::env::var_os("BK_IMAGE_PREINSTALLED").is_some()
}

/// The inputs label of the local image `tag`, empty when it has none.
fn inputs_label(tag: &str) -> String {
    Command::new("docker")
        .args(["image", "inspect", "--format"])
        .arg(format!("{{{{index .Config.Labels \"{INPUTS_LABEL}\"}}}}"))
        .arg(tag)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// The controller image of this checkout, tagged by a prefix of its inputs
/// hash. It is built only when Docker has no image with that hash, so a run
/// whose sources have not changed skips the build. Returns the tag.
///
/// The tag is per content, not `:dev`, so a build from another checkout cannot
/// replace the image between this build and the `kind load` that follows.
pub(crate) fn controller() -> Result<String> {
    let _lock = host_lock()?;
    let root = root();
    let hash = inputs_hash(&root, preinstalled())?;
    let tag = format!("{REPOSITORY}:{}", &hash[..16]);
    if inputs_label(&tag) == hash {
        println!("image {tag} is up to date");
        return Ok(tag);
    }
    build(&root, &tag, &hash)?;
    Ok(tag)
}

/// Loads the controller image into `env`, building it first if its inputs
/// changed, and tags it there as [`CONTROLLER_IMAGE`], the name every suite uses.
pub(crate) fn load_controller(env: &crate::kube::Cluster) -> Result<()> {
    let tag = controller()?;
    crate::proc::status(&mut crate::kube::kind_load(&tag, env.name()))?;
    env.tag_image(&tag, CONTROLLER_IMAGE)
}

/// Build the image, tag it, and optionally load it into a kind cluster.
pub(crate) fn image(tag: &str, load_kind: Option<&str>) -> Result<()> {
    let built = controller()?;
    run(Command::new("docker").args(["tag", &built, tag]))?;
    if let Some(cluster) = load_kind {
        run(&mut crate::kube::kind_load(tag, cluster))?;
    }
    println!("tagged {built} as {tag}");
    Ok(())
}

/// Compile the controller and build the image `tag`, labelled with `hash`.
fn build(root: &Path, tag: &str, hash: &str) -> Result<()> {
    let _t = crate::proc::Timed::named("image build");
    let root = root.to_path_buf();
    let work = root.join("target/image");
    let ctx = work.join("ctx");
    std::fs::create_dir_all(&ctx)?;
    let toolchain = std::fs::read_to_string(root.join("rust-toolchain.toml"))
        .context("reading rust-toolchain.toml")?;
    let pinned = std::fs::read_to_string(root.join("docker/builder-image"))
        .context("reading docker/builder-image")?;
    let rust = rust_image(&toolchain, &pinned)?;

    if native_host(std::env::consts::OS, std::env::consts::ARCH) {
        compile_native(&root, &ctx)?;
    } else {
        vendor(&root, &work)?;
        compile(&root, &work, &rust, &ctx).context(
            "compiling in the rust image; if rustup could not download the pinned toolchain \
             and Docker is offline, retry with BK_IMAGE_PREINSTALLED=1 (local builds only)",
        )?;
    }
    std::fs::copy(root.join("docker/Dockerfile"), ctx.join("Dockerfile"))
        .context("copying Dockerfile")?;
    run(Command::new("docker")
        .args(["build", "-t", tag, "--label"])
        .arg(format!("{INPUTS_LABEL}={hash}"))
        .arg(&ctx))?;
    println!("built {tag}");
    Ok(())
}

/// The rootless `BuildKit` image the cluster harnesses run workers on.
pub(crate) const WORKER_IMAGE: &str = "moby/buildkit:v0.34.0-rootless";
/// A locally built `BuildKit` image that stands in for [`WORKER_IMAGE`] when the
/// pinned tag is absent and Docker Hub is unreachable.
const LOCAL_BUILDKIT: &str = "moby/buildkit:rootless";

/// Whether the local Docker daemon has `image`.
pub(crate) fn have(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Make `image` present locally, pulling it when it is not.
pub(crate) fn ensure(image: &str) -> Result<()> {
    if !have(image) {
        run(Command::new("docker").args(["pull", image]))?;
    }
    Ok(())
}

/// Make [`WORKER_IMAGE`] present locally: the pinned tag if Docker has it, else
/// [`LOCAL_BUILDKIT`] retagged, else a pull. A fresh CI runner has neither.
pub(crate) fn ensure_worker() -> Result<()> {
    let _lock = host_lock()?;
    if have(WORKER_IMAGE) {
        return Ok(());
    }
    if have(LOCAL_BUILDKIT) {
        return run(Command::new("docker").args(["tag", LOCAL_BUILDKIT, WORKER_IMAGE]));
    }
    ensure(WORKER_IMAGE)
}

/// Copies busybox and the musl loader out of the worker image into `dir`.
/// The files come out as a tar stream on stdout, not through a bind mount. The
/// rootless image runs as uid 1000, which cannot write to a host directory the
/// runner owns on Linux. Extracted on the host, the files belong to the caller.
pub(crate) fn copy_busybox(dir: &Path) -> Result<()> {
    let mut docker = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "sh",
            WORKER_IMAGE,
            "-c",
            "cd / && tar -chf - bin/busybox lib/ld-musl-*.so.1 lib/libc.musl-*.so.1",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .context("starting docker run for busybox")?;
    let stream = docker.stdout.take().context("docker stdout")?;
    let untar = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(dir)
        .stdin(stream)
        .status()
        .context("running tar")?;
    let copied = docker.wait().context("waiting for docker run")?;
    ensure!(
        copied.success(),
        "copying busybox out of {WORKER_IMAGE} failed: {copied}"
    );
    ensure!(
        untar.success(),
        "extracting busybox into {} failed: {untar}",
        dir.display()
    );
    ensure!(
        dir.join("bin/busybox").is_file(),
        "busybox missing after the copy"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_linux_amd64_hosts_compile_natively() {
        assert!(native_host("linux", "x86_64"));
        assert!(!native_host("linux", "aarch64"));
        assert!(!native_host("macos", "x86_64"));
        assert!(!native_host("macos", "aarch64"));
    }

    /// The image is rebuilt exactly when a file it is built from changes.
    #[test]
    fn inputs_hash_follows_sources_and_ignores_tests() {
        let root = std::env::temp_dir().join(format!("xtask-inputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        for f in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "xtask/Cargo.toml",
            "xtask/src/image.rs",
        ] {
            write(f, "x");
        }
        write("docker/Dockerfile", "FROM scratch");
        write("crates/server/src/main.rs", "fn main() {}");
        write("crates/server/tests/kind.rs", "a");
        let base = inputs_hash(&root, false).unwrap();
        assert_eq!(base, inputs_hash(&root, false).unwrap(), "stable");
        write("crates/server/tests/kind.rs", "b");
        assert_eq!(
            base,
            inputs_hash(&root, false).unwrap(),
            "tests are not inputs"
        );
        write("crates/server/src/main.rs", "fn main() { }");
        let changed = inputs_hash(&root, false).unwrap();
        assert_ne!(base, changed, "a source edit rebuilds");
        write("crates/server/src/new.rs", "");
        assert_ne!(
            changed,
            inputs_hash(&root, false).unwrap(),
            "a new file rebuilds"
        );
        let before = inputs_hash(&root, false).unwrap();
        assert_ne!(
            inputs_hash(&root, false).unwrap(),
            inputs_hash(&root, true).unwrap(),
            "BK_IMAGE_PREINSTALLED rebuilds"
        );
        write("xtask/src/image.rs", "flags");
        assert_ne!(
            before,
            inputs_hash(&root, false).unwrap(),
            "image.rs is an input"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression (soak): one shared volume let another worktree's newer
    /// build make this checkout's edits look up to date.
    #[test]
    fn target_volume_is_per_checkout_and_stable() {
        let a = target_volume(Path::new("/w/a"));
        assert_eq!(a, target_volume(Path::new("/w/a")));
        assert_ne!(a, target_volume(Path::new("/w/b")));
        assert!(a.starts_with(TARGET_VOLUME));
    }

    #[test]
    fn prune_selects_orphans_and_the_legacy_volume_only() {
        let live = std::env::temp_dir();
        let gone = live.join("bk-prune-no-such-checkout");
        let v = |n: &str, l: &Path| (format!("{TARGET_VOLUME}{n}"), l.display().to_string());
        let vols = vec![
            (TARGET_VOLUME.to_owned(), String::new()),
            v("-aaaa", &live),
            v("-bbbb", &gone),
            v("-cccc", Path::new("")),
            ("other-volume".into(), gone.display().to_string()),
        ];
        let got = prunable(&vols, Path::exists);
        assert_eq!(
            got,
            vec![TARGET_VOLUME.to_owned(), format!("{TARGET_VOLUME}-bbbb")]
        );
    }

    #[test]
    fn rust_image_follows_channel() {
        let toml = "[toolchain]\nchannel = \"1.96.0\"\n";
        let pin = "rust:1.96-trixie@sha256:abc\n";
        assert_eq!(
            rust_image(toml, pin).ok().as_deref(),
            Some("rust:1.96-trixie@sha256:abc")
        );
        assert!(rust_image("[toolchain]\n", pin).is_err());
        assert!(rust_image("channel = \"stable\"\n", pin).is_err());
        // stale pin after a toolchain bump, bookworm, or an unpinned tag
        assert!(rust_image("channel = \"1.97.0\"\n", pin).is_err());
        assert!(rust_image(toml, "rust:1.96-bookworm@sha256:abc").is_err());
        assert!(rust_image(toml, "rust:1.96-trixie").is_err());
    }

    /// A toolchain bump without a matching `docker/builder-image` bump fails here.
    #[test]
    fn repo_builder_pin_matches_toolchain() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let read = |p: &str| std::fs::read_to_string(root.join(p));
        let toolchain = read("rust-toolchain.toml").unwrap_or_default();
        let pinned = read("docker/builder-image").unwrap_or_default();
        assert!(
            rust_image(&toolchain, &pinned).is_ok(),
            "stale docker/builder-image"
        );
    }
}
