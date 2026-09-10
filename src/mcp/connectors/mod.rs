//! Connector subsystem — marketplace-style packaging for MCP servers and
//! CLI-backed integrations.
//!
//! A *connector* is a versioned package (a directory with a
//! [`connector.json`](manifest::ConnectorManifest)) that can provide any mix
//! of:
//!
//! - an **MCP server** (`mcp` section) connected through [`McpManager`] when
//!   the connector is enabled;
//! - **lifecycle hooks** (`lifecycle` section): per-platform dependency
//!   install, version verification, and auth commands, executed by the
//!   [`LifecycleRunner`];
//! - **bundled skills** (`skills` section): `SKILL.md` folders installed into
//!   the user skills directory so the agent learns how to drive the service.
//!
//! State is persisted in a versioned [`states.json`](state) file; remote
//! catalogs are synced through [`catalog::CatalogCache`] with ETag/304
//! semantics and sha256 archive verification.
//!
//! Layout under [`crate::dirs::connectors_dir()`]:
//!
//! ```text
//! ├── states.json                     # ConnectorStateFile (versioned)
//! ├── catalog/{catalog.json,meta.json}
//! └── cache/<connector-id>/<semver>/  # installed packages (connector.json at root)
//! ```

pub mod catalog;
pub mod lifecycle;
pub mod manifest;
pub mod state;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

use crate::error::SyscityError;
use crate::skills::SkillStorage;

use self::catalog::{CatalogCache, CatalogDocument, PendingUpdate};
use self::lifecycle::{LifecycleRunner, VersionCheckOutcome};
use self::manifest::ConnectorManifest;
use self::state::{new_record, put_record, ConnectorRecord, StateKind, StateStore};
use super::McpManager;

// ─────────────────────────────────────────────
// Public summary type
// ─────────────────────────────────────────────

/// Read-only view of an installed connector for listings and tool output.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectorSummary {
    /// Connector id.
    pub id: String,
    /// Installed version.
    pub version: String,
    /// Display name from the manifest.
    pub display_name: String,
    /// Description from the manifest.
    pub description: String,
    /// Lifecycle state.
    pub state: StateKind,
    /// Last error, when `state == Error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether enabling opens an MCP connection.
    pub provides_mcp: bool,
    /// Live MCP connection state for `provides_mcp` connectors; `None` for
    /// skills/experts and for callers that didn't probe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected: Option<bool>,
    /// Bundled skill names installed on this connector's behalf.
    pub skills: Vec<String>,
}

// ─────────────────────────────────────────────
// Manager
// ─────────────────────────────────────────────

/// Facade over the connector package cache, persistent state machine, bundled
/// skill installation, lifecycle execution, MCP connection, and catalog sync.
///
/// Mutating operations serialize through one write lock — installs touch the
/// filesystem, the skills dir, and [`McpManager`], and tool calls may arrive
/// concurrently from multiple sessions.
pub struct ConnectorManager {
    root: PathBuf,
    mcp_manager: Arc<McpManager>,
    skill_storage: Arc<SkillStorage>,
    lifecycle: LifecycleRunner,
    /// Serializes state-mutating operations.
    write_lock: AsyncMutex<()>,
    /// Syscity Cloud API base (feature `cloud`): kind=cloud connectors route
    /// through the cloud MCP relay instead of a local MCP server.
    #[cfg(feature = "cloud")]
    cloud_api_base: Option<String>,
    /// Secret-store handle for reading the cloud session token during catalog
    /// sync (attached to the request as a Bearer token).
    secrets: Arc<crate::secrets::SecretStoreHandle>,
}

impl std::fmt::Debug for ConnectorManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Lifecycle runner and storage handles carry no secrets, but they are
        // not Debug; summarize by location instead.
        f.debug_struct("ConnectorManager")
            .field("root", &self.root)
            .finish()
    }
}

