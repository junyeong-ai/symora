use std::time::Duration;

/// How long a test waits on a process for what it does on its own:
/// starting, recording itself, answering, exiting. A passing run waits only
/// as long as that takes, so the bound is set for a loaded machine, where a
/// process can take seconds to start, rather than for the suite's wall
/// time. It is the start budget a command gives its language server by
/// default, so no test gives up on a start the command it runs would still
/// be waiting for.
pub fn wait() -> Duration {
    Duration::from_secs(
        symora::models::config::SymoraConfig::default()
            .lsp
            .timeout_secs,
    )
}
