//! provider = "command": self-managed version managers (nvm/pyenv style).
//! `check_cmd` prints newline-separated versions on stdout; order preserved
//! (the engine re-sorts). `apply_cmd` is executed by the engine, not here.

use super::{Provider, Release};
use crate::config::{AppSource, SourceKind};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::time::Duration;

pub struct Command;

#[async_trait]
impl Provider for Command {
    fn id(&self) -> &'static str {
        "command"
    }

    async fn check(&self, app: &AppSource, _http: &reqwest::Client) -> Result<Vec<Release>> {
        let cfg = match &app.kind {
            SourceKind::Command(c) => c.clone(),
            _ => bail!("provider mismatch: command got another app kind"),
        };
        let out = crate::exec::run(&cfg.check_cmd, None, Duration::from_secs(120), true)
            .await
            .context("running check_cmd")?;
        anyhow::ensure!(
            out.status,
            "check_cmd failed ({}):\n{}",
            cfg.check_cmd.join(" "),
            out.stderr_tail
        );
        Ok(out
            .stdout_tail
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|v| Release {
                version: v.to_string(),
                published_at: None,
                assets: vec![],
            })
            .collect())
    }
}