impl ConnectorManager {
    /// Manager rooted at `root` (normally [`crate::dirs::connectors_dir()]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: PathBuf,
        mcp_manager: Arc<McpManager>,
        skill_storage: Arc<SkillStorage>,
        #[cfg(feature = "cloud")] cloud_api_base: Option<String>,
        secrets: Arc<crate::secrets::SecretStoreHandle>,
    ) -> Self {
        Self {
            root,
            mcp_manager,
            skill_storage,
            lifecycle: LifecycleRunner::new(300),
            write_lock: AsyncMutex::new(()),
            #[cfg(feature = "cloud")]
            cloud_api_base,
            secrets,
        }
    }

    /// The `McpServerConfig` a connector should use: its local MCP config when
    /// present, else (feature `cloud`) a cloud-relay config for kind=cloud
    /// connectors, else `None` (CLI-only connectors have no MCP server).
    fn server_config_for(
        &self,
        _id: &str,
        manifest: &ConnectorManifest,
    ) -> Option<crate::mcp::McpServerConfig> {
        if let Some(mcp) = &manifest.mcp {
            return Some(mcp.to_server_config());
        }
        #[cfg(feature = "cloud")]
        if manifest.kind == "cloud" {
            if let Some(api_base) = &self.cloud_api_base {
                return Some(crate::mcp::McpServerConfig {
                    transport: crate::mcp::McpTransport::Cloud {
                        connector_id: _id.to_string(),
                        api_base: api_base.clone(),
                    },
                    ..Default::default()
                });
            }
        }
        None
    }

    /// Whether `manifest` is a cloud-provisioned connector (`kind == "cloud"`),
    /// independent of the cloud feature — callers gate on availability.
    fn is_cloud_connector(manifest: &ConnectorManifest) -> bool {
        manifest.kind == "cloud"
    }

    fn state_store(&self) -> StateStore {
        StateStore::new(&self.root)
    }

    fn catalog_cache(&self) -> CatalogCache {
        CatalogCache::new(self.root.join("catalog"))
    }

    fn cache_root(&self) -> PathBuf {
        self.root.join("cache")
    }

    fn package_dir(&self, id: &str, version: &str) -> PathBuf {
        self.cache_root().join(id).join(version)
    }

    async fn load_manifest_for(
        &self,
        record: &ConnectorRecord,
    ) -> crate::Result<ConnectorManifest> {
        let pkg = self.package_dir(&record.id, &record.version);
        ConnectorManifest::load(&pkg).await.map_err(|e| match e {
            SyscityError::NotFound { .. } => SyscityError::NotFound {
                resource: format!(
                    "connector {} v{} missing from cache ({})",
                    record.id,
                    record.version,
                    pkg.display()
                ),
            },
            other => other,
        })
    }

    async fn require_record(&self, id: &str) -> crate::Result<ConnectorRecord> {
        let file = self.state_store().load().await?;
        file.connectors
            .get(id)
            .cloned()
            .ok_or_else(|| SyscityError::NotFound {
                resource: format!("connector {id} is not installed"),
            })
    }

    // ── Install / remove ─────────────────────────────────────────────────

    /// Install a connector from a local package directory.
    ///
    /// Copies the package into the versioned cache, installs bundled skills,
    /// runs the declared `init` + `version_check` hooks, records the state,
    /// and — when the manifest has an auto-connect `mcp` section — enables it.
    ///
    /// On hook failure the package stays cached as `Installed` with the reason
    /// recorded on its error field, and the error propagates to the caller.
    pub async fn install_from_dir(&self, source_dir: &Path) -> crate::Result<ConnectorSummary> {
        let manifest = ConnectorManifest::load(source_dir).await?;
        let id = manifest.connector.id.clone();
        let version = if manifest.connector.version.is_empty() {
            "0.0.0".to_string()
        } else {
            manifest.connector.version.clone()
        };

        let _guard = self.write_lock.lock().await;
        let dest = self.package_dir(&id, &version);
        // `upgrade()` hands us the package already living at `dest` (the
        // catalog cache rename targeted it); copying onto itself would wipe
        // it, so only copy genuinely external source dirs.
        if source_dir != dest {
            copy_dir_recursive(source_dir, &dest).await?;
        }

        // Bundled-skills bridge: install every discovered SKILL.md directory
        // under a connector-prefixed flat name into the user skills directory.
        // Each `skills` entry may be one skill folder itself or a folder of
        // skill folders (both shapes exist in the wild).
        let mut installed_skills = Vec::new();
        for rel in &manifest.skills {
            let base = source_dir.join(rel);
            let discovered = discover_skill_dirs(&base).ok_or_else(|| {
                SyscityError::Validation(format!(
                    "connector {id}: declared skill path '{rel}' not found"
                ))
            })?;
            if discovered.is_empty() {
                rollback_skills(&self.skill_storage, &installed_skills).await;
                let _ = tokio::fs::remove_dir_all(&dest).await;
                return Err(SyscityError::Validation(format!(
                    "connector {id}: no SKILL.md found under '{rel}'"
                )));
            }
            for (leaf, skill_dir) in discovered {
                let name = format!("connector-{id}-{leaf}");
                match self.skill_storage.install_to_user(&skill_dir, &name).await {
                    Ok(_) => installed_skills.push(name),
                    Err(e) => {
                        rollback_skills(&self.skill_storage, &installed_skills).await;
                        let _ = tokio::fs::remove_dir_all(&dest).await;
                        return Err(e);
                    }
                }
            }
        }

        let store = self.state_store();
        let mut record = new_record(&id, &version);
        record.skills = installed_skills;

        // Lifecycle hooks run after unpack; failures leave the package in
        // place (as `Installed` with the reason recorded) so the user can fix
        // auth/deps and retry enable.
        if let Err(e) = self.run_hooks(&manifest).await {
            record.error = Some(e.to_string());
            store
                .update(|f| {
                    put_record(f, record);
                    Ok(())
                })
                .await?;
            return Err(e);
        }

        store
            .update(|f| {
                put_record(f, record.clone());
                Ok(())
            })
            .await?;
        info!("Installed connector {id} v{version}");

        // Auto-enable connectors that ship an always-on MCP server.
        if let Some(mcp) = &manifest.mcp {
            if mcp.auto_connect {
                drop(_guard);
                return self.enable(&id).await;
            }
        }
        Ok(self
            .with_live_connection(self.summary_from_parts(record, Some(&manifest)))
            .await)
    }

    /// Install a skill-type catalog package (`type=skill`): the archive is a
    /// bare `SKILL.md` folder (no connector.json manifest), so the folder is
    /// copied into the package cache and every discovered skill is installed
    /// into the user skills directory under `skill-<id>` (or
    /// `skill-<id>-<leaf>` for multi-skill packages).
    ///
    /// A state record is written so catalog "installed" detection and the
    /// update pipeline see the package; the returned summary reports
    /// `provides_mcp: false` (no MCP server, nothing to enable).
    async fn install_skill_package(
        &self,
        entry: &catalog::CatalogEntry,
        source_dir: &Path,
    ) -> crate::Result<ConnectorSummary> {
        let _guard = self.write_lock.lock().await;
        let dest = self.package_dir(&entry.id, &entry.version);
        // Same in-place rule as `install_from_dir`: `upgrade()` passes the
        // package root already living at `dest`.
        if source_dir != dest {
            copy_dir_recursive(source_dir, &dest).await?;
        }

        let discovered = discover_skill_dirs(&dest).unwrap_or_default();
        if discovered.is_empty() {
            let _ = tokio::fs::remove_dir_all(&dest).await;
            return Err(SyscityError::Validation(format!(
                "skill package {}: no SKILL.md found",
                entry.id
            )));
        }

        let single = discovered.len() == 1;
        let mut installed_skills = Vec::new();
        for (leaf, skill_dir) in &discovered {
            let name = if single {
                format!("skill-{}", entry.id)
            } else {
                format!("skill-{}-{leaf}", entry.id)
            };
            match self.skill_storage.install_to_user(skill_dir, &name).await {
                Ok(_) => installed_skills.push(name),
                Err(e) => {
                    rollback_skills(&self.skill_storage, &installed_skills).await;
                    let _ = tokio::fs::remove_dir_all(&dest).await;
                    return Err(e);
                }
            }
        }

        let store = self.state_store();
        let mut record = new_record(&entry.id, &entry.version);
        record.skills = installed_skills.clone();
        store
            .update(|f| {
                put_record(f, record);
                Ok(())
            })
            .await?;
        info!("Installed skill package {} v{}", entry.id, entry.version);

        Ok(ConnectorSummary {
            id: entry.id.clone(),
            version: entry.version.clone(),
            display_name: if entry.display_name.is_empty() {
                entry.id.clone()
            } else {
                entry.display_name.clone()
            },
            description: entry.description.clone(),
            state: StateKind::Installed,
            error: None,
            provides_mcp: false,
            connected: None,
            skills: installed_skills,
        })
    }

    /// Install an expert role package (`type=expert`, §3.6): extract the
    /// archive and copy its role definition(s) into the agents directory so
    /// the expert becomes summonable as an agent (new session bound to its id).
    ///
    /// Recognized layouts:
    /// - `SOUL.md` at the package root → a single agent named after the entry id;
    /// - `agents/<id>/SOUL.md` → one agent per subdirectory.
    ///
    /// Optional `skills/` dirs are installed like a connector's bundled skills.
    /// The caller must re-run the agent registry `discover()` afterwards so the
    /// new personality is visible to on-demand spawn.
    ///
    /// Returns the installed agent ids.
    pub async fn install_expert(
        &self,
        entry: &catalog::CatalogEntry,
    ) -> crate::Result<Vec<String>> {
        let cache_root = self.cache_root();
        let dest = self
            .catalog_cache()
            .install_entry(entry, &cache_root)
            .await?;
        let agents_dir = crate::dirs::agents_dir();
        let mut installed = Vec::new();

        install_agent_roles(&dest, &agents_dir, &entry.id, &mut installed).await?;
        if installed.is_empty() {
            return Err(crate::error::SyscityError::Validation(format!(
                "expert {}: no role definition found (expected SOUL.md at package root \
                 or agents/<id>/SOUL.md)",
                entry.id
            )));
        }

        // Bundled skills (optional): `skills/` dir, mirroring the connector bridge.
        let skills_dir = dest.join("skills");
        if skills_dir.is_dir() {
            if let Some(discovered) = discover_skill_dirs(&skills_dir) {
                for (leaf, skill_dir) in discovered {
                    let name = format!("expert-{}-{leaf}", entry.id);
                    if let Err(e) = self.skill_storage.install_to_user(&skill_dir, &name).await {
                        warn!("Expert {} skill '{leaf}' install failed: {e}", entry.id);
                    }
                }
            }
        }

        Ok(installed)
    }

    /// Enable a connector: connect its MCP server (if any) and flip state.
    pub async fn enable(&self, id: &str) -> crate::Result<ConnectorSummary> {
        let _guard = self.write_lock.lock().await;
        let store = self.state_store();
        let record = self.require_record(id).await?;
        let manifest = self.load_manifest_for(&record).await?;
        let mut next = record.clone();

        // A cloud-provisioned connector has no local MCP server; it is usable
        // only when cloud mode is active (feature + cloud.enabled + session).
        // Fail loudly instead of enabling an inert connector with zero tools.
        if Self::is_cloud_connector(&manifest) && self.server_config_for(id, &manifest).is_none() {
            let reason = "connector is cloud-provisioned but cloud mode is not available \
                          (enable the cloud feature, set SYSCITY_CLOUD_ENABLED=1 and sign in)"
                .to_string();
            next.state = StateKind::Error;
            next.error = Some(reason.clone());
            next.updated_at = Utc::now();
            store
                .update(|f| {
                    put_record(f, next.clone());
                    Ok(())
                })
                .await?;
            return Err(crate::error::SyscityError::Internal(reason));
        }

        if let Some(cfg) = self.server_config_for(id, &manifest) {
            match self.mcp_manager.connect(id, cfg).await {
                Ok(tools) => {
                    info!("Connector {id} connected ({} tools)", tools.len());
                }
                Err(e) => {
                    next.state = StateKind::Error;
                    next.error = Some(e.to_string());
                    next.updated_at = Utc::now();
                    store
                        .update(|f| {
                            put_record(f, next.clone());
                            Ok(())
                        })
                        .await?;
                    return Err(e);
                }
            }
        }

        next.state = StateKind::Enabled;
        next.error = None;
        next.updated_at = Utc::now();
        store
            .update(|f| {
                put_record(f, next.clone());
                Ok(())
            })
            .await?;
        Ok(self
            .with_live_connection(self.summary_from_parts(next, Some(&manifest)))
            .await)
    }

    /// Disable a connector: drop its MCP connection and park it as `Disabled`.
    pub async fn disable(&self, id: &str) -> crate::Result<ConnectorSummary> {
        let _guard = self.write_lock.lock().await;
        let store = self.state_store();
        let mut record = self.require_record(id).await?;

        if record.state == StateKind::Enabled {
            if let Err(e) = self.mcp_manager.disconnect(id).await {
                warn!("Connector {id} disconnect failed: {e}");
            }
        }
        record.state = StateKind::Disabled;
        record.updated_at = Utc::now();
        store
            .update(|f| {
                put_record(f, record.clone());
                Ok(())
            })
            .await?;
        let manifest = self.load_manifest_for(&record).await.ok();
        Ok(self
            .with_live_connection(self.summary_from_parts(record, manifest.as_ref()))
            .await)
    }

    /// Remove a connector entirely: bundled skills, cached package, record.
    ///
    /// Refused while `Enabled` — disable first so the MCP connection is torn
    /// down before its package disappears.
    pub async fn uninstall(&self, id: &str) -> crate::Result<()> {
        let _guard = self.write_lock.lock().await;
        let store = self.state_store();
        let record = self.require_record(id).await?;

        if record.state == StateKind::Enabled {
            return Err(SyscityError::Validation(format!(
                "connector {id} is enabled; disable it before uninstalling"
            )));
        }

        for skill in &record.skills {
            if let Err(e) = self.skill_storage.uninstall_from_user(skill).await {
                warn!("Failed to remove bundled skill {skill}: {e}");
            }
        }

        let pkg = self.package_dir(id, &record.version);
        if pkg.exists() {
            tokio::fs::remove_dir_all(&pkg)
                .await
                .map_err(|e| SyscityError::IoContext {
                    context: format!("Failed to remove {}", pkg.display()),
                    source: e,
                })?;
        }

        store
            .update(|f| {
                f.connectors.remove(id);
                Ok(())
            })
            .await?;
        info!("Uninstalled connector {id}");
        Ok(())
    }

    // ── Listing ───────────────────────────────────────────────────────────

    /// List every connector known to the subsystem (cache entries plus state
    /// records), joined with manifests where available.
    pub async fn list(&self) -> crate::Result<Vec<ConnectorSummary>> {
        let file = self.state_store().load().await?;

        let cache_root = self.cache_root();
        let mut ids: Vec<String> = match tokio::fs::read_dir(&cache_root).await {
            Ok(mut rd) => {
                let mut names = Vec::new();
                while let Some(entry) = rd.next_entry().await? {
                    if entry.path().is_dir() {
                        if let Ok(name) = entry.file_name().into_string() {
                            names.push(name);
                        }
                    }
                }
                names
            }
            Err(_) => Vec::new(),
        };
        // Records whose cache entry vanished still show up (degraded view).
        for key in file.connectors.keys() {
            if !ids.contains(key) {
                ids.push(key.clone());
            }
        }
        ids.sort();

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let record = file.connectors.get(&id).cloned();
            let manifest = match record.as_ref().map(|r| r.version.clone()) {
                Some(version) => ConnectorManifest::load(&self.package_dir(&id, &version))
                    .await
                    .ok(),
                None => None,
            };
            let Some(summary) = self.summary_from_parts_opt(record, manifest.as_ref(), &id) else {
                continue;
            };
            out.push(self.with_live_connection(summary).await);
        }
        Ok(out)
    }

    // ── Lifecycle passthroughs ────────────────────────────────────────────

    /// Run the connector's interactive auth login.
    pub async fn auth_login(&self, id: &str) -> crate::Result<lifecycle::LifecycleOutput> {
        let manifest = self.manifest_by_id(id).await?;
        self.lifecycle.run_auth_login(&manifest).await
    }

    /// Probe the connector's auth status; `None` when unauthenticated /
    /// undeclared.
    pub async fn auth_status(&self, id: &str) -> crate::Result<Option<String>> {
        let manifest = self.manifest_by_id(id).await?;
        self.lifecycle.run_auth_status(&manifest).await
    }

    /// Run the connector's auth logout / credential reset.
    pub async fn auth_logout(&self, id: &str) -> crate::Result<()> {
        let manifest = self.manifest_by_id(id).await?;
        self.lifecycle.run_auth_logout(&manifest).await
    }

    /// Verify the connector's runtime against its declared version requirement.
    pub async fn check_version(&self, id: &str) -> crate::Result<VersionCheckOutcome> {
        let manifest = self.manifest_by_id(id).await?;
        self.lifecycle.run_version_check(&manifest).await
    }

    async fn manifest_by_id(&self, id: &str) -> crate::Result<ConnectorManifest> {
        let record = self.require_record(id).await?;
        self.load_manifest_for(&record).await
    }

    // ── Catalog operations ────────────────────────────────────────────────

    /// Refresh the remote catalog. Returns `(document, refreshed)`.
    ///
    /// When a cloud session token is stored it is attached to the request
    /// (P1-4), so the catalog carries the login-visible view (member entries).
    /// `lang` is forwarded as `Accept-Language` (ETags are per-language —
    /// keep it stable between calls or expect one full refresh per change).
    pub async fn sync_catalog(
        &self,
        url: &str,
        lang: Option<&str>,
    ) -> crate::Result<(CatalogDocument, bool)> {
        #[cfg(feature = "cloud")]
        let token = crate::cloud::session::get_token(&self.secrets).await;
        #[cfg(not(feature = "cloud"))]
        let token: Option<String> = None;
        self.catalog_cache().sync(url, token.as_deref(), lang).await
    }

    /// Locally cached catalog document (no network).
    pub async fn cached_catalog(&self) -> crate::Result<Option<CatalogDocument>> {
        self.catalog_cache().cached().await
    }

    /// Language bucket (`"en"` | `"zh"`) of the locally cached catalog, if
    /// known (`meta.json`; `None` for pre-lang caches or no cache).
    pub async fn cached_catalog_lang(&self) -> Option<String> {
        self.catalog_cache().cached_lang().await
    }

    /// Updates available according to the locally cached catalog.
    pub async fn check_updates(&self) -> crate::Result<Vec<PendingUpdate>> {
        let Some(doc) = self.cached_catalog().await? else {
            return Ok(Vec::new());
        };
        let file = self.state_store().load().await?;
        let installed = file
            .connectors
            .values()
            .map(|r| (r.id.clone(), r.version.clone()));
        Ok(catalog::diff_updates(installed, &doc.connectors))
    }

    /// Upgrade or fresh-install a single connector from a catalog entry.
    ///
    /// An `Enabled` connector is disabled before the swap and re-enabled after.
    pub async fn upgrade(&self, entry: &catalog::CatalogEntry) -> crate::Result<ConnectorSummary> {
        let cache_root = self.cache_root();
        let dest = self
            .catalog_cache()
            .install_entry(entry, &cache_root)
            .await?;

        let previous = self.require_record(&entry.id).await.ok();
        let was_enabled = previous
            .as_ref()
            .is_some_and(|r| r.state == StateKind::Enabled);
        if was_enabled {
            self.disable(&entry.id).await?;
        }

        // Pure skill packages (SKILL.md folder, no connector.json manifest)
        // install straight into the user skills dir; everything else is a
        // manifest-driven connector install.
        let summary = if entry.entry_type == "skill" && !dest.join("connector.json").exists() {
            self.install_skill_package(entry, &dest).await?
        } else {
            self.install_from_dir(&dest).await?
        };

        if let Some(prev) = &previous {
            if prev.version != entry.version && prev.skills != summary.skills {
                // Skill name sets can change between versions; drop stale ones.
                for stale in &prev.skills {
                    if !summary.skills.contains(stale) {
                        let _ = self.skill_storage.uninstall_from_user(stale).await;
                    }
                }
            }
            if prev.version != entry.version {
                let old = self.package_dir(&entry.id, &prev.version);
                if old.exists() {
                    let _ = tokio::fs::remove_dir_all(old).await;
                }
            }
        }

        if was_enabled && summary.provides_mcp {
            return self.enable(&entry.id).await;
        }
        Ok(summary)
    }

    /// Apply pending updates; when `auto_only`, restrict to entries flagged
    /// `auto_update`. Failures are logged per-entry, never abort the batch.
    pub async fn apply_updates(&self, auto_only: bool) -> crate::Result<Vec<String>> {
        let updates = self.check_updates().await?;
        let mut applied = Vec::new();
        for update in updates {
            if auto_only && !update.entry.auto_update {
                continue;
            }
            match self.upgrade(&update.entry).await {
                Ok(_) => applied.push(update.id.clone()),
                Err(e) => warn!("Auto-update of connector {} failed: {e}", update.id),
            }
        }
        Ok(applied)
    }

    // ── Startup rehydration ───────────────────────────────────────────────

    /// Reconnect every `Enabled` MCP-backed connector after a restart.
    ///
    /// Best effort per connector: failures mark that connector `Error` and are
    /// logged, never propagated. Awaited by the gateway inside a spawned task
    /// so startup stays non-blocking.
    pub async fn load_and_connect(&self) -> usize {
        let file = match self.state_store().load().await {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to load connector states: {e}");
                return 0;
            }
        };
        let enabled: Vec<ConnectorRecord> = file
            .connectors
            .values()
            .filter(|r| r.state == StateKind::Enabled)
            .cloned()
            .collect();

        let mut connected = 0;
        for record in enabled {
            let manifest = match self.load_manifest_for(&record).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("Connector {} cannot be loaded: {e}", record.id);
                    continue;
                }
            };
            let Some(cfg) = self.server_config_for(&record.id, &manifest) else {
                // CLI-only connectors have nothing to reconnect.
                connected += 1;
                continue;
            };
            match self.mcp_manager.connect(&record.id, cfg).await {
                Ok(tools) => {
                    info!("Connector {} reconnected ({} tools)", record.id, tools.len());
                    connected += 1;
                }
                Err(e) => {
                    warn!("Connector {} failed to reconnect: {e}", record.id);
                    self.mark_error(&record, e.to_string()).await;
                }
            }
        }
        connected
    }

    async fn run_hooks(&self, manifest: &ConnectorManifest) -> crate::Result<()> {
        self.lifecycle.run_init(manifest).await?;
        // Unverifiable declarations are non-fatal by design (logged upstream).
        let outcome = self.lifecycle.run_version_check(manifest).await?;
        let _ = outcome;
        Ok(())
    }

    async fn mark_error(&self, record: &ConnectorRecord, message: String) {
        let mut rec = record.clone();
        rec.state = StateKind::Error;
        rec.error = Some(message);
        rec.updated_at = Utc::now();
        if let Err(save_err) = self
            .state_store()
            .update(|f| {
                put_record(f, rec);
                Ok(())
            })
            .await
        {
            warn!("Failed to persist connector error state: {save_err}");
        }
    }

    fn summary_from_parts(
        &self,
        record: ConnectorRecord,
        manifest: Option<&ConnectorManifest>,
    ) -> ConnectorSummary {
        let meta = manifest.map(|m| &m.connector);
        ConnectorSummary {
            id: record.id.clone(),
            version: record.version.clone(),
            display_name: meta
                .map(|m| m.display_name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| record.id.clone()),
            description: meta.map(|m| m.description.clone()).unwrap_or_default(),
            state: record.state,
            error: record.error,
            provides_mcp: manifest.as_ref().is_some_and(|m| m.provides_mcp()),
            connected: None,
            skills: record.skills,
        }
    }

    /// Patch live MCP connection state into a summary (probes [`McpManager`]).
    async fn with_live_connection(&self, mut s: ConnectorSummary) -> ConnectorSummary {
        if s.provides_mcp {
            let servers = self.mcp_manager.list_servers().await;
            s.connected = Some(servers.contains(&s.id));
        }
        s
    }

    /// Build a summary from whichever pieces exist. Returns `None` only when
    /// neither a record nor a manifest exists (nothing knowable about `id`).
    fn summary_from_parts_opt(
        &self,
        record: Option<ConnectorRecord>,
        manifest: Option<&ConnectorManifest>,
        fallback_id: &str,
    ) -> Option<ConnectorSummary> {
        let meta = manifest.map(|m| &m.connector);
        let record = match record {
            Some(r) => r,
            None => {
                // Cache entry without a state record — synthesize a fresh one.
                let m = meta?;
                let id = if m.id.is_empty() {
                    fallback_id.to_string()
                } else {
                    m.id.clone()
                };
                new_record(&id, &m.version)
            }
        };
        Some(ConnectorSummary {
            id: record.id.clone(),
            version: record.version.clone(),
            display_name: meta
                .map(|m| m.display_name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| record.id.clone()),
            description: meta.map(|m| m.description.clone()).unwrap_or_default(),
            state: record.state,
            error: record.error,
            provides_mcp: manifest.as_ref().is_some_and(|m| m.provides_mcp()),
            connected: None,
            skills: record.skills.clone(),
        })
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

/// Discover skill directories under a declared `skills` entry.
///
/// Returns `(leaf_name, directory)` pairs. Two shapes are accepted:
/// - the entry itself contains `SKILL.md` → a single skill named after the
///   entry's final path segment;
/// - the entry is a folder of skill folders → every subdirectory containing
///   `SKILL.md` becomes a skill.
///
/// `None` when the path does not exist; empty vec when nothing under it is a
/// valid skill.
fn discover_skill_dirs(base: &Path) -> Option<Vec<(String, PathBuf)>> {
    if !base.exists() {
        return None;
    }
    if base.join("SKILL.md").is_file() {
        let leaf = base
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("skill")
            .to_string();
        return Some(vec![(leaf, base.to_path_buf())]);
    }
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(base) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("SKILL.md").is_file() {
                if let Ok(name) = entry.file_name().into_string() {
                    out.push((name, path));
                }
            }
        }
    }
    Some(out)
}

