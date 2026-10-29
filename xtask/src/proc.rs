//! Process helpers shared by the tasks: running commands, polling, child
//! processes that die with the task, and cleanup on SIGINT and SIGTERM.

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, waitid};

/// Runs `c` and fails when it exits non-zero. Output goes to the terminal.
pub(crate) fn status(c: &mut Command) -> Result<()> {
    let t = Timed::start(c);
    let st = Spawned::spawn(c)?.wait()?;
    drop(t);
    ensure!(st.success(), "{c:?} failed: {st}");
    Ok(())
}

/// Stdout and stderr of `c`; fails with both when it exits non-zero.
pub(crate) fn output(c: &mut Command) -> Result<String> {
    let o = run(c).with_context(|| format!("{c:?}"))?;
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
    let o = run(c).with_context(|| format!("{c:?}"))?;
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

/// Process groups whose leader is a [`Spawned`] child that has not been reaped.
///
/// An unreaped leader, running or a zombie, keeps its pid, so its group id
/// cannot be reused: signalling a group in this list never reaches another
/// process. Every reap of a leader happens with this lock held and removes
/// the group, and the signal handler holds it while it signals.
static GROUPS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn groups() -> std::sync::MutexGuard<'static, Vec<u32>> {
    GROUPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sends `sig` to the process group `pgid`; whether any member got it.
fn signal_group(pgid: u32, sig: Option<Signal>) -> bool {
    let Some(group) = i32::try_from(pgid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    match sig {
        Some(sig) => rustix::process::kill_process_group(group, sig).is_ok(),
        None => rustix::process::test_kill_process_group(group).is_ok(),
    }
}

/// Whether the group `pgid`, whose leader `Spawned` still holds unreaped, has
/// nothing left to wait for. A leader that exited on SIGTERM stays a zombie
/// while the caller holds the [`GROUPS`] lock, and a zombie keeps the group
/// visible to `kill(-pgid, 0)`, so the leader is checked with `waitid` without
/// reaping it, and the other members with `ps`, which marks zombies `Z`.
fn group_done(pgid: u32) -> bool {
    use std::os::unix::process::CommandExt as _;
    if !signal_group(pgid, None) {
        return true;
    }
    let Some(leader) = i32::try_from(pgid).ok().and_then(Pid::from_raw) else {
        return true;
    };
    let opts = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
    if !matches!(waitid(WaitId::Pid(leader), opts), Ok(Some(_))) {
        return false;
    }
    // Own process group, so a terminal Ctrl-C cannot kill `ps` mid-check.
    let Ok(ps) = Command::new("ps")
        .args(["-A", "-o", "pgid=,stat="])
        .stdin(Stdio::null())
        .process_group(0)
        .output()
    else {
        return false;
    };
    !ps_has_live_member(&ps, pgid)
}

/// Whether `ps` output lists a non-zombie member of group `pgid`. A `ps` that
/// failed says nothing, so it counts as a live member and the wait goes on.
fn ps_has_live_member(ps: &Output, pgid: u32) -> bool {
    !ps.status.success()
        || String::from_utf8_lossy(&ps.stdout).lines().any(|l| {
            let mut f = l.split_whitespace();
            f.next().and_then(|g| g.parse::<u32>().ok()) == Some(pgid)
                && f.next().is_some_and(|s| !s.starts_with('Z'))
        })
}

/// SIGTERM to every group in `pgids`, then SIGKILL to any still alive after 5 s,
/// so no child outlives the task. A controller replica drains on SIGTERM and
/// could otherwise keep running after its cluster is gone. The caller holds
/// the [`GROUPS`] lock, so none of these leaders is reaped meanwhile; the wait
/// ends as soon as every group is [`group_done`].
fn stop_groups(pgids: &[u32]) {
    for pgid in pgids {
        signal_group(*pgid, Some(Signal::TERM));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pgids.iter().all(|p| group_done(*p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    for pgid in pgids {
        signal_group(*pgid, Some(Signal::KILL));
    }
}

/// Prints how long a slow harness step took, as `timing: <step> <secs>s` on
/// stderr, when dropped. Only the steps that dominate a suite's wall time are
/// reported: cluster create and delete, image loads, chart installs and builds.
pub(crate) struct Timed {
    label: String,
    start: Instant,
}

impl Timed {
    /// A timer named `label`.
    pub(crate) fn named(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            start: Instant::now(),
        }
    }

    /// A timer for `c` when it is one of the reported steps.
    pub(crate) fn start(c: &Command) -> Option<Self> {
        step(c).map(Self::named)
    }
}

impl Drop for Timed {
    fn drop(&mut self) {
        eprintln!(
            "timing: {} {:.1}s",
            self.label,
            self.start.elapsed().as_secs_f64()
        );
    }
}

/// The step `c` performs, if it is one [`Timed`] reports.
fn step(c: &Command) -> Option<String> {
    let program = c.get_program().to_str()?;
    let args: Vec<&str> = c.get_args().filter_map(|a| a.to_str()).collect();
    let has = |seq: &[&str]| args.windows(seq.len()).any(|w| w == seq);
    let label = match program {
        "kind" if has(&["create", "cluster"]) => "kind create".to_owned(),
        "kind" if has(&["delete", "cluster"]) => "kind delete".to_owned(),
        "kind" if has(&["load", "docker-image"]) => {
            let image = args.iter().skip_while(|a| **a != "docker-image").nth(1)?;
            format!("kind load {image}")
        }
        "helm" if args.iter().any(|a| *a == "install" || *a == "upgrade") => {
            "helm install".to_owned()
        }
        "cargo" => format!("cargo {}", args.iter().find(|a| !a.starts_with('-'))?),
        "docker" if has(&["build"]) => "docker build".to_owned(),
        _ => return None,
    };
    Some(label)
}

/// Runs `c` in its own process group, like [`Command::output`]: stdin empty,
/// stdout and stderr captured. SIGINT or SIGTERM to the task stops it too.
pub(crate) fn run(c: &mut Command) -> Result<Output> {
    let _t = Timed::start(c);
    Spawned::spawn(
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?
    .wait_with_output()
}

/// A child process in its own process group, the group leader.
///
/// When the leader exits, waiting signals what is left of the group before it
/// reaps the leader. Dropping it kills the group. Either way nothing it started
/// (buildx, curl, kubectl exec) outlives it, and no signal is sent to a group
/// id after it could have been reused.
pub(crate) struct Spawned {
    child: Child,
    reaped: Option<ExitStatus>,
}

impl Spawned {
    pub(crate) fn spawn(c: &mut Command) -> Result<Self> {
        use std::os::unix::process::CommandExt as _;
        on_signal();
        let mut groups = groups();
        let child = c
            .process_group(0)
            .spawn()
            .with_context(|| format!("spawning {c:?}"))?;
        groups.push(child.id());
        Ok(Self {
            child,
            reaped: None,
        })
    }

    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// SIGKILL to the leader alone, which runs no cleanup. It stays unreaped
    /// until a wait or the drop.
    pub(crate) fn kill_leader(&mut self) -> Result<()> {
        if self.reaped.is_none() {
            self.child.kill()?;
        }
        Ok(())
    }

    /// Blocks until the leader exits, then reaps it.
    pub(crate) fn wait(&mut self) -> Result<ExitStatus> {
        loop {
            if let Some(s) = self.reap(WaitIdOptions::empty())? {
                return Ok(s);
            }
        }
    }

    /// Reaps the leader if it has exited.
    pub(crate) fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.reap(WaitIdOptions::NOHANG)
    }

    /// Reads stdout and stderr to the end and waits.
    pub(crate) fn wait_with_output(mut self) -> Result<Output> {
        fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut p) = pipe {
                    let _ = p.read_to_end(&mut buf);
                }
                buf
            })
        }
        let out = drain(self.child.stdout.take());
        let err = drain(self.child.stderr.take());
        let status = self.wait()?;
        Ok(Output {
            status,
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
        })
    }

    /// Waits (blocking unless `extra` has `NOHANG`) for the leader to exit
    /// without reaping it, then signals the rest of its group and reaps it.
    fn reap(&mut self, extra: WaitIdOptions) -> Result<Option<ExitStatus>> {
        if let Some(s) = self.reaped {
            return Ok(Some(s));
        }
        let pid = Pid::from_child(&self.child);
        let opts = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | extra;
        let exited = loop {
            match waitid(WaitId::Pid(pid), opts) {
                Ok(s) => break s.is_some(),
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e).context("waitid"),
            }
        };
        if !exited {
            return Ok(None);
        }
        let mut groups = groups();
        // The leader is a zombie: its pid, and so the group id, are still ours.
        signal_group(self.child.id(), Some(Signal::TERM));
        let status = self.child.wait()?;
        groups.retain(|p| *p != self.child.id());
        self.reaped = Some(status);
        Ok(Some(status))
    }

    /// Kills the group and reaps the leader, unless it was reaped already.
    pub(crate) fn kill(&mut self) {
        if self.reaped.is_some() {
            return;
        }
        let mut groups = groups();
        signal_group(self.child.id(), Some(Signal::TERM));
        let _ = self.child.kill();
        if let Ok(s) = self.child.wait() {
            self.reaped = Some(s);
        }
        groups.retain(|p| *p != self.child.id());
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Installs, once, a SIGINT and SIGTERM handler that stops every [`Spawned`]
/// group, deletes every live [`crate::kube::Cluster`] and exits with 130.
/// Without it, Ctrl-C or a CI timeout would skip the `Drop` guards. Children
/// are stopped first, so a `kind create` in flight cannot outlast the delete.
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
            // Held until exit: no leader is reaped, and no new child starts.
            let groups = groups();
            stop_groups(&groups);
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

    fn alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid.trim()])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Reaping the leader first signals its leftover group members, then drops
    /// the group from GROUPS, so no later signal can hit a reused group id.
    #[test]
    fn reaping_the_leader_stops_its_group_and_forgets_it() {
        let dir = std::env::temp_dir().join(format!("xtask-reap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("grandchild");
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(format!("sleep 60 & echo $! > {}", pidfile.display()));
        let mut s = Spawned::spawn(&mut c).unwrap();
        let id = s.id();
        assert!(groups().contains(&id));
        let st = poll(
            "leader exit",
            Duration::from_secs(5),
            Duration::from_millis(20),
            || s.try_wait(),
        )
        .unwrap();
        assert!(st.success());
        assert!(!groups().contains(&id), "reaped group still listed");
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        poll(
            "grandchild gone",
            Duration::from_secs(5),
            Duration::from_millis(20),
            || Ok((!alive(&pid)).then_some(())),
        )
        .unwrap();
        // Waiting again returns the same status and sends nothing.
        assert_eq!(s.wait().unwrap(), st);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Leaders that exit on SIGTERM stay unreaped zombies here, as in the signal
    /// handler. The wait must end at once, not run to the 5 s cap.
    #[test]
    fn stop_groups_returns_promptly_when_every_child_exits() {
        let kids: Vec<Spawned> = (0..3)
            .map(|_| Spawned::spawn(Command::new("sleep").arg("60")).unwrap())
            .collect();
        let ids: Vec<u32> = kids.iter().map(Spawned::id).collect();
        let start = Instant::now();
        stop_groups(&ids);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn ps_output_decides_live_members() {
        use std::os::unix::process::ExitStatusExt as _;
        let out = |code, text: &str| Output {
            status: ExitStatus::from_raw(code),
            stdout: text.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        let table = " 42 S\n 42 Z\n 7 R+\n";
        assert!(ps_has_live_member(&out(0, table), 42));
        assert!(!ps_has_live_member(&out(0, " 42 Z\n 7 R+\n"), 42));
        assert!(!ps_has_live_member(&out(0, ""), 42));
        // A failed ps (nonzero exit, or killed) must not read as "no members".
        assert!(ps_has_live_member(&out(1 << 8, ""), 42));
        assert!(ps_has_live_member(&out(2, ""), 42));
    }

    /// A grandchild holding the pipe open does not keep `run` waiting.
    #[test]
    fn run_returns_when_the_leader_exits() {
        let start = Instant::now();
        let o = run(Command::new("sh").args(["-c", "sleep 60 & echo hi"])).unwrap();
        assert!(o.status.success());
        assert_eq!(String::from_utf8_lossy(&o.stdout), "hi\n");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
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
