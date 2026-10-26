use std::io;
use std::process::ExitStatus;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// A language server and the processes it starts in its process group. On
/// unix the server leads a session of its own, and so a group, and stopping
/// or dropping it stops the group: a server signalled alone leaves what it
/// ran still running, as rust-analyzer leaves the `cargo check` it started,
/// whether it was killed or exited when asked. A process it starts in a
/// session of its own, as rust-analyzer starts its check on save, only the
/// server stops, which it does when asked to exit
/// (`LspService::shutdown`). Out of Symora's group and terminal, a server
/// is reached by no signal sent to either: a signal Symora does not handle,
/// such as SIGKILL or SIGQUIT, ends Symora alone, and the server ends as its
/// input closes, leaving what it started; and a process it starts that
/// opens the terminal finds none.
pub(super) struct ServerProcess {
    child: Child,
    /// The group to stop, held only until the server is reaped: past that
    /// its id can be given to another process.
    #[cfg(unix)]
    group: Option<libc::pid_t>,
}

impl ServerProcess {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        // SAFETY: `setsid` is async-signal-safe, which `pre_exec` requires.
        unsafe {
            command.pre_exec(|| match libc::setsid() {
                -1 => Err(io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        let child = command.kill_on_drop(true).spawn()?;
        Ok(Self {
            #[cfg(unix)]
            group: child.id().and_then(|id| libc::pid_t::try_from(id).ok()),
            child,
        })
    }

    pub(super) fn take_stdio(
        &mut self,
    ) -> (Option<ChildStdin>, Option<ChildStdout>, Option<ChildStderr>) {
        (
            self.child.stdin.take(),
            self.child.stdout.take(),
            self.child.stderr.take(),
        )
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait();
        if matches!(status, Ok(Some(_))) {
            self.stop_group();
        }
        status
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await;
        self.stop_group();
        status
    }

    pub(super) fn start_kill(&mut self) {
        self.stop_group();
        let _ = self.child.start_kill();
    }

    pub(super) async fn kill(&mut self) -> io::Result<()> {
        self.start_kill();
        self.wait().await.map(drop)
    }

    fn stop_group(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            // SAFETY: `killpg` takes plain integers and touches no memory.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop_group();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    use super::ServerProcess;

    /// A server that starts `sleep` and prints its pid, then runs `then`.
    async fn server_with_a_child(then: &str) -> (ServerProcess, libc::pid_t) {
        let mut server = ServerProcess::spawn(
            Command::new("sh")
                .arg("-c")
                .arg(format!("sleep 30 & echo $!; {then}"))
                .stdout(Stdio::piped()),
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(server.take_stdio().1.unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        (server, line.trim().parse().unwrap())
    }

    async fn stops(pid: libc::pid_t) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only asks whether the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            if Instant::now() > deadline {
                // SAFETY: as above; the pid is the test's own `sleep`.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    #[tokio::test]
    async fn a_server_that_exits_leaves_nothing_it_started_running() {
        let (mut server, child) = server_with_a_child("exit 0").await;
        server.wait().await.unwrap();
        assert!(stops(child).await, "pid {child} outlived the server");
    }

    #[tokio::test]
    async fn a_server_seen_to_have_exited_leaves_nothing_it_started_running() {
        let (mut server, child) = server_with_a_child("exit 0").await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.try_wait().unwrap().is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(stops(child).await, "pid {child} outlived the server");
    }

    #[tokio::test]
    async fn a_killed_server_takes_what_it_started_with_it() {
        let (mut server, child) = server_with_a_child("wait").await;
        server.kill().await.unwrap();
        assert!(stops(child).await, "pid {child} outlived the server");
    }

    #[tokio::test]
    async fn a_server_leads_a_session_of_its_own() {
        let (mut server, child) = server_with_a_child("wait").await;
        let leader = server
            .child
            .id()
            .and_then(|id| libc::pid_t::try_from(id).ok());
        // SAFETY: `getsid` only reads the session of a pid this test started.
        let sessions = unsafe { (libc::getsid(leader.unwrap()), libc::getsid(child)) };
        server.kill().await.unwrap();
        assert!(stops(child).await, "pid {child} outlived the server");
        assert_eq!(sessions, (leader.unwrap(), leader.unwrap()));
    }

    #[tokio::test]
    async fn a_dropped_server_takes_what_it_started_with_it() {
        let (server, child) = server_with_a_child("wait").await;
        drop(server);
        assert!(stops(child).await, "pid {child} outlived the server");
    }
}
