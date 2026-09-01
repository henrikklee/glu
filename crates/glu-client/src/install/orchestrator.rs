use crate::hash::Sha256Mismatch;
use crate::{
    bottle::{
        codesign::CodeSignPool,
        prepare::{perl_relocation_path, prepare_bottle, PrepareInput, PreparedKeg},
        writer::WriterPool,
    },
    download::{cache::ArtifactCache, ArtifactDownloader, DownloadProgress, VerifiedArtifact},
    events::ExecutionEvents,
    install::{
        dag::ExecutionPlan,
        manifest_lookup::ManifestLookup,
        scheduler::{
            execute_plan, ExecutionContext, ExecutionObserver, InstallOperations, InstallResult,
            RuntimeSubphase,
        },
        InstallOptions,
    },
    link::{
        keg::link_keg,
        opt::make_relative_symlink,
        prefix::{commit_prepared_keg, write_install_receipt, write_prepared_receipt},
        unlink::unlink_keg,
    },
    postinstall::{
        sandbox::{run_deferred_global_postinstall_sandboxed, run_formula_postinstall_sandboxed},
        structured::{DeferredPostinstallQueue, PostinstallPlans},
    },
    state::{store::InstalledStateStore, GluInstallReceipt, ReceiptStatus},
};
use anyhow::{bail, Context, Result};
use futures_util::{future::BoxFuture, FutureExt};
use glu_core::{ArtifactId, InstallManifest, PackageId, PackageName, Prefix};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;

#[derive(Debug, Default)]
pub struct InstallStats {
    pub reused: AtomicUsize,
    pub downloaded: AtomicUsize,
    pub prepared: AtomicUsize,
    pub signed_machos: AtomicUsize,
    pub linked_files: AtomicUsize,
    pub global_postinstalls: AtomicUsize,
}

