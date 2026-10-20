//! Orchestration: check/update pipelines, approval gate, notifications.
//!
//! Load-bearing: `update()` executes the 8-step pipeline in the exact order
//! documented in the design; the script-integrity gate (step 5) stops the
//! pipeline before any build step runs when tracked files changed.

use crate::config::{
    self, AppFile, DigestPolicy, GlobalConfig, HomePaths, InstallType, SourceKind,
};
use crate::exec;
use crate::hash;
use crate::installer::{self, Publication};
use crate::net::{self, DownloadOpts};
use crate::provider::{self, Release};
use crate::store::Store;
use crate::version::cmp_version;
use anyhow::{Context as _, Result, bail};
use futures::stream::StreamExt;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum Status {
    NeverChecked,
    Ok,
    UpdateAvailable,
    NeedsApproval,
    Disabled,
    Error(String),
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::NeverChecked => "· unchecked",
            Status::Ok => "● ok",
            Status::UpdateAvailable => "▲ update",
            Status::NeedsApproval => "⚠ approval",
            Status::Disabled => "○ disabled",
            Status::Error(_) => "✖ error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckReport {
    pub app: String,
    pub provider: String,
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub status: Status,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOpts {
    pub dry_run: bool,
    pub yes: bool,
    pub force: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UpdateReport {
    pub app: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub actions: Vec<String>,
    pub warnings: Vec<String>,
    /// Set when the pipeline stopped with a failure (drives exit code 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

pub struct Engine {
    global: GlobalConfig,
    store: Store,
    http: reqwest::Client,
    registry: BTreeMap<String, Arc<dyn provider::Provider>>,
    installers: std::collections::HashMap<String, std::sync::Arc<dyn crate::installer::Installer>>,
    home: HomePaths,
}

impl Engine {
    pub fn new(home_override: Option<&Path>) -> Result<Engine> {
        let home = HomePaths::resolve(home_override)?;
        let cfg_path = home.config_file();
        let mut global = GlobalConfig::load(&cfg_path)
            .with_context(|| format!("loading {}", cfg_path.display()))?;
        global.home = home.clone();
        let store = Store::open(&home.db_path())?;
        let registry: std::collections::BTreeMap<_, _> =
            provider::registry(&global).into_iter().collect();
        let http = net::build_client()?;
        let installers = crate::installer::registry(&global);
        Ok(Engine {
            global,
            store,
            http,
            registry,
            installers,
            home,
        })
    }

    fn load_apps(&self) -> Result<Vec<AppFile>> {
        config::load_apps(&self.home.apps_d())
    }

    // -----------------------------------------------------------------
    // check
    // -----------------------------------------------------------------

    /// Check one app or all enabled apps. One app's failure never fails the
    /// batch — it lands in that row's `Status::Error`.
    pub async fn check(&self, ids: Option<&[String]>) -> Result<Vec<CheckReport>> {
        let apps = self.resolve_apps(ids)?;

        // DB bookkeeping for every known app (insert-missing semantics).
        self.store.sync_apps(
            &apps
                .iter()
                .map(|a| (a.id.as_str(), a.provider.as_str(), a.enabled))
                .collect::<Vec<_>>(),
        )?;

        let parallel = self.global.max_parallel_checks.clamp(1, 32) as usize;

        // Phase 1: concurrent provider checks.
        let fetched: Vec<(AppFile, Result<Vec<Release>>)> =
            futures::stream::iter(apps.into_iter().map(|app| {
                let http = &self.http;
                async move {
                    if !app.enabled {
                        return (app, Ok(Vec::new()));
                    }
                    let src = app.app_source();
                    let res = match provider::lookup_btree(&self.registry, &app.provider) {
                        Ok(p) => p.check(&src, http).await,
                        Err(e) => Err(e),
                    };
                    (app, res)
                }
            }))
            .buffer_unordered(parallel)
            .collect()
            .await;

        // Phase 2: sequential state writes + status computation.
        let mut reports = Vec::with_capacity(fetched.len());
        for (app, res) in fetched {
            let installed = self.installed_for(&app);
            let pending = self.is_pending_approval(&app.id).unwrap_or(false);

            match res {
                Err(e) => {
                    let msg = format!("{e:#}");
                    let _ = self.store.touch_check(&app.id, Some(&msg));
                    let _ = self.store.event("check_error", Some(&app.id), Some(&msg));
                    reports.push(CheckReport {
                        app: app.id,
                        provider: app.provider,
                        installed,
                        latest: None,
                        status: Status::Error(msg),
                    });
                }
                Ok(mut releases) => {
                    releases.sort_by(|a, b| cmp_version(&b.version, &a.version));
                    releases.dedup_by(|a, b| a.version == b.version);
                    let versions: Vec<String> =
                        releases.iter().map(|r| r.version.clone()).collect();
                    let _ = self.store.record_versions(&app.id, &versions);
                    let _ = self.store.touch_check(&app.id, None);
                    let _ = self.store.event(
                        "check_ok",
                        Some(&app.id),
                        Some(&serde_json::json!({ "count": versions.len() }).to_string()),
                    );

                    // "Latest" is the newest version the provider advertises
                    // right now, not the all-time maximum ever recorded: config
                    // filters (tag_regex, prereleases, ref_regex) can remove a
                    // previously seen version from the current result set.
                    let latest = versions.first().cloned();
                    let status = if pending {
                        Status::NeedsApproval
                    } else {
                        match (&installed, &latest) {
                            (_, None) => Status::NeverChecked,
                            (None, Some(_)) => Status::NeverChecked,
                            (Some(i), Some(l)) => match cmp_version(l, i) {
                                std::cmp::Ordering::Greater => {
                                    let _ = self.store.event(
                                        "update_available",
                                        Some(&app.id),
                                        Some(
                                            &serde_json::json!({
                                                "installed": i, "latest": l
                                            })
                                            .to_string(),
                                        ),
                                    );
                                    Status::UpdateAvailable
                                }
                                _ => Status::Ok,
                            },
                        }
                    };
                    reports.push(CheckReport {
                        app: app.id,
                        provider: app.provider,
                        installed,
                        latest,
                        status,
                    });
                }
            }
        }
        reports.sort_by(|a, b| a.app.cmp(&b.app));
        Ok(reports)
    }

    fn resolve_apps(&self, ids: Option<&[String]>) -> Result<Vec<AppFile>> {
        let apps = self.load_apps()?;
        match ids {
            None => Ok(apps.into_iter().filter(|a| a.enabled).collect()),
            Some(ids) => {
                for id in ids {
                    if !apps.iter().any(|a| &a.id == id) {
                        bail!(
                            "unknown app '{id}' (add it under {} first)",
                            self.home.apps_d().display()
                        );
                    }
                }
                Ok(apps.into_iter().filter(|a| ids.contains(&a.id)).collect())
            }
        }
    }

    /// App has a script_changed event newer than its last approved event.
    pub fn is_pending_approval(&self, id: &str) -> Result<bool> {
        let mut events = self.store.events(Some(id), 200)?;
        // events arrive newest-first; replay chronologically so the most recent
        // script_changed/approved decides.
        events.reverse();
        let mut pending = false;
        for e in events {
            match e.kind.as_str() {
                "script_changed" => pending = true,
                "approved" => pending = false,
                _ => {}
            }
        }
        Ok(pending)
    }

    pub fn pending_approvals(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for a in self.store.list_apps()? {
            if self.is_pending_approval(&a.id)? {
                out.push(a.id);
            }
        }
        out.sort();
        Ok(out)
    }

    // -----------------------------------------------------------------
    // update — 8-step pipeline
    // -----------------------------------------------------------------

    pub async fn update(&self, id: &str, o: &UpdateOpts) -> Result<UpdateReport> {
        // Step 1: pick target version.
        let apps = self.load_apps()?;
        let app = apps
            .into_iter()
            .find(|a| a.id == id)
            .with_context(|| format!("unknown app '{id}'"))?;
        anyhow::ensure!(app.enabled, "app '{id}' is disabled");
        // Ensure the FK target row exists even when update runs without a prior
        // check.
        self.store
            .sync_apps(&[(app.id.as_str(), app.provider.as_str(), app.enabled)])?;

        let from = self.installed_for(&app);

        let mut releases = self
            .check_one_versions(&app)
            .await
            .with_context(|| format!("checking '{id}'"))?;
        releases.sort_by(|a, b| cmp_version(&b.version, &a.version));
        releases.dedup_by(|a, b| a.version == b.version);

        let target: Release = {
            let chosen = match (&from, o.force) {
                // Forced run always takes the newest known release.
                (_, true) => releases.first().cloned(),
                // Nothing recorded yet: latest.
                (None, _) => releases.first().cloned(),
                // Otherwise only strictly newer than installed.
                (Some(i), _) => releases
                    .iter()
                    .find(|r| cmp_version(&r.version, i) == std::cmp::Ordering::Greater)
                    .cloned(),
            };
            match chosen {
                Some(r) => r,
                None => {
                    return Ok(UpdateReport {
                        app: id.to_string(),
                        from: from.clone(),
                        to: from.clone(),
                        actions: vec!["nothing to do".into()],
                        warnings: vec![],
                        failed: None,
                    });
                }
            }
        };

        let mut rep = UpdateReport {
            app: id.to_string(),
            from,
            to: Some(target.version.clone()),
            actions: vec![],
            warnings: vec![],
            failed: None,
        };

        if o.dry_run {
            // Steps 1-2 only: resolve the asset URL, print plan, mutate nothing.
            let plan_asset = self.primary_asset(&app, &target)?;
            rep.actions.push(format!(
                "would download {}",
                plan_asset
                    .as_ref()
                    .map(|a| a.url.to_string())
                    .unwrap_or_else(|| "(no assets)".into())
            ));
            self.describe_plan(&app, &target, &mut rep.actions);
            return Ok(rep);
        }

        self.store.event(
            "install_started",
            Some(id),
            Some(&serde_json::json!({"to": target.version}).to_string()),
        )?;

        match self.pipeline(&app, &target, &mut rep).await {
            Ok(()) => Ok(rep),
            Err(e) => {
                let msg = format!("{e:#}");
                rep.failed = Some(msg.clone());
                // install_failed already emitted inside pipeline where precise;
                // ensure an event exists even for early failures:
                let _ = self.store.event("install_failed", Some(id), Some(&msg));
                Ok(rep)
            }
        }
    }

    async fn check_one_versions(&self, app: &AppFile) -> Result<Vec<Release>> {
        let p = provider::lookup_btree(&self.registry, &app.provider)?;
        p.check(&app.app_source(), &self.http).await
    }

    fn describe_plan(&self, app: &AppFile, target: &Release, actions: &mut Vec<String>) {
        match &app.source_kind {
            SourceKind::GithubReleases(_) | SourceKind::HtmlLinks(_) => {}
            SourceKind::SourceBuild(sb) => {
                actions.push(format!("clone/fetch {}", sb.repo_url));
                actions.push(format!("checkout --detach {}", target.version));
                actions.push(format!("run {} build steps", sb.build.steps.len()));
            }
            SourceKind::Command(c) => {
                if let Some(apply) = &c.apply_cmd {
                    actions.push(format!("apply via {}", apply.join(" ")));
                }
            }
            SourceKind::DockerRegistry(d) => {
                actions.push(format!("pull {}", d.pull_cmd.join(" ")));
                if let Some(r) = &d.redeploy_cmd {
                    actions.push(format!("redeploy via {}", r.join(" ")));
                }
            }
            SourceKind::Apt(c) => {
                if let Some(apply) = &c.apply_cmd {
                    actions.push(format!("apply via {}", apply.join(" ")));
                }
            }
            SourceKind::Flatpak(f) => {
                if let Some(apply) = &f.apply_cmd {
                    actions.push(format!("apply via {}", apply.join(" ")));
                }
            }
            SourceKind::External { .. } => {}
        }
        match &app.install.kind {
            InstallType::Appimage => actions.push(format!(
                "install AppImage to {}",
                app.install
                    .target
                    .as_deref()
                    .map(crate::config::expand_tilde)
                    .unwrap_or_else(|| self.home.default_appimage_target())
                    .display()
            )),
            InstallType::Archive => actions.push("extract + publish bin_glob".into()),
            InstallType::Dir => actions.push("publish artifacts".into()),
            InstallType::None => {}
            InstallType::External(n) => {
                actions.push(format!("install via external installer '{n}'"));
            }
        }
        if let Some(d) = &app.install.desktop {
            actions.push(format!("write desktop entry '{}'", d.name));
        }
    }

    /// The primary downloadable asset of the target release, when the app type
    /// carries assets. Errors when assets exist but none match `asset_regex`.
    fn primary_asset(&self, app: &AppFile, target: &Release) -> Result<Option<provider::Asset>> {
        let wants_assets = matches!(
            app.source_kind,
            SourceKind::GithubReleases(_)
                | SourceKind::HtmlLinks(_)
                | SourceKind::External { .. }
        );
        if !wants_assets {
            return Ok(None);
        }
        let re = match &app.source_kind {
            SourceKind::GithubReleases(c) => c.asset_regex.as_deref(),
            SourceKind::HtmlLinks(_) => None,
            _ => None,
        };
        let matched: Vec<_> = match re {
            Some(rx) => {
                let rx = regex::Regex::new(rx).context("bad asset_regex")?;
                target
                    .assets
                    .iter()
                    .filter(|a| rx.is_match(&a.name))
                    .collect()
            }
            None => target.assets.iter().collect(),
        };
        match matched.into_iter().next() {
            Some(a) => Ok(Some(a.clone())),
            None if target.assets.is_empty() => bail!(
                "app '{}' expects downloadable assets but release '{}' had none",
                app.id,
                target.version
            ),
            None => bail!(
                "no asset of release '{}' matches asset_regex for app '{}'",
                target.version,
                app.id
            ),
        }
    }

    async fn pipeline(
        &self,
        app: &AppFile,
        target: &Release,
        rep: &mut UpdateReport,
    ) -> Result<()> {
        match &app.source_kind {
            SourceKind::SourceBuild(sb) => {
                // Steps 5-7: no downloadable assets; checkout + gate + build.
                return self.build_and_install_source(app, sb, target, rep).await;
            }
            SourceKind::Command(c) => {
                // Track-and-apply: optional apply_cmd substitutes {version}.
                if let Some(apply) = &c.apply_cmd {
                    let vars = BTreeMap::from([("version", target.version.as_str())]);
                    let argv = exec::substitute(apply, &vars)?;
                    let detail = serde_json::json!({"cmd": &argv});
                    self.run_apply_cmd(app, &argv, &detail, rep).await?;
                } else {
                    rep.actions.push("track-only (no apply_cmd)".into());
                }
                self.store.set_installed(&app.id, &target.version)?;
                rep.actions
                    .push(format!("set installed = {}", target.version));
                return Ok(());
            }
            SourceKind::DockerRegistry(d) => {
                self.apply_docker(app, d, target, rep).await?;
                self.store.set_installed(&app.id, &target.version)?;
                rep.actions
                    .push(format!("set installed = {}", target.version));
                return Ok(());
            }
            SourceKind::Apt(c) => {
                self.apply_apt(app, c, target, rep).await?;
                self.store.set_installed(&app.id, &target.version)?;
                rep.actions
                    .push(format!("set installed = {}", target.version));
                return Ok(());
            }
            SourceKind::Flatpak(f) => {
                self.apply_flatpak(app, f, target, rep).await?;
                self.store.set_installed(&app.id, &target.version)?;
                rep.actions
                    .push(format!("set installed = {}", target.version));
                return Ok(());
            }
            SourceKind::External { .. } if matches!(app.install.kind, InstallType::None) => {
                // Track-only: no install requested.
                self.store.set_installed(&app.id, &target.version)?;
                rep.actions
                    .push(format!("set installed = {}", target.version));
                return Ok(());
            }
            SourceKind::External { .. } => {} // fall through to the asset path
            SourceKind::GithubReleases(_) | SourceKind::HtmlLinks(_) => {}
        }

        // Asset-carrying shapes: steps 2-4 then installer dispatch (5-7).
        let asset = match self.primary_asset(app, target)? {
            Some(a) => a,
            None => bail!("app '{}' requires assets but none resolved", app.id),
        };

        let opts = DownloadOpts {
            max_bytes: self.global.max_download_mb.saturating_mul(1024 * 1024),
            timeout: std::time::Duration::from_secs(self.global.download_timeout_secs),
            allow_insecure_http: app.allow_insecure_http,
        };
        let download_path = net::download(&self.http, &asset.url, &self.home.downloads(), &opts)
            .await
            .context("downloading asset")?;
        self.store.event(
            "download_done",
            Some(&app.id),
            Some(
                &serde_json::json!({
                    "asset": asset.name,
                    "url": asset.url.to_string(),
                    "path": download_path.display().to_string()
                })
                .to_string(),
            ),
        )?;
        rep.actions.push(format!("downloaded {}", asset.name));

        // Step 4: digest gate.
        match (&asset.digest, app.digest_policy) {
            (None, DigestPolicy::Required) => {
                bail!(
                    "digest policy 'required' but no digest available for {}",
                    asset.name
                )
            }
            (None, DigestPolicy::Prefer) => {
                let w = format!(
                    "no digest published for {}; accepted under 'prefer'",
                    asset.name
                );
                self.store.event(
                    "verify_failed",
                    Some(&app.id),
                    Some(&serde_json::json!({"level": "warn", "detail": w}).to_string()),
                )?;
                rep.warnings.push(w);
            }
            (None, DigestPolicy::Off) => {
                rep.actions.push("digest check skipped (off)".into());
            }
            (Some(d), _) => {
                use sha2::{Digest as _, Sha256};
                let bytes = std::fs::read(&download_path).context("reading download")?;
                let actual = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
                if actual != *d {
                    let detail = serde_json::json!({
                        "expected": d, "actual": actual, "asset": asset.name
                    })
                    .to_string();
                    self.store
                        .event("verify_failed", Some(&app.id), Some(&detail))?;
                    bail!(
                        "digest mismatch for {}: expected {d}, got {actual}",
                        asset.name
                    );
                }
                rep.actions.push(format!("digest verified {}", asset.name));
            }
        }

        // Steps 5-7: installer registry dispatch.
        let inst = installer::lookup(&self.installers, &app.install.kind.key())?;
        let extra_table = app.install.extra_table();
        let ctx = installer::InstallCtx {
            app_id: &app.id,
            version: &target.version,
            cfg: &app.install,
            extra: &extra_table,
            download: Some(&download_path),
            filename: Some(&asset.name),
            checkout: None,
            artifacts: &[],
            home: &self.home,
        };
        let outcome = inst.install(&ctx).await?;
        rep.actions.extend(outcome.actions);
        match outcome.publication {
            Some(p) => {
                self.publish_common(app, p, rep)?;
            }
            None => {
                self.finish_install(app, outcome.exec_hint.as_deref(), rep)?;
            }
        }
        Ok(())
    }

    /// Clone (first time) or fetch an existing checkout, then detach onto the
    /// target tag. Tries the bare version first, then a `v`-prefixed tag.
    /// Fetch/update a configured checkout or the managed source root, then detach onto the target tag.
    async fn checkout_source(&self, repo_url: &str, id: &str, version: &str, source_dir: Option<&str>) -> Result<PathBuf> {
        let dir = source_dir
            .map(crate::config::expand_tilde)
            .unwrap_or_else(|| self.home.src().join(id));
        let timeout = std::time::Duration::from_secs(300);
        if dir.join(".git").exists() {
            let out = exec::run(
                &["git".into(), "fetch".into(), "--tags".into(), "--force".into()],
                Some(&dir),
                timeout,
                true,
            )
            .await?;
            anyhow::ensure!(out.status, "git fetch failed:\n{}", out.stderr_tail);
        } else {
            if dir.exists() {
                bail!("configured source_dir exists but is not a git checkout: {}", dir.display());
            }
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let out = exec::run(
                &[
                    "git".into(),
                    "clone".into(),
                    "--filter=blob:none".into(),
                    repo_url.to_string(),
                    dir.to_string_lossy().into_owned(),
                ],
                None,
                timeout,
                true,
            )
            .await?;
            anyhow::ensure!(out.status, "git clone failed:\n{}", out.stderr_tail);
        }
        for tag in [version.to_string(), format!("v{version}")] {
            let out = exec::run(
                &["git".into(), "checkout".into(), "--detach".into(), tag],
                Some(&dir),
                timeout,
                true,
            )
            .await?;
            if out.status {
                return Ok(dir);
            }
        }
        bail!("tag '{version}' not found in {repo_url}")
    }

    /// Steps 5-7 for provider = source-build.
    async fn build_and_install_source(
        &self,
        app: &AppFile,
        sb: &crate::config::SourceBuildCfg,
        target: &Release,
        rep: &mut UpdateReport,
    ) -> Result<()> {
        // Step 5: checkout + integrity gate.
        let checkout = self
            .checkout_source(&sb.repo_url, &app.id, &target.version, sb.source_dir.as_deref())
            .await?;
        rep.actions.push(format!("checked out {}", target.version));

        let files = hash::collect_files(&checkout, &sb.build.tracked_files)?;
        if files.is_empty() {
            bail!(
                "tracked_files {:?} matched nothing in {}",
                sb.build.tracked_files,
                checkout.display()
            );
        }
        let bundle = hash::bundle_hash(&files);
        let stored = self.store.script_hash(&app.id)?;
        match stored.as_deref() {
            None => {
                self.store.set_script_hash(&app.id, Some(&bundle))?;
            }
            Some(prev) if prev != bundle => {
                // Preserve the CHANGED candidate for `--show-diff` review
                // (the previously approved state is already on file from the
                // last successful build), then STOP. Only `approve` clears.
                hash::save_snapshot(&self.home.script_snapshots().join(&app.id), &bundle, &files)
                    .context("saving changed-scripts snapshot")?;
                let detail = serde_json::json!({
                    "old": prev, "new": bundle,
                    "hint": "review with: updatium approve"
                })
                .to_string();
                self.store
                    .event("script_changed", Some(&app.id), Some(&detail))?;
                bail!(
                    "build scripts changed since last approval ({} -> {}); run `updatium approve {}` after review",
                    &prev[..12],
                    &bundle[..12],
                    app.id
                );
            }
            Some(_) => {}
        }

        // Persist the now-approved script state so future gates can diff
        // against exactly these bytes.
        hash::save_snapshot(&self.home.script_snapshots().join(&app.id), &bundle, &files)?;

        // Step 6: build steps sequentially.
        let timeout = std::time::Duration::from_secs(sb.build.timeout_secs);
        for (i, step) in sb.build.steps.iter().enumerate() {
            let out = exec::run_env(step, Some(&checkout), timeout, &sb.build.env, true)
                .await
                .with_context(|| {
                    format!("build step {} ({}) failed to start", i + 1, step.join(" "))
                })?;
            anyhow::ensure!(
                out.status,
                "build step {} ({}) failed:\n{}",
                i + 1,
                step.join(" "),
                out.stderr_tail
            );
            rep.actions.push(format!("build[{}] ok", step[0]));
        }

        // Step 7: install into a configured external directory via a staging prefix.
        if let Some(install_dir) = sb.install_dir.as_deref() {
            if sb.build.install_steps.is_empty() {
                bail!("install_dir requires at least one build.install_steps command");
            }
            let destination = crate::config::expand_tilde(install_dir);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let staging_root = destination.with_file_name(format!(".{}-staging-{}", app.id, target.version));
            let _ = std::fs::remove_dir_all(&staging_root);
            std::fs::create_dir_all(&staging_root)?;
            let staging_text = staging_root.to_string_lossy().into_owned();
            let mut vars = BTreeMap::new();
            vars.insert("install_dir", staging_text.as_str());
            for (i, step) in sb.build.install_steps.iter().enumerate() {
                let command = exec::substitute(step, &vars)?;
                let out = exec::run_env(&command, Some(&checkout), timeout, &sb.build.env, true)
                    .await
                    .with_context(|| format!("install step {} ({}) failed to start", i + 1, command.join(" ")))?;
                anyhow::ensure!(
                    out.status,
                    "install step {} ({}) failed:\n{}",
                    i + 1,
                    command.join(" "),
                    out.stderr_tail
                );
                rep.actions.push(format!("install[{}] ok", command[0]));
            }
            if destination.exists() {
                std::fs::remove_dir_all(&destination)
                    .with_context(|| format!("replacing {}", destination.display()))?;
            }
            std::fs::rename(&staging_root, &destination)
                .with_context(|| format!("activating {}", destination.display()))?;
            self.store.event("install_ok", Some(&app.id), None)?;
            return self.post_build_finish(app, target, rep).await;
        }

        // Default path: publish through Updatium's managed installer tree.
        let inst = installer::lookup(&self.installers, "dir")?;
        let empty_extra = serde_json::Value::Object(Default::default());
        let ctx = installer::InstallCtx {
            app_id: &app.id,
            version: &target.version,
            cfg: &app.install,
            extra: &empty_extra,
            download: None,
            filename: None,
            checkout: Some(&checkout),
            artifacts: &sb.build.artifacts,
            home: &self.home,
        };
        let outcome = inst.install(&ctx).await?;
        rep.actions.extend(outcome.actions);
        let pubn = outcome
            .publication
            .context("dir installer returned no publication")?;
        self.publish_common(app, pubn, rep)?;
        self.post_build_finish(app, target, rep).await
    }
    fn finish_install(
        &self,
        app: &AppFile,
        exec_hint: Option<&Path>,
        rep: &mut UpdateReport,
    ) -> Result<()> {
        self.store.event("install_ok", Some(&app.id), None)?;
        if let Some(to) = rep.to.clone() {
            self.store.set_installed(&app.id, &to)?;
            rep.actions.push(format!("set installed = {to}"));
        }
        if let (Some(to), Some(from)) = (rep.to.clone(), rep.from.clone()) {
            let _ = (from, to); // kept for future changelog hooks
        }
        if let Some(d) = &app.install.desktop {
            match exec_hint {
                Some(exec) => {
                    let icon = d.icon.as_deref().map(crate::config::expand_tilde);
                    desktop_entry_write(
                        &self.home.desktop_dir,
                        &app.id,
                        &d.name,
                        exec,
                        icon.as_deref(),
                        &d.categories,
                        d.terminal,
                    )?;
                    rep.actions.push(format!("desktop entry '{}'", d.name));
                }
                None => {
                    rep.warnings.push(
                        "desktop requested but installer returned no exec path".into(),
                    );
                }
            }
        }
        Ok(())
    }

    fn publish_common(
        &self,
        app: &AppFile,
        mut pubn: Publication,
        rep: &mut UpdateReport,
    ) -> Result<PathBuf> {
        // Symlink publication into ~/.local/bin (or relocated equivalent).
        if !app.install.link_bins.is_empty() {
            pubn = installer::link_bins(
                pubn,
                &self.home.install_current(&app.id),
                &app.id,
                &self.home.installs(),
                &app.install.link_bins,
                &self.home.local_bin,
            )?;
            for l in &pubn.linked {
                rep.actions.push(format!("linked {}", l.display()));
            }
            for c in &pubn.conflicts {
                self.store.event(
                    "install_failed",
                    Some(&app.id),
                    Some(&serde_json::json!({"conflict": c}).to_string()),
                )?;
                rep.warnings.push(c.clone());
            }
        }
        self.store.event("install_ok", Some(&app.id), None)?;
        if let Some(to) = rep.to.clone() {
            self.store.set_installed(&app.id, &to)?;
            rep.actions.push(format!("set installed = {to}"));
        }
        let exec_hint = self
            .home
            .install_current(&app.id)
            .join(pubn.published.first().map(String::as_str).unwrap_or(""));
        if let Some(d) = &app.install.desktop {
            let icon = d.icon.as_deref().map(crate::config::expand_tilde);
            desktop_entry_write(
                &self.home.desktop_dir,
                &app.id,
                &d.name,
                &exec_hint,
                icon.as_deref(),
                &d.categories,
                d.terminal,
            )?;
            rep.actions.push(format!("desktop entry '{}'", d.name));
        }
        Ok(exec_hint)
    }

    async fn post_build_finish(
        &self,
        app: &AppFile,
        target: &Release,
        rep: &mut UpdateReport,
    ) -> Result<()> {
        self.store.set_installed(&app.id, &target.version)?;
        rep.actions
            .push(format!("set installed = {}", target.version));
        let _ = app;
        Ok(())
    }

    /// Run a provider `apply_cmd`, recording the `applied` event and a report
    /// action. Shares the docker apply timeout rule.
    async fn run_apply_cmd(
        &self,
        app: &AppFile,
        argv: &[String],
        detail: &serde_json::Value,
        rep: &mut UpdateReport,
    ) -> Result<()> {
        let out = exec::run(
            argv,
            None,
            std::time::Duration::from_secs(self.global.download_timeout_secs.max(600)),
            true,
        )
        .await?;
        anyhow::ensure!(out.status, "apply_cmd failed:\n{}", out.stderr_tail);
        self.store
            .event("applied", Some(&app.id), Some(&detail.to_string()))?;
        rep.actions.push(format!("applied via {}", argv.join(" ")));
        Ok(())
    }

    async fn apply_apt(
        &self,
        app: &AppFile,
        cfg: &crate::config::AptCfg,
        target: &Release,
        rep: &mut UpdateReport,
    ) -> Result<()> {
        let Some(apply) = &cfg.apply_cmd else {
            rep.actions.push("track-only (no apply_cmd)".into());
            return Ok(());
        };
        let vars: BTreeMap<&str, &str> = BTreeMap::from([
            ("package", cfg.package.as_str()),
            ("version", target.version.as_str()),
        ]);
        let argv = exec::substitute(apply, &vars)?;
        let detail = serde_json::json!({"package": cfg.package, "version": target.version});
        self.run_apply_cmd(app, &argv, &detail, rep).await
    }

    async fn apply_flatpak(
        &self,
        app: &AppFile,
        cfg: &crate::config::FlatpakCfg,
        target: &Release,
        rep: &mut UpdateReport,
    ) -> Result<()> {
        let Some(apply) = &cfg.apply_cmd else {
            rep.actions.push("track-only (no apply_cmd)".into());
            return Ok(());
        };
        let vars: BTreeMap<&str, &str> = BTreeMap::from([
            ("app_id", cfg.app_id.as_str()),
            ("remote", cfg.remote.as_str()),
            ("version", target.version.as_str()),
        ]);
        let argv = exec::substitute(apply, &vars)?;
        let detail =
            serde_json::json!({"app_id": cfg.app_id, "remote": cfg.remote, "version": target.version});
        self.run_apply_cmd(app, &argv, &detail, rep).await
    }

    async fn apply_docker(
        &self,
        app: &AppFile,
        cfg: &crate::config::DockerRegistryCfg,
        target: &Release,
        rep: &mut UpdateReport,
    ) -> Result<()> {
        let cfg = cfg.clone();
        let (registry_host, repo) = provider::docker_registry::split_image(&cfg.image)?;
        let vars: BTreeMap<&str, &str> = BTreeMap::from([
            ("version", target.version.as_str()),
            ("image", cfg.image.as_str()),
            ("registry", registry_host.as_str()),
            ("repo", repo.as_str()),
        ]);
        let pull = exec::substitute(&cfg.pull_cmd, &vars)?;
        let out = exec::run(
            &pull,
            None,
            std::time::Duration::from_secs(self.global.download_timeout_secs.max(600)),
            true,
        )
        .await?;
        anyhow::ensure!(out.status, "pull_cmd failed:\n{}", out.stderr_tail);
        rep.actions.push(format!("pulled {}", cfg.image));

        if let Some(rd) = &cfg.redeploy_cmd {
            let rd_argv = exec::substitute(rd, &vars)?;
            let out = exec::run(&rd_argv, None, std::time::Duration::from_secs(600), true).await?;
            anyhow::ensure!(out.status, "redeploy_cmd failed:\n{}", out.stderr_tail);
            rep.actions
                .push(format!("redeployed via {}", rd_argv.join(" ")));
        }
        self.store.event(
            "applied",
            Some(&app.id),
            Some(&serde_json::json!({"image": cfg.image, "tag": target.version}).to_string()),
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Introspection for CLI/TUI/tests
    // -----------------------------------------------------------------

    /// Installed version for `app`: recorded install state wins, with a
    /// source-build checkout fallback for builds done outside updatium.
    pub fn installed_for(&self, app: &AppFile) -> Option<String> {
        self.store
            .state(&app.id)
            .ok()
            .flatten()
            .and_then(|s| s.installed_version)
            .or_else(|| self.detect_source_build_version(app))
    }

    /// Derive the installed version of a source-build app from the tag checked
    /// out in its `source_dir` (or managed `src/<id>` checkout) when an install
    /// target exists. Detached HEADs without a matching tag yield None.
    fn detect_source_build_version(&self, app: &AppFile) -> Option<String> {
        let SourceKind::SourceBuild(sb) = &app.source_kind else {
            return None;
        };
        let checkout = sb
            .source_dir
            .as_deref()
            .map(crate::config::expand_tilde)
            .unwrap_or_else(|| self.home.src().join(&app.id));
        let installed_exists = match sb.install_dir.as_deref() {
            Some(dir) => crate::config::expand_tilde(dir).exists(),
            None => self.home.install_current(&app.id).exists(),
        };
        if !installed_exists || !checkout.join(".git").exists() {
            return None;
        }
        let re = regex::Regex::new(&sb.ref_regex).ok()?;
        let out = std::process::Command::new("git")
            .args(["tag", "--points-at", "HEAD"])
            .current_dir(&checkout)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut versions: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|tag| !tag.is_empty() && re.is_match(&format!("refs/tags/{tag}")))
            .map(provider::version_from_tag)
            .collect();
        versions.sort_by(|a, b| cmp_version(a, b));
        versions.pop()
    }

    pub fn installed_version(&self, id: &str) -> Result<Option<String>> {
        if let Some(app) = self.load_apps()?.into_iter().find(|a| a.id == id) {
            return Ok(self.installed_for(&app));
        }
        Ok(self.store.state(id)?.and_then(|s| s.installed_version))
    }

    pub fn script_hash_of(&self, id: &str) -> Result<Option<String>> {
        self.store.script_hash(id)
    }

    pub fn history(&self, id: Option<&str>, limit: u32) -> Result<Vec<crate::store::EventRow>> {
        self.store.events(id, limit)
    }

    pub fn latest_known_pub(&self, id: &str) -> Result<Option<String>> {
        self.store.latest_known(id)
    }

    /// (id, provider, installed) rows for `updatium list` and the TUI frame.
    pub fn store_list(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let apps: std::collections::HashMap<String, AppFile> = self
            .load_apps()?
            .into_iter()
            .map(|a| (a.id.clone(), a))
            .collect();
        let mut out = Vec::new();
        for a in self.store.list_apps()? {
            let installed = match apps.get(&a.id) {
                Some(app) => self.installed_for(app),
                None => self
                    .store
                    .state(&a.id)
                    .ok()
                    .flatten()
                    .and_then(|s| s.installed_version),
            };
            out.push((a.id, a.provider, installed));
        }
        Ok(out)
    }

    /// Enabled app ids from apps.d/ (for update --all).
    pub fn load_enabled_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .load_apps()?
            .into_iter()
            .filter(|a| a.enabled)
            .map(|a| a.id)
            .collect())
    }

    pub fn home_paths(&self) -> &HomePaths {
        &self.home
    }

    /// Recompute the tracked-files bundle and clear the approval gate.
    /// Returns (old_hash, new_hash).
    pub fn approve(&self, id: &str) -> Result<(Option<String>, String)> {
        let app = self
            .load_apps()?
            .into_iter()
            .find(|a| a.id == id)
            .with_context(|| format!("unknown app '{id}'"))?;
        let sb = match &app.source_kind {
            SourceKind::SourceBuild(sb) => sb.clone(),
            _ => bail!("approval gate applies only to source-build apps"),
        };
        let checkout = sb
            .source_dir
            .as_deref()
            .map(crate::config::expand_tilde)
            .unwrap_or_else(|| self.home.src().join(id));
        anyhow::ensure!(
            checkout.exists(),
            "no checkout at {} for '{id}' — run an update first",
            checkout.display()
        );
        let files = hash::collect_files(&checkout, &sb.build.tracked_files)?;
        if files.is_empty() {
            bail!("tracked_files matched nothing under {}", checkout.display());
        }
        let new = hash::bundle_hash(&files);
        let old = self.store.script_hash(id)?;
        self.store.set_script_hash(id, Some(&new))?;
        self.store.event(
            "approved",
            Some(id),
            Some(&serde_json::json!({"hash": new}).to_string()),
        )?;
        Ok((old, new))
    }

    /// Two newest snapshots for `approve --show-diff`.
    pub fn newest_snapshots(&self, id: &str) -> Result<Vec<PathBuf>> {
        let dir = self.home.script_snapshots().join(id);
        let mut snaps: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "tar") {
                    e.metadata().ok()?.modified().ok().map(|m| (m, p))
                } else {
                    None
                }
            })
            .collect();
        snaps.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(snaps.into_iter().map(|(_, p)| p).take(2).collect())
    }

    /// Pipe a JSON summary into notify_cmd when configured.
    pub fn notify(&self, updates: &[CheckReport], failures: &[String]) -> Result<bool> {
        let cmd = match &self.global.notify_cmd {
            c if c.is_empty() => return Ok(false),
            c => c,
        };
        let payload = serde_json::json!({
            "updates": updates.iter().filter(|r| matches!(r.status, Status::UpdateAvailable))
                .map(|r| serde_json::json!({"app": r.app, "installed": r.installed, "latest": r.latest}))
                .collect::<Vec<_>>(),
            "failures": failures,
        });
        let payload_str = payload.to_string();
        let rt = tokio::runtime::Handle::try_current()
            .map(|h| h.clone())
            .ok();
        let result = match rt {
            Some(h) => h.block_on(async {
                exec::run_with_stdin(
                    cmd,
                    None,
                    std::time::Duration::from_secs(30),
                    true,
                    Some(payload_str.as_bytes()),
                    &[],
                )
                .await
            }),
            None => Err(anyhow::anyhow!("no async runtime for notify")),
        };
        match result {
            Ok(out) if out.status => {
                let _ = self.store.event("notified", None, Some(&payload_str));
            }
            Ok(out) => {
                let _ = self.store.event(
                    "notified",
                    None,
                    Some(&format!("notify command failed: {}", out.stderr_tail)),
                );
            }
            Err(e) => {
                let _ = self
                    .store
                    .event("notified", None, Some(&format!("notify error: {e:#}")));
            }
        }
        Ok(true)
    }
}

/// Thin wrapper so both install paths share the desktop writer call.
#[allow(clippy::too_many_arguments)]
fn desktop_entry_write(
    dir: &Path,
    id: &str,
    name: &str,
    exec_path: &Path,
    icon: Option<&Path>,
    categories: &[String],
    terminal: bool,
) -> Result<PathBuf> {
    crate::desktop::write_desktop(dir, id, name, exec_path, icon, categories, terminal)
}