async fn rollback_skills(storage: &SkillStorage, names: &[String]) {
    for name in names {
        if let Err(e) = storage.uninstall_from_user(name).await {
            warn!("Rollback: failed to remove skill {name}: {e}");
        }
    }
}

async fn copy_dir_recursive(src: &Path, dst: &Path) -> crate::Result<()> {
    async_recursion(src, dst).await
}

/// Boxed body so the recursion compiles inside an async fn.
fn async_recursion<'a>(
    src: &'a Path,
    dst: &'a Path,
) -> std::pin::Pin<Box<dyn Future<Output = crate::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if dst.exists() {
            tokio::fs::remove_dir_all(dst).await?;
        }
        tokio::fs::create_dir_all(dst).await?;
        let mut entries = tokio::fs::read_dir(src).await?;
        while let Some(entry) = entries.next_entry().await? {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                async_recursion(&from, &to).await?;
            } else {
                tokio::fs::copy(&from, &to).await?;
            }
        }
        Ok(())
    })
}

/// Copy an expert package's role definition(s) into the agents directory
/// (`§3.6`). Supports `SOUL.md` at the package root (single agent named after
/// the entry id) or `agents/<id>/SOUL.md` (one agent per subdirectory). The
/// whole role directory is copied so IDENTITY.md / avatars come along.
async fn install_agent_roles(
    pkg: &Path,
    agents_dir: &Path,
    entry_id: &str,
    installed: &mut Vec<String>,
) -> crate::Result<()> {
    if pkg.join("SOUL.md").exists() {
        let target = agents_dir.join(entry_id);
        copy_dir_recursive(pkg, &target).await?;
        installed.push(entry_id.to_string());
        return Ok(());
    }

    let agents_sub = pkg.join("agents");
    if agents_sub.is_dir() {
        let mut rd = tokio::fs::read_dir(&agents_sub).await?;
        while let Some(e) = rd.next_entry().await? {
            let sub = e.path();
            if sub.is_dir() && sub.join("SOUL.md").exists() {
                let id = e.file_name().to_string_lossy().to_string();
                let target = agents_dir.join(&id);
                copy_dir_recursive(&sub, &target).await?;
                installed.push(id);
            }
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::semver;
    use serde_json::json;

    /// URL template for the MCP section; `{url}` is filled in by the fixture
    /// with the address of a local fake streamable-HTTP server.
    const MCP_PACKAGE_JSON: &str = r#"{
        "connector": { "id": "test-mcp", "display_name": "Test MCP",
                       "description": "fixture", "version": "1.0.0" },
        "mcp": { "transport": "streamable_http", "url": "{url}",
                 "auto_connect": false, "timeout_secs": 10 },
        "skills": ["skills"]
    }"#;

    const SKILL_PACKAGE_JSON: &str = r#"{
        "connector": { "id": "cli-tool", "display_name": "CLI Tool",
                       "description": "fixture", "version": "0.3.0" },
        "lifecycle": {
            "init": { "darwin": "echo ok", "linux": "echo ok", "win32": "echo ok" }
        },
        "skills": ["skills"]
    }"#;

    /// Minimal fake streamable-HTTP MCP server: answers initialize /
    /// tools-list / tools-call with SSE-framed JSON-RPC responses.
    async fn spawn_fake_mcp_server() -> String {
        use axum::routing::post;

        async fn handle(body: String) -> String {
            let req: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
            let id = req["id"].clone();
            let result = match req["method"].as_str().unwrap_or("") {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fake", "version": "1.0.0" }
                }),
                "tools/list" => json!({ "tools": [
                    { "name": "echo", "description": "Echo text",
                      "inputSchema": { "type": "object" } }
                ]}),
                _ => json!({ "content": [{ "type": "text", "text": "ok" }],
                             "isError": false }),
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            format!("data: {reply}\n\n")
        }

        let app = axum::Router::new().route("/mcp", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/mcp")
    }

    struct Fixture {
        root: PathBuf,
        user_skills: PathBuf,
        manager: ConnectorManager,
        mcp_url: String,
    }

    async fn fixture() -> Fixture {
        let base =
            std::env::temp_dir().join(format!("syscity_connectors_{}", uuid::Uuid::new_v4()));
        let root = base.join("connectors");
        let user_skills = base.join("user-skills");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&user_skills).unwrap();
        let mcp_url = spawn_fake_mcp_server().await;
        let manager = ConnectorManager::new(
            root.clone(),
            Arc::new(McpManager::default()),
            Arc::new(SkillStorage::with_user_dir(user_skills.clone())),
            #[cfg(feature = "cloud")]
            None,
            Arc::new(crate::secrets::SecretStoreHandle::default()),
        );
        Fixture {
            root,
            user_skills,
            manager,
            mcp_url,
        }
    }

    fn write_package(dir: &Path, json: &str, with_skill: bool) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("connector.json"), json).unwrap();
        if with_skill {
            let skill = dir.join("skills/my-usage");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                "---\nname: my-usage\ndescription: how to drive the cli\n---\n# usage\n",
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
        }
    }

    #[tokio::test]
    async fn install_enable_disable_uninstall_full_cycle() {
        let fx = fixture().await;
        let src = fx.root.parent().unwrap().join("pkg-mcp");
        write_package(&src, &MCP_PACKAGE_JSON.replace("{url}", &fx.mcp_url), true);

        let summary = fx.manager.install_from_dir(&src).await.unwrap();
        assert_eq!(summary.id, "test-mcp");
        assert_eq!(summary.version, "1.0.0");
        assert_eq!(summary.state, StateKind::Installed); // auto_connect=false
        assert!(summary.provides_mcp);

        // Bundled skill landed under the connector-prefixed name.
        assert!(fx.user_skills.join("connector-test-mcp-my-usage").exists());

        let enabled = fx.manager.enable("test-mcp").await.unwrap();
        assert_eq!(enabled.state, StateKind::Enabled);

        // Enabled connectors refuse uninstall until disabled.
        assert!(fx.manager.uninstall("test-mcp").await.is_err());

        let disabled = fx.manager.disable("test-mcp").await.unwrap();
        assert_eq!(disabled.state, StateKind::Disabled);

        fx.manager.uninstall("test-mcp").await.unwrap();
        assert!(!fx.user_skills.join("connector-test-mcp-my-usage").exists());
        assert!(fx.manager.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lifecycle_failure_is_recorded_on_state() {
        let fx = fixture().await;
        let src = fx.root.parent().unwrap().join("pkg-broken");
        write_package(
            &src,
            r#"{
            "connector": { "id": "broken", "display_name": "B", "version": "1.0.0" },
            "lifecycle": { "init": { "darwin": "exit 3", "linux": "exit 3" } },
            "skills": []
        }"#,
            false,
        );
        let err = fx.manager.install_from_dir(&src).await.unwrap_err();
        assert!(err.to_string().contains("init"), "{err}");

        let list = fx.manager.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].state, StateKind::Installed);
        assert!(list[0]
            .error
            .as_deref()
            .unwrap()
            .contains("exited with code"));
    }

    #[tokio::test]
    async fn missing_skill_md_fails_install_without_side_effects() {
        let fx = fixture().await;
        let src = fx.root.parent().unwrap().join("pkg-noskill");
        // Manifest claims a skills dir we deliberately do not create.
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("connector.json"), SKILL_PACKAGE_JSON).unwrap();

        let err = fx.manager.install_from_dir(&src).await.unwrap_err();
        assert!(err.to_string().contains("skill path 'skills' not found"), "{err}");
        // No cache entry, no state record leaked by the failed install.
        assert!(fx.manager.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_joins_cache_and_states_and_survives_missing_manifest() {
        let fx = fixture().await;
        let src = fx.root.parent().unwrap().join("pkg-cli");
        write_package(&src, SKILL_PACKAGE_JSON, true);
        fx.manager.install_from_dir(&src).await.unwrap();

        let list = fx.manager.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].display_name, "CLI Tool");
        assert_eq!(list[0].state, StateKind::Installed);
        assert_eq!(list[0].skills, vec!["connector-cli-tool-my-usage"]);

        // Simulate manual deletion of the package dir: listing must degrade
        // gracefully instead of panicking or dropping the record.
        let pkg = fx.root.join("cache/cli-tool/0.3.0");
        std::fs::remove_dir_all(&pkg).unwrap();
        let degraded = fx.manager.list().await.unwrap();
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].id, "cli-tool");
    }

    #[tokio::test]
    async fn unknown_connector_operations_are_not_found() {
        let fx = fixture().await;
        assert!(matches!(
            fx.manager.enable("ghost").await.unwrap_err(),
            SyscityError::NotFound { .. }
        ));
        assert!(matches!(
            fx.manager.auth_status("ghost").await.unwrap_err(),
            SyscityError::NotFound { .. }
        ));
    }

    /// A skill-type catalog entry (bare SKILL.md package, no connector.json)
    /// upgrades into the user skills dir and records an installed state.
    #[tokio::test]
    async fn upgrade_installs_skill_package() {
        let fx = fixture().await;
        // Pre-place the package where `install_entry`'s fast path finds it:
        // cache/<id>/<version>/SKILL.md (no download happens in this test).
        let dest = fx.root.join("cache/brand-skill/1.0.0");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(
            dest.join("SKILL.md"),
            "---\nname: brand\ndescription: brand guidelines\n---\n# brand\n",
        )
        .unwrap();

        let entry = catalog::CatalogEntry {
            id: "brand-skill".to_string(),
            version: "1.0.0".to_string(),
            display_name: "Brand Skill".to_string(),
            description: "fixture".to_string(),
            icon: None,
            entry_type: "skill".to_string(),
            kind: "byoa".to_string(),
            visibility: "public".to_string(),
            credits_per_use: 0,
            category: None,
            starter_prompt: None,
            source: catalog::CatalogSource {
                kind: "tar.gz".to_string(),
                url: "https://example.com/brand-skill.tgz".to_string(),
            },
            sha256: None,
            auto_update: false,
        };

        let summary = fx.manager.upgrade(&entry).await.unwrap();
        assert_eq!(summary.id, "brand-skill");
        assert_eq!(summary.state, StateKind::Installed);
        assert!(!summary.provides_mcp);
        assert_eq!(summary.skills, vec!["skill-brand-skill".to_string()]);

        // The skill landed in the user skills dir, and the package cache copy
        // survived the in-place install (no copy-onto-self wipe).
        assert!(fx.user_skills.join("skill-brand-skill/SKILL.md").exists());
        assert!(dest.join("SKILL.md").exists());

        // The state record makes catalog "installed" detection work.
        let listed = fx.manager.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "brand-skill");
        assert_eq!(listed[0].state, StateKind::Installed);
    }

    /// A connector-type upgrade whose package already lives in the cache dir
    /// must not wipe it (regression: copy-onto-self emptied the package).
    #[tokio::test]
    async fn upgrade_connector_keeps_in_place_package() {
        let fx = fixture().await;
        let dest = fx.root.join("cache/test-mcp/1.0.0");
        std::fs::create_dir_all(dest.join("skills/my-usage")).unwrap();
        std::fs::write(dest.join("connector.json"), MCP_PACKAGE_JSON.replace("{url}", &fx.mcp_url))
            .unwrap();
        std::fs::write(
            dest.join("skills/my-usage/SKILL.md"),
            "---\nname: my-usage\ndescription: usage\n---\n# usage\n",
        )
        .unwrap();

        let entry = catalog::CatalogEntry {
            id: "test-mcp".to_string(),
            version: "1.0.0".to_string(),
            display_name: "Test MCP".to_string(),
            description: "fixture".to_string(),
            icon: None,
            entry_type: "connector".to_string(),
            kind: "byoa".to_string(),
            visibility: "public".to_string(),
            credits_per_use: 0,
            category: None,
            starter_prompt: None,
            source: catalog::CatalogSource {
                kind: "tar.gz".to_string(),
                url: "https://example.com/test-mcp.tgz".to_string(),
            },
            sha256: None,
            auto_update: false,
        };

        let summary = fx.manager.upgrade(&entry).await.unwrap();
        assert_eq!(summary.id, "test-mcp");
        assert_eq!(summary.state, StateKind::Installed);
        assert!(summary.provides_mcp);
        // Package contents survived; bundled skill installed.
        assert!(dest.join("connector.json").exists());
        assert!(fx
            .user_skills
            .join("connector-test-mcp-my-usage/SKILL.md")
            .exists());
    }

    #[tokio::test]
    async fn check_updates_empty_without_catalog() {
        let fx = fixture().await;
        assert!(fx.manager.check_updates().await.unwrap().is_empty());
    }

    #[test]
    fn semver_reexport_usable() {
        // Guards the semver dependency used by lifecycle version checks.
        let v = semver::Version::parse("1.2.3").unwrap();
        assert!(semver::VersionReq::parse(">=1.0.0").unwrap().matches(&v));
    }

    /// An expert package with `SOUL.md` at the root installs a single agent
    /// named after the entry id.
    #[tokio::test]
    async fn expert_install_copies_root_soul_to_agents_dir() {
        let base = std::env::temp_dir().join(format!("syscity_expert_{}", uuid::Uuid::new_v4()));
        let pkg = base.join("pkg");
        let agents = base.join("agents");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("SOUL.md"), "---\nname: equity-research\n---\n# Expert\n").unwrap();
        std::fs::write(pkg.join("IDENTITY.md"), "# Identity\n").unwrap();

        let mut installed = Vec::new();
        install_agent_roles(&pkg, &agents, "equity-research", &mut installed)
            .await
            .unwrap();
        assert_eq!(installed, vec!["equity-research".to_string()]);
        assert!(agents.join("equity-research").join("SOUL.md").exists());
        assert!(agents.join("equity-research").join("IDENTITY.md").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An expert package with an `agents/<id>/SOUL.md` layout installs one
    /// agent per role subdirectory.
    #[tokio::test]
    async fn expert_install_supports_agents_subdir_layout() {
        let base = std::env::temp_dir().join(format!("syscity_expert_{}", uuid::Uuid::new_v4()));
        let pkg = base.join("pkg");
        let agents = base.join("agents");
        let role = pkg.join("agents").join("analyst");
        std::fs::create_dir_all(&role).unwrap();
        std::fs::write(role.join("SOUL.md"), "---\nname: analyst\n---\n").unwrap();

        let mut installed = Vec::new();
        install_agent_roles(&pkg, &agents, "expert-pack", &mut installed)
            .await
            .unwrap();
        assert_eq!(installed, vec!["analyst".to_string()]);
        assert!(agents.join("analyst").join("SOUL.md").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A `kind=cloud` manifest with no local mcp/lifecycle/skills is valid and
    /// parsed as a cloud connector regardless of the build feature.
    #[test]
    fn cloud_kind_manifest_parses() {
        let m = ConnectorManifest::parse(
            r#"{
            "connector": { "id": "market-data", "display_name": "Market data", "version": "1.0.0" },
            "kind": "cloud"
        }"#,
        )
        .unwrap();
        assert_eq!(m.kind, "cloud");
        assert!(m.mcp.is_none());
    }

    /// Without the cloud feature, a cloud connector has no local server and no
    /// relay → `server_config_for` yields no MCP config.
    #[cfg(not(feature = "cloud"))]
    #[tokio::test]
    async fn cloud_connector_has_no_config_without_cloud_feature() {
        let fx = fixture().await;
        let m = ConnectorManifest::parse(
            r#"{
            "connector": { "id": "market-data", "version": "1.0.0" },
            "kind": "cloud"
        }"#,
        )
        .unwrap();
        assert!(fx.manager.server_config_for("market-data", &m).is_none());
    }

    /// With the cloud feature + `cloud_api_base`, a `kind=cloud` connector
    /// routes through the cloud MCP relay.
    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn cloud_connector_routes_to_cloud_relay_when_cloud_mode_on() {
        let base =
            std::env::temp_dir().join(format!("syscity_connectors_{}", uuid::Uuid::new_v4()));
        let root = base.join("connectors");
        let user_skills = base.join("user-skills");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&user_skills).unwrap();
        let manager = ConnectorManager::new(
            root.clone(),
            Arc::new(McpManager::default()),
            Arc::new(SkillStorage::with_user_dir(user_skills)),
            Some("https://api.syscity.net".to_string()),
            Arc::new(crate::secrets::SecretStoreHandle::default()),
        );

        let m = ConnectorManifest::parse(
            r#"{
            "connector": { "id": "market-data", "version": "1.0.0" },
            "kind": "cloud"
        }"#,
        )
        .unwrap();
        let cfg = manager
            .server_config_for("market-data", &m)
            .expect("cloud relay config expected");
        match cfg.transport {
            crate::mcp::McpTransport::Cloud { connector_id, api_base } => {
                assert_eq!(connector_id, "market-data");
                assert_eq!(api_base, "https://api.syscity.net");
            }
            other => panic!("expected cloud relay transport, got {other:?}"),
        }
    }

    /// A `kind=cloud` connector cannot be enabled without cloud mode — fail
    /// loudly instead of enabling an inert connector.
    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn cloud_connector_enable_fails_without_cloud_mode() {
        let fx = fixture().await; // cloud_api_base = None (cloud mode off)
        let src = fx.root.parent().unwrap().join("pkg-cloud");
        write_package(
            &src,
            r#"{
            "connector": { "id": "market-data", "display_name": "Market data", "version": "1.0.0" },
            "kind": "cloud"
        }"#,
            false,
        );
        let summary = fx.manager.install_from_dir(&src).await.unwrap();
        assert_eq!(summary.state, StateKind::Installed);

        let err = fx.manager.enable("market-data").await.unwrap_err();
        assert!(err.to_string().contains("cloud-provisioned"), "{err}");

        // The failure is recorded on the state.
        let list = fx.manager.list().await.unwrap();
        assert_eq!(list[0].state, StateKind::Error);
        assert!(list[0]
            .error
            .as_deref()
            .unwrap()
            .contains("cloud-provisioned"));
    }
}