impl InstallStats {
    pub fn snapshot(&self) -> InstallStatsSnapshot {
        InstallStatsSnapshot {
            reused: self.reused.load(Ordering::Relaxed),
            downloaded: self.downloaded.load(Ordering::Relaxed),
            prepared: self.prepared.load(Ordering::Relaxed),
            signed_machos: self.signed_machos.load(Ordering::Relaxed),
            linked_files: self.linked_files.load(Ordering::Relaxed),
            global_postinstalls: self.global_postinstalls.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InstallStatsSnapshot {
    pub reused: usize,
    pub downloaded: usize,
    pub prepared: usize,
    pub signed_machos: usize,
    pub linked_files: usize,
    pub global_postinstalls: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InstallPoolStatsSnapshot {
    pub writer_seconds: f64,
    pub writer_workers: usize,
    pub codesign_seconds: f64,
    pub codesign_workers: usize,
}

#[derive(Debug, Clone)]
pub struct SchedulerInstallResult {
    pub execution: InstallResult,
    pub stats: InstallStatsSnapshot,
    pub pool_stats: InstallPoolStatsSnapshot,
}

pub struct ExecuteInstallPlanInput {
    pub plan: ExecutionPlan,
    pub manifest: Arc<InstallManifest>,
    pub prefix: Prefix,
    pub postinstall_plans: PostinstallPlans,
    pub options: InstallOptions,
    pub deactivated_names: BTreeSet<PackageName>,
    pub ctx: ExecutionContext,
    pub observer: Arc<dyn ExecutionObserver>,
    pub download_progress: DownloadProgress,
    pub events: Arc<dyn ExecutionEvents>,
}

pub async fn execute_install_plan(
    input: ExecuteInstallPlanInput,
) -> Result<SchedulerInstallResult> {
    let ExecuteInstallPlanInput {
        plan,
        manifest,
        prefix,
        postinstall_plans,
        options,
        deactivated_names,
        ctx,
        observer,
        download_progress,
        events,
    } = input;

    let ops = Arc::new(SchedulerInstallOperations::new(
        manifest,
        prefix,
        options,
        deactivated_names,
        postinstall_plans,
        download_progress,
        events,
    ));
    let execution = execute_plan(plan, ops.clone(), Some(observer), ctx).await?;
    Ok(SchedulerInstallResult {
        execution,
        stats: ops.stats.snapshot(),
        pool_stats: InstallPoolStatsSnapshot {
            writer_seconds: ops.writer_pool.total_write_time().as_secs_f64(),
            writer_workers: ops.writer_pool.workers(),
            codesign_seconds: ops.code_sign_pool.total_sign_time().as_secs_f64(),
            codesign_workers: ops.code_sign_pool.workers(),
        },
    })
}

struct SchedulerInstallOperations {
    manifest: Arc<InstallManifest>,
    prefix: Prefix,
    downloader: ArtifactDownloader,
    download_progress: DownloadProgress,
    verified_artifacts: Mutex<BTreeMap<ArtifactId, VerifiedArtifact>>,
    prepared_kegs: Mutex<BTreeMap<PackageId, PreparedKeg>>,
    link_counts: Mutex<BTreeMap<PackageId, usize>>,
    receipt_linked: Mutex<BTreeMap<PackageId, bool>>,
    deferred_postinstalls: Arc<Mutex<DeferredPostinstallQueue>>,
    postinstall_plans: PostinstallPlans,
    stats: InstallStats,
    options: InstallOptions,
    deactivated_names: BTreeSet<PackageName>,
    writer_pool: Arc<WriterPool>,
    code_sign_pool: Arc<CodeSignPool>,
    events: Arc<dyn ExecutionEvents>,
}

impl SchedulerInstallOperations {
    fn new(
        manifest: Arc<InstallManifest>,
        prefix: Prefix,
        options: InstallOptions,
        deactivated_names: BTreeSet<PackageName>,
        postinstall_plans: PostinstallPlans,
        download_progress: DownloadProgress,
        events: Arc<dyn ExecutionEvents>,
    ) -> Self {
        let downloader = ArtifactDownloader::new(&prefix);
        let writer_pool = Arc::new(WriterPool::new());
        let code_sign_pool = Arc::new(CodeSignPool::new(Arc::clone(&writer_pool)));
        Self {
            manifest,
            prefix,
            downloader,
            download_progress,
            verified_artifacts: Mutex::new(BTreeMap::new()),
            prepared_kegs: Mutex::new(BTreeMap::new()),
            link_counts: Mutex::new(BTreeMap::new()),
            receipt_linked: Mutex::new(BTreeMap::new()),
            deferred_postinstalls: Arc::new(Mutex::new(DeferredPostinstallQueue::new())),
            postinstall_plans,
            stats: InstallStats::default(),
            options,
            deactivated_names,
            writer_pool,
            code_sign_pool,
            events,
        }
    }

    async fn ghcr_auth(&self) -> Result<()> {
        self.downloader
            .configure_install_token(self.manifest.artifacts.values())
            .await
    }

    async fn ghcr_bottle_download(
        &self,
        package_id: &PackageId,
        cached: bool,
        priority: u64,
    ) -> Result<()> {
        let package = self.manifest.require_package(package_id)?;
        let artifact = self.manifest.require_artifact(&package.artifact)?;

        // The plan already resolved cache state (`uncached_packages` at plan
        // build time is what decided whether `ghcr_auth` runs). On a cache hit
        // the download node needs no downloader, no token, and no re-check:
        // record the verified artifact straight from the known cache path.
        if cached {
            let path = ArtifactCache::new(&self.prefix).path_for_artifact(artifact);
            self.stats.reused.fetch_add(1, Ordering::Relaxed);
            self.verified_artifacts.lock().await.insert(
                package.artifact.clone(),
                VerifiedArtifact {
                    id: package.artifact.clone(),
                    path,
                    sha256: artifact.sha256.clone(),
                    reused: true,
                },
            );
            return Ok(());
        }

        let node_id = crate::install::dag::NodeKind::GhcrBottleDownload.node_id(&package.name.0);
        let progress = self.download_progress.clone();
        let verified = self
            .downloader
            .get_or_download_with_priority(
                package.artifact.clone(),
                artifact,
                priority,
                move |bytes| {
                    progress.report(&node_id, bytes);
                },
            )
            .await?;
        if verified.reused {
            self.stats.reused.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.downloaded.fetch_add(1, Ordering::Relaxed);
        }
        self.verified_artifacts
            .lock()
            .await
            .insert(package.artifact.clone(), verified);
        Ok(())
    }

    async fn bottle_prepare(
        &self,
        package_id: &PackageId,
        ctx: ExecutionContext,
        node_id: &str,
        pool: &str,
        slot: Option<usize>,
    ) -> Result<()> {
        let package = self.manifest.require_package(package_id)?.clone();
        let verified = self
            .verified_artifacts
            .lock()
            .await
            .get(&package.artifact)
            .cloned()
            .with_context(|| format!("missing verified artifact {}", package.artifact.0))?;
        let start = ctx.now();
        // Skip only the Mach-O + fixed-prefix byte passes for `:any_skip_relocation`
        // bottles, mirroring Homebrew: `bottle_specification.rb skip_relocation?` gates
        // only `relocate_dynamic_linkage` (formula_installer.rb pour path), while the text
        // pass always runs — a skip bottle like gradle can still ship text placeholders.
        //
        // Placeholder replacement is NEVER skippable based on the target prefix: bottles
        // carry `@@HOMEBREW_PREFIX@@` bytes that must be substituted into the active prefix,
        // even when that prefix happens to be `/opt/homebrew` (the substitution is still a
        // byte change). Only the literal fixed-prefix pass is identity in that case, and it
        // self-noops. See docs/explanation/install-pipeline.md.
        let cellar = self
            .manifest
            .require_artifact(&package.artifact)
            .map(|a| a.cellar.as_str())
            .unwrap_or("");
        let skip_relocation = cellar == ":any_skip_relocation";
        // The literal build-prefix string bottles of this artifact may contain, derived from
        // its fixed cellar: `/opt/homebrew/Cellar` -> `/opt/homebrew`. Marker cellars
        // (`:any`, `:any_skip_relocation`) have no fixed path; fall back to `/opt/homebrew`,
        // the only build prefix present in the registry (26,812 variants: 3,487 fixed at
        // /opt/homebrew/Cellar, 0 at any other path in the audited registry.
        let build_prefix = if cellar.starts_with('/') {
            Path::new(&cellar)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/opt/homebrew".to_string())
        } else {
            "/opt/homebrew".to_string()
        };
        // Homebrew's java relocation pair (extend/os/mac/keg_relocate.rb
        // prepare_relocation_to_locations): @@HOMEBREW_JAVA@@ ->
        // <prefix>/opt/<openjdk>/libexec/openjdk.jdk/Contents/Home, added when the
        // formula declares an openjdk runtime dependency.
        let prefix_str = self.prefix.0.to_string_lossy().into_owned();
        let java_path = package
            .deps
            .iter()
            .find(|dep| dep.requested_as.0.starts_with("openjdk"))
            .map(|dep| {
                format!(
                    "{prefix_str}/opt/{}/libexec/openjdk.jdk/Contents/Home",
                    dep.requested_as.0
                )
            });
        // Homebrew's perl relocation pair: opt perl when the package is perl or declares
        // perl directly; else the bottle's built_on.preferred_perl if that system binary
        // exists; else the current-OS preferred perl (5.34).
        let perl_path = perl_relocation_path(
            &package.name.0,
            package
                .deps
                .iter()
                .any(|dep| dep.package_key.0 == "package:perl"),
            self.manifest
                .require_artifact(&package.artifact)
                .ok()
                .and_then(|a| a.built_on.as_ref())
                .and_then(|b| b.preferred_perl.as_deref()),
            &prefix_str,
        );
        let mut verified = verified;
        let mut keg_result = prepare_bottle(PrepareInput {
            package_id: package_id.clone(),
            package: package.clone(),
            artifact: verified.clone(),
            prefix: self.prefix.clone(),
            writer_pool: Arc::clone(&self.writer_pool),
            code_sign_pool: Arc::clone(&self.code_sign_pool),
            skip_relocation,
            build_prefix: build_prefix.clone(),
            java_path: java_path.clone(),
            perl_path: perl_path.clone(),
        })
        .await;

        if let Err(error) = keg_result {
            // Cached artifacts are reverified fused into prepare/extract. A mismatch on a reused
            // cache hit means the admitted file is corrupt or tampered with; evict it and retry
            // once through the normal fresh-download cache-admission path.
            if verified.reused && is_sha256_mismatch(&error) {
                let artifact = self.manifest.require_artifact(&package.artifact)?.clone();
                ArtifactCache::new(&self.prefix)
                    .remove_artifact(&artifact)
                    .await
                    .with_context(|| {
                        format!("evicting corrupt cached artifact {}", package.artifact.0)
                    })?;
                let node_id =
                    crate::install::dag::NodeKind::GhcrBottleDownload.node_id(&package.name.0);
                let progress = self.download_progress.clone();
                let fresh = self
                    .downloader
                    .get_or_download_with_priority(
                        package.artifact.clone(),
                        &artifact,
                        0,
                        move |bytes| {
                            progress.report(&node_id, bytes);
                        },
                    )
                    .await
                    .with_context(|| format!("redownloading artifact {}", package.artifact.0))?;
                let _ =
                    self.stats
                        .reused
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                            Some(value.saturating_sub(1))
                        });
                if fresh.reused {
                    self.stats.reused.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.stats.downloaded.fetch_add(1, Ordering::Relaxed);
                }
                self.verified_artifacts
                    .lock()
                    .await
                    .insert(package.artifact.clone(), fresh.clone());
                verified = fresh;
                keg_result = prepare_bottle(PrepareInput {
                    package_id: package_id.clone(),
                    package: package.clone(),
                    artifact: verified,
                    prefix: self.prefix.clone(),
                    writer_pool: Arc::clone(&self.writer_pool),
                    code_sign_pool: Arc::clone(&self.code_sign_pool),
                    skip_relocation,
                    build_prefix,
                    java_path,
                    perl_path,
                })
                .await;
            } else {
                return Err(error);
            }
        }

        let keg = keg_result?;
        // Subphase breakdown (extract / writer_wait / text_relocate /
        // fixed_prefix_relocate / macho_patch / codesign) so traces are directly comparable
        // across tools. glu times these per-file within one streaming pass rather than separate
        // whole-keg passes, so
        // the spans recorded here are stacked sequentially in the given order rather than
        // reflecting true interleaved wall-clock timing — the summed durations are the honest,
        // comparable quantity.
        let timings = keg.phase_timings;
        let mut cursor = start;
        for (phase, duration) in [
            ("extract", timings.extract),
            ("writer_wait", timings.writer_wait),
            ("text_relocate", timings.text_relocate),
            ("fixed_prefix_relocate", timings.fixed_prefix_relocate),
            ("macho_patch", timings.macho_patch),
            ("codesign", timings.codesign),
        ] {
            let end = cursor + duration.as_secs_f64();
            ctx.record_subphase(RuntimeSubphase {
                node_id,
                phase,
                start: cursor,
                end,
                status: "ok",
                pool,
                slot,
            });
            cursor = end;
        }
        self.stats.prepared.fetch_add(1, Ordering::Relaxed);
        self.stats
            .signed_machos
            .fetch_add(keg.files_to_codesign.len(), Ordering::Relaxed);
        self.prepared_kegs
            .lock()
            .await
            .insert(package_id.clone(), keg);
        Ok(())
    }

    async fn keg_link(&self, package_id: &PackageId) -> Result<()> {
        let package = self.manifest.require_package(package_id)?.clone();
        let keg = self
            .prepared_kegs
            .lock()
            .await
            .get(package_id)
            .cloned()
            .with_context(|| format!("missing prepared keg for {}", package_id.0))?;
        let artifact = self.manifest.require_artifact(&package.artifact)?.clone();
        let prefix = self.prefix.clone();
        let force = self.options.force;
        let active = !package.exposure.is_isolated() && self.should_activate_package(&package);
        let receipt_linked = active;
        let links = tokio::task::spawn_blocking(move || {
            write_prepared_receipt(&keg, &package, &package.artifact, &artifact, false)?;
            #[cfg(test)]
            crate::install::fault::after_prepared_receipt(&package.name)?;
            let links = commit_prepared_keg(&prefix, &keg, &package, force, active)?;
            #[cfg(test)]
            crate::install::fault::after_commit(&package.name)?;
            Ok::<usize, anyhow::Error>(links)
        })
        .await
        .context("keg link task failed")??;
        self.stats.linked_files.fetch_add(links, Ordering::Relaxed);
        self.link_counts
            .lock()
            .await
            .insert(package_id.clone(), links);
        self.receipt_linked
            .lock()
            .await
            .insert(package_id.clone(), receipt_linked);
        Ok(())
    }

    fn should_activate_package(&self, package: &glu_core::ResolvedPackage) -> bool {
        !self.deactivated_names.contains(&package.name)
            && package.oldnames.iter().all(|oldname| {
                !self
                    .deactivated_names
                    .contains(&PackageName(oldname.0.clone()))
            })
    }

    async fn formula_postinstall(&self, package_id: &PackageId) -> Result<()> {
        let package = self.manifest.require_package(package_id)?.clone();
        let keg = self
            .prepared_kegs
            .lock()
            .await
            .get(package_id)
            .cloned()
            .with_context(|| format!("missing prepared keg for {}", package_id.0))?;
        let prefix = self.prefix.clone();
        let plan = self.postinstall_plans.plan_for(package_id)?.clone();
        let verbose = self.options.verbose;
        let deferred_postinstalls = self.deferred_postinstalls.clone();
        let events = Arc::clone(&self.events);
        tokio::task::spawn_blocking(move || {
            let mut deferred = deferred_postinstalls.blocking_lock();
            run_formula_postinstall_sandboxed(
                &prefix,
                &package,
                &keg.final_keg_path,
                &plan,
                &mut deferred,
                verbose,
                events.as_ref(),
            )
        })
        .await
        .context("formula postinstall task failed")??;
        Ok(())
    }

    async fn cache_postinstall(&self, node: &crate::install::dag::ExecNode) -> Result<()> {
        let kind = node.inputs["kind"]
            .as_str()
            .context("cache_postinstall missing kind")?;
        let key = node.inputs["key"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        let requested = {
            let mut deferred = self.deferred_postinstalls.lock().await;
            deferred.take(kind, &key)
        };
        let Some(item) = requested else {
            // The DAG node represents possible work. Runtime guards are
            // authoritative, so a guarded global step that did not actually
            // defer becomes a cheap no-op here.
            return Ok(());
        };
        let prefix = self.prefix.clone();
        let kind = kind.to_string();
        let verbose = self.options.verbose;
        let events = Arc::clone(&self.events);
        tokio::task::spawn_blocking(move || {
            run_deferred_global_postinstall_sandboxed(
                &prefix,
                &kind,
                &key,
                item.network_access_allowed,
                verbose,
                events.as_ref(),
            )
        })
        .await
        .context("cache postinstall task failed")??;
        self.stats
            .global_postinstalls
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn rename_existing(&self, node: &crate::install::dag::ExecNode) -> Result<()> {
        let package_id = node
            .package_id
            .as_ref()
            .context("rename node missing package_id")?;
        let package = self.manifest.require_package(package_id)?.clone();
        let package_id = package_id.clone();
        let prefix = self.prefix.clone();
        let old_name = PackageName(required_input(node, "old_name")?.to_string());
        let old_keg = PathBuf::from(required_input(node, "old_keg")?);
        let old_keg_version = required_input(node, "old_keg_version")?.to_string();
        let old_version = required_input(node, "old_version")?.to_string();
        let old_revision = node
            .inputs
            .get("old_revision")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        tokio::task::spawn_blocking(move || {
            rename_existing_keg(RenameExistingKegInput {
                prefix: &prefix,
                old_name: &old_name,
                old_keg: &old_keg,
                old_keg_version: &old_keg_version,
                old_version: &old_version,
                old_revision,
                package: &package,
                package_id: &package_id,
            })
        })
        .await
        .context("rename existing keg task failed")?
    }

    async fn registry_write(&self, package_id: &PackageId) -> Result<()> {
        let package = self.manifest.require_package(package_id)?.clone();
        let artifact = self.manifest.require_artifact(&package.artifact)?.clone();
        let keg = self
            .prepared_kegs
            .lock()
            .await
            .get(package_id)
            .cloned()
            .with_context(|| format!("missing prepared keg for {}", package_id.0))?;
        let linked = self
            .receipt_linked
            .lock()
            .await
            .get(package_id)
            .copied()
            .unwrap_or(false);
        tokio::task::spawn_blocking(move || {
            write_install_receipt(&keg, &package, &package.artifact, &artifact, linked)
        })
        .await
        .context("registry write task failed")?
    }
}

fn required_input<'a>(node: &'a crate::install::dag::ExecNode, key: &str) -> Result<&'a str> {
    node.inputs
        .get(key)
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("{} node missing input {key}", node.id))
}

struct RenameExistingKegInput<'a> {
    prefix: &'a Prefix,
    old_name: &'a PackageName,
    old_keg: &'a Path,
    old_keg_version: &'a str,
    old_version: &'a str,
    old_revision: u32,
    package: &'a glu_core::ResolvedPackage,
    package_id: &'a PackageId,
}

