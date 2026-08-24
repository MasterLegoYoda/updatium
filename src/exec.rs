//! Safe subprocess execution: argv templating with an injection firewall.
//!
//! Load-bearing: [`substitute`] validates every interpolated variable value
//! against a fixed safe charset BEFORE splicing it into argv. Combined with
//! direct argv execution (never a shell), this is the boundary between
//! provider-controlled data and process invocation.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Values substituted into `{name}` slots must consist solely of these
/// characters: letters, digits and `._+/=@:-`. Anything else (spaces, `;`,
/// backticks, `$`, quotes, newlines...) is rejected.
fn is_safe_value(v: &str) -> bool {
    !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+/=@:-".contains(&b))
}

/// Replace `{name}` tokens in every argv element using `vars`.
///
/// A token whose name is present in `vars` is replaced after its value passes
/// the safety charset. A token whose name is a bare lowercase identifier NOT
/// present in `vars` is an error (missing context / typo). Anything else in
/// braces (e.g. `{}` or `{not a var}`) passes through untouched.
pub fn substitute(argv: &[String], vars: &BTreeMap<&str, &str>) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(argv.len());
    for arg in argv {
        let mut res = String::with_capacity(arg.len());
        let bytes = arg.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'{' {
                if let Some(end) = arg[i + 1..].find('}') {
                    let name = &arg[i + 1..i + 1 + end];
                    if let Some(val) = vars.get(name) {
                        if !is_safe_value(val) {
                            bail!(
                                "variable '{{{name}}}' has unsafe value {val:?}: allowed \
                                 characters are [A-Za-z0-9._+/=@:-]"
                            );
                        }
                        res.push_str(val);
                        i += end + 2;
                        continue;
                    }
                    // Unknown identifier-shaped token: refuse rather than leak
                    // a literal placeholder into a command line.
                    if !name.is_empty()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    {
                        bail!("unresolved placeholder '{{{name}}}' in argument {arg:?}");
                    }
                }
            }
            let ch_len = utf8_len(bytes[i]);
            res.push_str(&arg[i..i + ch_len]);
            i += ch_len;
        }
        out.push(res);
    }
    Ok(out)
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[derive(Debug, Clone)]
pub struct RunOut {
    /// true when the process exited with code 0
    pub status: bool,
    /// last 200 lines of stdout (empty when capture=false)
    pub stdout_tail: String,
    /// last 200 lines of stderr (empty when capture=false)
    pub stderr_tail: String,
}

const TAIL_LINES: usize = 200;

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    lines[start..].join("\n")
}

