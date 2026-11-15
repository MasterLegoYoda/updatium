//! Post-install health checks for trusted application configuration.
//!
//! Commands run directly as argv, not through a shell. The existing probe
//! executor limits runtime to three seconds and bounds both output streams;
//! only the direct child is terminated on timeout or cancellation.

use anyhow::{bail, Result};

/// Run a post-install check. A zero exit status means the check passed.
/// Output and command arguments are never included in the returned error,
/// because either can contain secrets.
pub async fn run(cmd: &[String]) -> Result<()> {
    if cmd.first().map_or(true, |executable| executable.trim().is_empty()) {
        bail!("health check requires a nonempty executable");
    }
    match crate::exec::run_probe(cmd).await {
        Ok(_) => Ok(()),
        Err(err) => {
            let message = err.to_string();
            if message.starts_with("probe timed out") {
                bail!("health check timed out after 3 seconds");
            }
            if message.starts_with("probe exited with ") {
                bail!("health check exited unsuccessfully");
            }
            if message.starts_with("probe stdout exceeded") || message.starts_with("probe stderr exceeded") {
                bail!("health check exceeded the output limit");
            }
            bail!("health check could not be completed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_exit_status_and_redact_output() {
        assert!(run(&["true".into()]).await.is_ok());
        let error = run(&["sh".into(), "-c".into(), "echo secret >&2; exit 1".into()])
            .await.unwrap_err().to_string();
        assert!(error.contains("unsuccessfully"));
        assert!(!error.contains("secret"));
        assert!(run(&[]).await.is_err());
        assert!(run(&[" ".into()]).await.is_err());
    }

    #[tokio::test]
    async fn check_redacts_spawn_errors() {
        let error = run(&["/nonexistent/secret-executable".into()]).await.unwrap_err().to_string();
        assert!(!error.contains("secret"));
    }
}