fn rename_existing_keg(input: RenameExistingKegInput<'_>) -> Result<()> {
    let RenameExistingKegInput {
        prefix,
        old_name,
        old_keg,
        old_keg_version,
        old_version,
        old_revision,
        package,
        package_id,
    } = input;
    let new_keg = prefix
        .0
        .join("Cellar")
        .join(&package.name.0)
        .join(old_keg_version);
    let store = InstalledStateStore::new(prefix.clone());

    let mut receipt = if old_keg.exists() {
        let mut receipt = store.read_receipt_for_keg(old_keg)?;
        if receipt.status != ReceiptStatus::Complete {
            bail!("cannot rename an incomplete installation of {}", old_name.0);
        }
        validate_rename_receipt(&receipt, old_name, old_version, old_revision)?;
        unlink_keg(prefix, old_name, old_keg)?;
        if new_keg.exists() {
            bail!(
                "cannot rename {} to {}; target keg already exists",
                old_keg.display(),
                new_keg.display()
            );
        }
        if let Some(parent) = new_keg.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // The directory move and metadata rewrite cannot be one filesystem
        // operation. Mark the old record incomplete first so interruption on
        // either side of the rename follows normal cleanup-and-retry recovery.
        receipt.status = ReceiptStatus::Incomplete;
        store.write_receipt_for_keg(old_keg, &receipt)?;
        std::fs::rename(old_keg, &new_keg)
            .with_context(|| format!("moving {} to {}", old_keg.display(), new_keg.display()))?;
        if let Some(old_rack) = old_keg.parent() {
            let _ = std::fs::remove_dir(old_rack);
        }
        receipt
    } else if new_keg.exists() {
        let receipt = InstalledStateStore::read_unbound_receipt_at_keg(&new_keg)?;
        if receipt.status != ReceiptStatus::Incomplete {
            bail!(
                "cannot resume renaming {}; target package metadata is not incomplete",
                old_name.0
            );
        }
        validate_rename_receipt(&receipt, old_name, old_version, old_revision)?;
        receipt
    } else {
        bail!(
            "cannot rename {}; neither source {} nor target {} exists",
            old_name.0,
            old_keg.display(),
            new_keg.display()
        );
    };

    validate_rename_receipt_name(&receipt, old_name, &package.name)?;
    receipt.status = ReceiptStatus::Complete;
    receipt.package.id = derived_package_id_for_keg(package_id, &package.name, old_keg_version);
    receipt.package.package_key = package.package_key.clone();
    receipt.package.name = package.name.clone();
    receipt.package.aliases = package.aliases.clone();
    let old_selector = glu_core::PackageSelector(old_name.0.clone());
    if !receipt.package.oldnames.contains(&old_selector) {
        receipt.package.oldnames.push(old_selector);
    }
    for oldname in &package.oldnames {
        if !receipt.package.oldnames.contains(oldname) {
            receipt.package.oldnames.push(oldname.clone());
        }
    }
    receipt.paths.keg = new_keg.clone();
    receipt.paths.opt = prefix.0.join("opt").join(&package.name.0);
    receipt.install.exposure = package.exposure.clone();
    receipt.install.link_overwrite = package.install.link_overwrite.clone();
    let link_package = glu_core::PackageLinkMetadata::from(package);
    receipt.links.opt_names = link_package.opt_names.clone();
    store.write_receipt_for_keg(&new_keg, &receipt)?;

    link_keg(prefix, &link_package, &new_keg)?;
    make_relative_symlink(&prefix.0.join("opt").join(&old_name.0), &new_keg, true)?;
    Ok(())
}

