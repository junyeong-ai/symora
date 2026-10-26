/// Resolves once the user or the system asks this process to stop: SIGTERM,
/// SIGINT, or a hangup unless the process was started with hangups ignored,
/// as `nohup` starts it; Ctrl-C where there are no Unix signals.
pub async fn stop_requested() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        let mut sighup =
            (!hangup_ignored()).then(|| signal(SignalKind::hangup()).expect("SIGHUP handler"));
        let hangup = async {
            match sighup.as_mut() {
                Some(sighup) => sighup.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
            _ = hangup => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
    }
}

#[cfg(unix)]
fn hangup_ignored() -> bool {
    // SAFETY: a null new action only reads the current one into `current`.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}
