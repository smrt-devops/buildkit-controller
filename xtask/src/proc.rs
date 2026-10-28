//! Process helpers shared by the tasks: running commands, polling, child
//! processes that die with the task, and cleanup on SIGINT and SIGTERM.

use std::path::Path;
use std::process::{Child, Command, Output};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

/// Runs `c` and fails when it exits non-zero. Output goes to the terminal.
pub(crate) fn status(c: &mut Command) -> Result<()> {
    ensure!(
        c.status().with_context(|| format!("{c:?}"))?.success(),
        "{c:?} failed"
    );
    Ok(())
}

/// Stdout and stderr of `c`; fails with both when it exits non-zero.
pub(crate) fn output(c: &mut Command) -> Result<String> {
    let o = c.output().with_context(|| format!("{c:?}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    ensure!(o.status.success(), "{c:?} failed:\n{text}");
    Ok(text)
}

/// Stdout of `c`; fails with stdout and stderr when it exits non-zero.
pub(crate) fn stdout(c: &mut Command) -> Result<String> {
    let o = c.output().with_context(|| format!("{c:?}"))?;
    ensure!(
        o.status.success(),
        "{c:?} failed:\n{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Writes `text` to `path` and returns the path as a string.
pub(crate) fn write(path: &Path, text: &str) -> Result<String> {
    std::fs::write(path, text).with_context(|| path.display().to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

/// The pool platform of this host.
pub(crate) fn host_platform() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("amd64"),
        a => bail!("unsupported host architecture {a}"),
    }
}

/// `N` distinct free loopback ports. All are bound at once, so none repeats;
/// another process may still take one before the caller binds it.
pub(crate) fn free_ports<const N: usize>() -> Result<[u16; N]> {
    let listeners = (0..N)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0"))
        .collect::<std::io::Result<Vec<_>>>()?;
    let ports = listeners
        .iter()
        .map(|l| l.local_addr().map(|a| a.port()))
        .collect::<std::io::Result<Vec<_>>>()?;
    ports
        .try_into()
        .map_err(|_| anyhow::anyhow!("free port count"))
}

/// Calls `f` every `interval` until it returns `Some`, and fails after `timeout`
/// naming `what` (and the last error, if `f` was failing).
pub(crate) fn poll<T>(
    what: &str,
    timeout: Duration,
    interval: Duration,
    mut f: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    let start = Instant::now();
    loop {
        match f() {
            Ok(Some(v)) => return Ok(v),
            Err(e) if start.elapsed() > timeout => bail!("{what}: {e:#}"),
            Ok(None) | Err(_) => {}
        }
        ensure!(
            start.elapsed() <= timeout,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(interval);
    }
}

/// [`poll`] every 3 s, for cluster state.
pub(crate) fn until<T>(
    what: &str,
    timeout: Duration,
    f: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    poll(what, timeout, Duration::from_secs(3), f)
}

/// Process groups of live [`Spawned`] children, killed on SIGINT or SIGTERM.
static GROUPS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn signal_group(pid: u32, signal: &str) -> bool {
    Command::new("kill")
        .args(["-s", signal, &format!("-{pid}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn kill_group(pid: u32) {
    signal_group(pid, "TERM");
}

/// SIGTERM to every group in `pids`, then SIGKILL to any still alive after 5 s,
/// so no child outlives the task. A controller replica drains on SIGTERM and
/// could otherwise keep running after its cluster is gone.
fn stop_groups(pids: &[u32]) {
    for pid in pids {
        kill_group(*pid);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while pids.iter().any(|p| signal_group(*p, "0")) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    for pid in pids {
        signal_group(*pid, "KILL");
    }
}

/// A child process in its own process group. Dropping it kills the group, so
/// nothing it started (buildx, curl, kubectl exec) outlives an error return.
pub(crate) struct Spawned(Option<Child>);

impl Spawned {
    pub(crate) fn spawn(c: &mut Command) -> Result<Self> {
        use std::os::unix::process::CommandExt as _;
        on_signal();
        let child = c
            .process_group(0)
            .spawn()
            .with_context(|| format!("spawning {c:?}"))?;
        if let Ok(mut g) = GROUPS.lock() {
            g.push(child.id());
        }
        Ok(Self(Some(child)))
    }

    /// The running child. Present until [`Spawned::wait_with_output`].
    pub(crate) fn child(&mut self) -> Result<&mut Child> {
        self.0.as_mut().context("child already reaped")
    }

    pub(crate) fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child()?.wait()?)
    }

    pub(crate) fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child()?.try_wait()?)
    }

    pub(crate) fn wait_with_output(mut self) -> Result<Output> {
        let child = self.0.take().context("child already reaped")?;
        forget_group(child.id());
        Ok(child.wait_with_output()?)
    }

    /// Kills the process group now.
    pub(crate) fn kill(&mut self) {
        if let Some(mut c) = self.0.take() {
            kill_group(c.id());
            let _ = c.kill();
            let _ = c.wait();
            forget_group(c.id());
        }
    }
}

fn forget_group(pid: u32) {
    if let Ok(mut g) = GROUPS.lock() {
        g.retain(|p| *p != pid);
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Installs, once, a SIGINT and SIGTERM handler that kills every [`Spawned`]
/// group, deletes every live [`crate::kube::Cluster`] and exits with 130.
/// Without it, Ctrl-C or a CI timeout would skip the `Drop` guards.
pub(crate) fn on_signal() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
            else {
                return;
            };
            let got = rt.block_on(async {
                use tokio::signal::unix::{SignalKind, signal};
                let mut term = signal(SignalKind::terminate()).ok()?;
                let mut int = signal(SignalKind::interrupt()).ok()?;
                tokio::select! {
                    _ = term.recv() => Some("SIGTERM"),
                    _ = int.recv() => Some("SIGINT"),
                }
            });
            let Some(sig) = got else { return };
            eprintln!("{sig}: cleaning up");
            stop_groups(&GROUPS.lock().map(|g| g.clone()).unwrap_or_default());
            crate::kube::delete_live_clusters();
            std::process::exit(130);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_returns_the_first_value_and_times_out_with_the_name() {
        let mut n = 0;
        let v = poll("three", Duration::from_secs(5), Duration::ZERO, || {
            n += 1;
            Ok((n == 3).then_some(n))
        })
        .unwrap();
        assert_eq!(v, 3);
        let e = poll::<()>("never", Duration::ZERO, Duration::ZERO, || Ok(None)).unwrap_err();
        assert!(e.to_string().contains("never"), "{e}");
        let e = poll::<()>("broken", Duration::ZERO, Duration::ZERO, || bail!("boom")).unwrap_err();
        assert!(format!("{e:#}").contains("boom"), "{e:#}");
    }

    #[test]
    fn dropping_a_spawned_child_kills_its_whole_group() {
        let dir = std::env::temp_dir().join(format!("xtask-spawned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("grandchild");
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(format!("sleep 60 & echo $! > {}; wait", pidfile.display()));
        let s = Spawned::spawn(&mut c).unwrap();
        let pid = poll(
            "grandchild pid",
            Duration::from_secs(5),
            Duration::from_millis(20),
            || {
                Ok(std::fs::read_to_string(&pidfile)
                    .ok()
                    .filter(|s| s.ends_with('\n')))
            },
        )
        .unwrap();
        drop(s);
        let alive = || {
            Command::new("kill")
                .args(["-0", pid.trim()])
                .output()
                .is_ok_and(|o| o.status.success())
        };
        poll(
            "grandchild gone",
            Duration::from_secs(5),
            Duration::from_millis(20),
            || Ok((!alive()).then_some(())),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