fn validate_rename_receipt(
    receipt: &GluInstallReceipt,
    old_name: &PackageName,
    old_version: &str,
    old_revision: u32,
) -> Result<()> {
    if receipt.package.name != *old_name {
        bail!(
            "cannot rename {}; source receipt belongs to {}",
            old_name.0,
            receipt.package.name.0
        );
    }
    if receipt.package.version != old_version || receipt.package.revision != old_revision {
        bail!(
            "cannot rename {}; installed version changed from {}_{} to {}_{}",
            old_name.0,
            old_version,
            old_revision,
            receipt.package.version,
            receipt.package.revision
        );
    }
    Ok(())
}

fn validate_rename_receipt_name(
    receipt: &GluInstallReceipt,
    old_name: &PackageName,
    new_name: &PackageName,
) -> Result<()> {
    if receipt.package.name != *old_name && receipt.package.name != *new_name {
        bail!(
            "cannot rename {}; receipt belongs to {}",
            old_name.0,
            receipt.package.name.0
        );
    }
    Ok(())
}

fn derived_package_id_for_keg(
    resolved_id: &PackageId,
    name: &PackageName,
    keg_version: &str,
) -> PackageId {
    let Some((source, _)) = resolved_id.0.rsplit_once('/') else {
        return resolved_id.clone();
    };
    PackageId(format!("{source}/{}@{}", name.0, keg_version))
}