/// Expand a leading `~` in each argv element (commands are exec'd without a
/// shell, so nothing else would expand it).
fn expand_argv(argv: &[String]) -> Vec<String> {
    argv.iter()
        .map(|a| {
            crate::config::expand_tilde(a)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Execute `cmd` directly (argv vector, NEVER through a shell).
///
/// With `capture`, stdout/stderr are collected (tails kept, capped at 200
/// lines); otherwise they inherit this process's streams and tails stay empty.
/// The child is killed if it exceeds `timeout`.
pub async fn run(
    cmd: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    capture: bool,
) -> Result<RunOut> {
    run_with_stdin(cmd, cwd, timeout, capture, None).await
}

/// Like [`run`] but pipes `stdin_bytes` to the child when provided.
pub async fn run_with_stdin(
    cmd: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    capture: bool,
    stdin_bytes: Option<&[u8]>,
) -> Result<RunOut> {
    run_impl(cmd, cwd, timeout, capture, stdin_bytes, &[]).await
}

/// Like [`run`] but overlays `env` on top of inherited environment variables.
/// Keys/values must already be safe (config-owned); nothing here shells out.
pub async fn run_env(
    cmd: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    env: &std::collections::BTreeMap<String, String>,
    capture: bool,
) -> Result<RunOut> {
    let env_pairs: Vec<(String, String)> =
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    run_impl(cmd, cwd, timeout, capture, None, &env_pairs).await
}

async fn run_impl(
    cmd: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    capture: bool,
    stdin_bytes: Option<&[u8]>,
    env: &[(String, String)],
) -> Result<RunOut> {
    if cmd.is_empty() {
        bail!("refusing to execute empty command");
    }
    let expanded = expand_argv(cmd);
    let mut command = tokio::process::Command::new(&expanded[0]);
    command.args(&expanded[1..]);
    if stdin_bytes.is_some() {
        command.stdin(std::process::Stdio::piped());
    } else {
        command.stdin(std::process::Stdio::null());
    }
    if capture {
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
    } else {
        command.stdout(std::process::Stdio::inherit());
        command.stderr(std::process::Stdio::inherit());
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    for (k, v) in env {
        command.env(k, v);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", expanded[0]))?;

    // Borrow-based wait future so `child` stays available for start_kill()
    // on the timeout path. Captured pipes are drained concurrently with wait().
    use tokio::io::AsyncReadExt;
    let stdin_data: Option<Vec<u8>> = stdin_bytes.map(|b| b.to_vec());
    let wait_fut = async {
        if let Some(data) = &stdin_data {
            // Write stdin fully, then close so the child sees EOF.
            if let Some(mut pipe) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let write_all = async move {
                    pipe.write_all(data).await?;
                    pipe.shutdown().await
                };
                tokio::pin!(write_all);
                let _ = write_all.await;
            }
        }
        if capture {
            let mut stdout_pipe = child.stdout.take().context("stdout pipe missing")?;
            let mut stderr_pipe = child.stderr.take().context("stderr pipe missing")?;
            let mut so = String::new();
            let mut se = String::new();
            let (r1, r2, r3) = tokio::join!(
                stdout_pipe.read_to_string(&mut so),
                stderr_pipe.read_to_string(&mut se),
                child.wait()
            );
            r1.context("reading stdout")?;
            r2.context("reading stderr")?;
            let status = r3.context("waiting for process")?;
            Ok(RunOut {
                status: status.success(),
                stdout_tail: tail(&so),
                stderr_tail: tail(&se),
            })
        } else {
            let status = child.wait().await.context("waiting for process")?;
            Ok(RunOut {
                status: status.success(),
                stdout_tail: String::new(),
                stderr_tail: String::new(),
            })
        }
    };

    match tokio::time::timeout(timeout, wait_fut).await {
        Ok(res) => res,
        Err(_) => {
            // Best-effort kill; ignore races with an already-exited child.
            let _ = child.start_kill();
            bail!(
                "command timed out after {:?} and was killed: {}",
                timeout,
                expanded.join(" ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &[(&'a str, &'a str)]) -> BTreeMap<&'a str, &'a str> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn substitutes_known_tokens() {
        let v = vars(&[("version", "1.2.3"), ("image", "ghcr.io/x/y")]);
        let argv = vec![
            "docker".to_string(),
            "pull".to_string(),
            "{image}:{version}".to_string(),
        ];
        assert_eq!(
            substitute(&argv, &v).unwrap(),
            vec!["docker", "pull", "ghcr.io/x/y:1.2.3"]
        );
    }

    #[test]
    fn rejects_unsafe_values() {
        for bad in [
            "1.0; rm -rf /",
            "`id`",
            "$(id)",
            "a b",
            "line\nbreak",
            "quote\"d",
            "single'q",
            "$PATH",
            "&bg",
            "|pipe",
            ">out",
            "<in",
        ] {
            let v = vars(&[("version", bad)]);
            let argv = vec!["echo".to_string(), "{version}".to_string()];
            assert!(substitute(&argv, &v).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn rejects_unknown_identifier_placeholder() {
        let argv = vec![
            "nvm".to_string(),
            "install".to_string(),
            "{verion}".to_string(),
        ];
        assert!(substitute(&argv, &BTreeMap::new()).is_err());
        // Non-identifier brace content passes through untouched.
        let argv2 = vec!["printf".to_string(), "{} done".to_string()];
        assert_eq!(
            substitute(&argv2, &BTreeMap::new()).unwrap(),
            vec!["printf", "{} done"]
        );
    }

    #[test]
    fn accepts_full_charset_values() {
        let v = vars(&[("v", "A-Z_a-z0-9.+/@:=-")]);
        let argv = vec!["x{v}y".to_string()];
        assert_eq!(substitute(&argv, &v).unwrap(), vec!["xA-Z_a-z0-9.+/@:=-y"]);
    }

    #[tokio::test]
    async fn run_captures_tails() {
        let cmd = vec![
            "bash".to_string(),
            "-c".to_string(),
            "echo out1; echo out2; echo err1 >&2".to_string(),
        ];
        let o = run(&cmd, None, Duration::from_secs(10), true)
            .await
            .unwrap();
        assert!(o.status);
        assert_eq!(o.stdout_tail, "out1\nout2");
        assert_eq!(o.stderr_tail, "err1");
    }

    #[tokio::test]
    async fn run_enforces_timeout() {
        let cmd = vec!["sleep".to_string(), "5".to_string()];
        let started = std::time::Instant::now();
        let res = run(&cmd, None, Duration::from_millis(100), false).await;
        assert!(res.is_err(), "sleep 5 must hit the 100ms timeout");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn run_reports_failure_status() {
        let cmd = vec!["false".to_string()];
        let o = run(&cmd, None, Duration::from_secs(5), true).await.unwrap();
        assert!(!o.status);
    }

    #[tokio::test]
    async fn run_expands_tilde_in_args() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let cmd = vec!["echo".to_string(), "~/x".to_string()];
        let o = run(&cmd, None, Duration::from_secs(5), true).await.unwrap();
        assert_eq!(o.stdout_tail, format!("{home}/x"));
    }
}