fn is_sha256_mismatch(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Sha256Mismatch>())
}

impl InstallOperations for SchedulerInstallOperations {
    fn execute<'a>(
        &'a self,
        node: &'a crate::install::dag::ExecNode,
        ctx: ExecutionContext,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            use crate::install::dag::NodeKind;
            match node.kind {
                NodeKind::GhcrAuth => self.ghcr_auth().await,
                NodeKind::GhcrBottleDownload => {
                    let package_id = node
                        .package_id
                        .as_ref()
                        .context("download node missing package_id")?;
                    let cached = node
                        .inputs
                        .get("cached")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    self.ghcr_bottle_download(package_id, cached, node.priority as u64)
                        .await
                }
                NodeKind::BottlePrepare => {
                    let package_id = node
                        .package_id
                        .as_ref()
                        .context("prepare node missing package_id")?;
                    self.bottle_prepare(package_id, ctx, &node.id, node.pool.as_str(), node.slot)
                        .await
                }
                NodeKind::KegLink => {
                    let package_id = node
                        .package_id
                        .as_ref()
                        .context("link node missing package_id")?;
                    self.keg_link(package_id).await
                }
                NodeKind::FormulaPostinstall => {
                    let package_id = node
                        .package_id
                        .as_ref()
                        .context("postinstall node missing package_id")?;
                    self.formula_postinstall(package_id).await
                }
                NodeKind::CachePostinstall => self.cache_postinstall(node).await,
                NodeKind::RegistryWrite => {
                    let package_id = node
                        .package_id
                        .as_ref()
                        .context("registry node missing package_id")?;
                    self.registry_write(package_id).await
                }
                // Already installed and linked — nothing to do. This node
                // is an ordering/presence anchor so satisfied deps appear in
                // the plan graph (see dag.rs's satisfied-package loop).
                NodeKind::KegLinkExisting => Ok(()),
                NodeKind::KegRenameExisting => self.rename_existing(node).await,
            }
        }
        .boxed()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths};
    use glu_core::{ArtifactId, KegVersion, PackageInstallMetadata, ResolvedPackage};
    use tempfile::TempDir;

    fn package(name: &str, oldnames: Vec<&str>) -> (PackageId, ResolvedPackage) {
        let id = PackageId(format!("pkg:homebrew/core/{name}@1.1"));
        (
            id.clone(),
            ResolvedPackage {
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: oldnames
                    .into_iter()
                    .map(|oldname| glu_core::PackageSelector(oldname.to_string()))
                    .collect(),
                version: "1.1".to_string(),
                revision: 0,
                keg_version: KegVersion("1.1".to_string()),
                deps: Vec::new(),
                dependency_requirements: Default::default(),
                exposure: glu_core::Exposure::Global,
                artifact: ArtifactId("art:test".to_string()),
                install: PackageInstallMetadata {
                    opt_names: Vec::new(),
                    link_overwrite: Vec::new(),
                    post_install_defined: false,
                    post_install_steps: Vec::new(),
                    postinstall_network_access_allowed: true,
                },
            },
        )
    }

    #[test]
    fn rename_existing_keg_moves_receipt_and_links_current_plus_compat_opt() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let old_keg = prefix.0.join("Cellar/foo/1.0");
        std::fs::create_dir_all(old_keg.join(".glu")).unwrap();
        std::fs::create_dir_all(old_keg.join("bin")).unwrap();
        std::fs::write(old_keg.join("bin/tool"), b"tool").unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Complete,
            package: ReceiptPackage {
                id: PackageId("pkg:homebrew/core/foo@1.0".to_string()),
                package_key: glu_core::PackageKey("package:foo".to_string()),
                name: PackageName("foo".to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: "1.0".to_string(),
                revision: 0,
                keg_version: KegVersion("1.0".to_string()),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId("art:test".to_string()),
                sha256: "deadbeef".to_string(),
                bottle_tag: "arm64_test".to_string(),
                cellar: ":any".to_string(),
            },
            sizes: crate::state::ReceiptSizes::default(),
            paths: ReceiptPaths {
                keg: old_keg.clone(),
                opt: prefix.0.join("opt/foo"),
            },
            links: crate::state::ReceiptLinkNames {
                opt_names: Vec::new(),
            },
            install: ReceiptInstall {
                exposure: glu_core::Exposure::Global,
                linked: true,
                link_overwrite: Vec::new(),
                deps: Vec::new(),
                dependency_requirements: Default::default(),
            },
        };
        InstalledStateStore::new(prefix.clone())
            .write_receipt_for_keg(&old_keg, &receipt)
            .unwrap();
        let (package_id, mut package) = package("bar", vec!["foo"]);
        package.exposure = glu_core::Exposure::Isolated {
            reason: Some("Conflicts with another package".to_string()),
        };

        let old_name = PackageName("foo".to_string());
        rename_existing_keg(RenameExistingKegInput {
            prefix: &prefix,
            old_name: &old_name,
            old_keg: &old_keg,
            old_keg_version: "1.0",
            old_version: "1.0",
            old_revision: 0,
            package: &package,
            package_id: &package_id,
        })
        .unwrap();

        let new_keg = prefix.0.join("Cellar/bar/1.0");
        assert!(!old_keg.exists());
        assert!(new_keg.exists());
        let receipt = InstalledStateStore::new(prefix.clone())
            .read_receipt_for_keg(&new_keg)
            .unwrap();
        assert_eq!(receipt.package.name.0, "bar");
        assert_eq!(receipt.package.keg_version.0, "1.0");
        assert!(receipt
            .package
            .oldnames
            .contains(&glu_core::PackageSelector("foo".to_string())));
        assert_eq!(receipt.paths.keg, new_keg);
        assert_eq!(receipt.paths.opt, prefix.0.join("opt/bar"));
        assert_eq!(receipt.install.exposure, package.exposure);
        assert!(!prefix.0.join("bin/tool").exists());
        assert_eq!(
            prefix.0.join("opt/bar").canonicalize().unwrap(),
            prefix.0.join("Cellar/bar/1.0").canonicalize().unwrap()
        );
        assert_eq!(
            prefix.0.join("opt/foo").canonicalize().unwrap(),
            prefix.0.join("Cellar/bar/1.0").canonicalize().unwrap()
        );
    }

    #[test]
    fn rename_existing_keg_resumes_after_source_move_before_receipt_rewrite() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let new_keg = prefix.0.join("Cellar/bar/1.0");
        std::fs::create_dir_all(new_keg.join(".glu")).unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Incomplete,
            package: ReceiptPackage {
                id: PackageId("pkg:homebrew/core/foo@1.0".to_string()),
                package_key: glu_core::PackageKey("package:foo".to_string()),
                name: PackageName("foo".to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: "1.0".to_string(),
                revision: 0,
                keg_version: KegVersion("1.0".to_string()),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId("art:test".to_string()),
                sha256: "deadbeef".to_string(),
                bottle_tag: "arm64_test".to_string(),
                cellar: ":any".to_string(),
            },
            sizes: crate::state::ReceiptSizes::default(),
            paths: ReceiptPaths {
                keg: prefix.0.join("Cellar/foo/1.0"),
                opt: prefix.0.join("opt/foo"),
            },
            links: crate::state::ReceiptLinkNames {
                opt_names: Vec::new(),
            },
            install: ReceiptInstall {
                exposure: glu_core::Exposure::Global,
                linked: true,
                link_overwrite: Vec::new(),
                deps: Vec::new(),
                dependency_requirements: Default::default(),
            },
        };
        InstalledStateStore::new(prefix.clone())
            .write_receipt_for_keg(&new_keg, &receipt)
            .unwrap();
        let (package_id, package) = package("bar", vec!["foo"]);

        let old_name = PackageName("foo".to_string());
        let old_keg = prefix.0.join("Cellar/foo/1.0");
        rename_existing_keg(RenameExistingKegInput {
            prefix: &prefix,
            old_name: &old_name,
            old_keg: &old_keg,
            old_keg_version: "1.0",
            old_version: "1.0",
            old_revision: 0,
            package: &package,
            package_id: &package_id,
        })
        .unwrap();

        let receipt = InstalledStateStore::new(prefix.clone())
            .read_receipt_for_keg(&new_keg)
            .unwrap();
        assert_eq!(receipt.package.name.0, "bar");
        assert_eq!(receipt.paths.keg, new_keg);
        assert_eq!(
            prefix.0.join("opt/foo").canonicalize().unwrap(),
            prefix.0.join("Cellar/bar/1.0").canonicalize().unwrap()
        );
    }

    #[test]
    fn sha256_retry_detection_uses_typed_error_not_message_text() {
        let typed = anyhow::Error::new(Sha256Mismatch::new("cache", "expected", "actual"))
            .context("preparing cached bottle");
        assert!(is_sha256_mismatch(&typed));

        let text_only = anyhow::anyhow!("sha256 mismatch for cache: expected expected, got actual");
        assert!(!is_sha256_mismatch(&text_only));
    }
}
