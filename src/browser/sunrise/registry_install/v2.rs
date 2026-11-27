//! Schema-2 signed registry planning, verification, staging, installation, updates, and explicit
//! provider switching.
//!
//! Every v2 operation keys mods by permanent 8-character project ID, treats `project.mod_id` as an
//! in-game slot with at most one enabled provider per profile, relies solely on authoritative
//! `release.fits` / `release.does_not_fit` verdicts against the exact Sunrise build ID, verifies
//! every archive's SHA-256 and offline Sigstore attestation against `project.repository.id` and
//! `release.workflow`, pins the publisher repository and workflow per computer, and refuses to
//! silently pick among competing providers or silently substitute a different provider.

use super::{
    at, guard_replacements, held_change_refusal, known_unfit_updates, manifest_conflict,
    missing_required, mutations, prove_edit, remember_unfit_sha256, ChangeImpact, Document,
    InstallError, InstallHooks, InstallProgress, InstalledBatch, UpdateRequest,
};
use crate::{
    attestation::{self, ExpectedRelease, PublisherChange, PublisherPin, VerifiedRelease},
    backend,
    browser::{
        download, installed, mod_list,
        sunrise::{chosen, v2 as catalogue_v2, SITE},
        Kind, SourceId,
    },
    i18n::{tr, trf},
    instances::{Instance, Kind as InstanceKind},
    managed_play::{self, FirstBuild, Profile, ScannedPackage},
    native_mods,
    profile_txn::Identity,
};
use semver::VersionReq;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
};
use sunrise_launcher_smod::{
    dependencies::parse_version,
    index::{self, MAX_FILE},
    index_v2::{self, FitVerdict, Index, Kind as V2Kind, Project, Release, Revoked},
    manifest::Side,
};

mod switch_v2;
// Wherever `update_tests` builds (`registry_install.rs`), its `build_switch` uses it, not only
// the native tests: on macOS too, and on no platform where it would go unused.
#[cfg(all(test, debug_assertions, unix))]
pub(crate) use switch_v2::switch_profile_build_v2_with;
pub use switch_v2::{
    checked_switch_fit_v2, mods_fit_by_build_v2, switch_build_v2, switch_fit_blockers_v2,
    switch_profile_build_v2, Blocker, FitIssue, V2SwitchHooks,
};

/// Maximum accepted `.sigstore.json` attestation bundle size (1 MiB).
const MAX_ATTESTATION_BYTES: u64 = 1 << 20;

/// Injectable byte fetcher used by internal `_with` entrypoints and unit tests.
pub(crate) type BytesFetcher<'a> = &'a dyn Fn(&str, u64) -> Result<Vec<u8>, String>;

/// Injectable offline attestation verifier (production always passes `attestation::verify_release`).
pub(crate) type ReleaseVerifier<'a> = &'a dyn Fn(
    &[u8],
    [u8; 32],
    &ExpectedRelease<'_>,
) -> Result<VerifiedRelease, attestation::Error>;

/// One installable v2 project offering a fitting release for an ambiguous dependency slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCandidate {
    pub project_id: String,
    pub slug: String,
    pub name: String,
    pub repository_name: String,
    pub version: String,
}

/// Asks the player which project provides a dependency several projects offer; `None` declines.
pub type ChooseProvider<'a> = dyn Fn(&ProviderChoiceNeeded) -> Option<String> + 'a;

/// Returned when a required dependency names only an in-game `mod_id` slot and several v2 projects
/// provide fitting releases for the profile's build. The planner never chooses one silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderChoiceNeeded {
    pub mod_id: String,
    pub required_by: String,
    pub candidates: Vec<ProviderCandidate>,
}

/// Returned when a requested project or required dependency would collide with a different provider
/// (another v2 project or an unadopted/local mod) already occupying the same in-game `mod_id` slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderConflict {
    pub mod_id: String,
    pub installed_project: Option<String>,
    pub installed_name: String,
    pub requested_project: String,
    pub requested_name: String,
}

/// Verified schema-2 ownership of an installed package on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum V2Ownership {
    /// Verified v2 receipt whose on-disk size and SHA-256 match a release in this project's signed
    /// v2 history.
    Verified {
        project_id: String,
        slug: String,
        version: String,
        repository: installed::VerifiedRepository,
    },
    /// Legacy v1 receipt adopted ONLY because exact on-disk size, version, kind, and SHA-256
    /// uniquely matched one `(Project, Release)` in signed v2 history.
    AdoptedV1 {
        project_id: String,
        slug: String,
        version: String,
        repository: installed::VerifiedRepository,
    },
    /// Manual file, GitHub source mod, unadopted v1 file, or tampered bytes: not owned by any v2
    /// project. Whole-project takedowns never apply, and updates/switches never replace it.
    Local,
}

impl V2Ownership {
    pub fn project_id(&self) -> Option<&str> {
        match self {
            Self::Verified { project_id, .. } | Self::AdoptedV1 { project_id, .. } => {
                Some(project_id.as_str())
            }
            Self::Local => None,
        }
    }

    pub fn repository(&self) -> Option<&installed::VerifiedRepository> {
        match self {
            Self::Verified { repository, .. } | Self::AdoptedV1 { repository, .. } => {
                Some(repository)
            }
            Self::Local => None,
        }
    }

    pub fn is_owned(&self) -> bool {
        !matches!(self, Self::Local)
    }
}

/// One scanned package in a profile together with its verified v2 ownership.
#[derive(Clone, Debug)]
pub struct InstalledV2Mod {
    pub package: ScannedPackage,
    pub digest: String,
    pub size: u64,
    pub enabled: bool,
    pub ownership: V2Ownership,
}

/// Options controlling v2 planning, version overrides, explicit provider choices, and publisher pin
/// confirmation.
#[derive(Clone, Debug, Default)]
pub struct V2InstallOptions {
    /// Explicit provider selections for ambiguous dependency slots: `mod_id -> project_id`.
    pub provider_choices: BTreeMap<String, String>,
    /// Explicit version overrides per project ID (takes precedence over `chosen-versions.json`).
    pub versions: BTreeMap<String, String>,
    /// Project IDs for which the player has already confirmed a verified publisher change.
    pub accepted_publisher_changes: BTreeSet<String>,
    /// Override directory for `publisher-pins.json` (`None` uses `backend::data_dir()`).
    pub pin_dir: Option<PathBuf>,
}

/// One concrete release selected by the v2 planner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedV2Release {
    pub project_id: String,
    pub slug: String,
    pub mod_id: String,
    pub name: String,
    pub kind: V2Kind,
    pub version: String,
    pub sha256: String,
    pub size: u64,
    pub url: String,
    pub attestation_url: String,
    pub workflow: String,
    pub repository_id: u64,
    pub repository_name: String,
    pub repository_path: String,
    pub dependency: bool,
}

/// Result of planning a v2 install or update batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V2Plan {
    pub steps: Vec<PlannedV2Release>,
    /// Optional dependencies declared by the planned releases (never auto-installed).
    pub optional_dependencies: Vec<index_v2::Dependency>,
}

/// An enabled dependent in the profile that would be affected by switching a slot's provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V2AffectedDependent {
    pub mod_id: String,
    pub name: String,
    pub reason: String,
}

/// Preview presented to the player before committing an explicit provider switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSwitchPreview {
    pub mod_id: String,
    pub from_project: Option<String>,
    pub from_name: String,
    pub to_project: String,
    pub to_name: String,
    pub to_version: String,
    pub worlds: Vec<crate::worlds::WorldRisk>,
}

/// What the player is asked when a release's verified publisher differs from this computer's
/// remembered pin for its project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherChangeQuestion {
    pub mod_name: String,
    /// The version installed in the profile now; `None` when this would be its first install there.
    pub current_version: Option<String>,
    pub incoming_version: String,
    pub change: PublisherChange,
}

/// Asked before a release from a changed publisher is staged; `true` accepts the new publisher.
pub type ConfirmPublisherChange<'a> = &'a dyn Fn(&PublisherChangeQuestion) -> bool;

/// Callbacks for v2 install and update operations.
#[derive(Clone, Copy)]
pub struct V2Hooks<'a> {
    pub install: InstallHooks<'a>,
    /// Prompt when a required dependency slot has several fitting providers and no pre-selected
    /// entry in `V2InstallOptions::provider_choices`. Returning `None` aborts with
    /// [`InstallError::AmbiguousProvider`].
    pub choose_provider: Option<&'a ChooseProvider<'a>>,
    /// Prompt when a project's attested repository ID or workflow path changed from this
    /// computer's remembered pin. Returning `false` (or `None`) aborts with
    /// [`InstallError::PublisherChanged`].
    pub confirm_publisher_change: Option<ConfirmPublisherChange<'a>>,
}

/// Callbacks for explicit v2 provider switching.
#[derive(Clone, Copy)]
pub struct V2ProviderSwitchHooks<'a> {
    pub install: V2Hooks<'a>,
    pub approve_switch: &'a dyn Fn(&ProviderSwitchPreview) -> bool,
}

#[derive(Debug)]
pub(crate) struct PendingPin {
    project_id: String,
    pin: PublisherPin,
    accepted_change: Option<PublisherChange>,
}

/// Read the player's pinned version for a project (keyed by permanent project ID, falling back to
/// legacy v1 in-game mod ID from `mod-browser/chosen-versions.json`).
pub fn chosen_version_pin<'a>(
    pins: &'a BTreeMap<String, String>,
    project_id: &str,
    mod_id: &str,
) -> Option<&'a str> {
    pins.get(project_id)
        .or_else(|| pins.get(mod_id))
        .map(String::as_str)
}

/// Classify an installed package's v2 ownership strictly from verified evidence.
///
/// A legacy v1 `mod-browser.json` row is adopted ONLY when its on-disk kind, version, byte size,
/// and SHA-256 uniquely match one `(Project, Release)` in signed v2 history. Any manual file,
/// GitHub source mod, unmatched v1 row, tampered file, or ambiguous digest stays
/// [`V2Ownership::Local`].
pub fn classify_v2_ownership(
    index: &Index,
    home: &Path,
    entry: Option<&installed::Entry>,
    package: &ScannedPackage,
    digest: &str,
    size: u64,
    has_github_origin: bool,
) -> V2Ownership {
    if package.directory || has_github_origin {
        return V2Ownership::Local;
    }
    let Some(row) = entry else {
        return V2Ownership::Local;
    };
    if row.source != SourceId::Sunrise
        || row.kind != Kind::Mod
        || row.path(home).as_deref() != Some(package.path.as_path())
    {
        return V2Ownership::Local;
    }
    let expected_kind = if package.manifest.native.is_some() {
        V2Kind::NativeMod
    } else {
        V2Kind::Mod
    };
    if row.is_v2_registry() && row.mod_id.as_deref() == Some(package.manifest.id.as_str()) {
        let Some(project) = index.project(&row.project) else {
            return V2Ownership::Local;
        };
        if project.mod_id != package.manifest.id || project.kind != expected_kind {
            return V2Ownership::Local;
        }
        let Some(release) = project.releases.iter().find(|release| {
            release.file.sha256.eq_ignore_ascii_case(digest) && release.file.size == size
        }) else {
            return V2Ownership::Local;
        };
        let Some(repository) = row.repository.as_ref() else {
            return V2Ownership::Local;
        };
        if repository.id != project.repository.id {
            return V2Ownership::Local;
        }
        return V2Ownership::Verified {
            project_id: project.id.clone(),
            slug: project.slug.clone(),
            version: release.version.clone(),
            repository: repository.clone(),
        };
    }
    // Legacy v1 receipt: project field holds the in-game mod id, mod_id/repository are absent.
    if row.mod_id.is_none() && row.project == package.manifest.id {
        let matches: Vec<(&Project, &Release)> = index
            .releases_by_sha256(&package.manifest.id, digest)
            .into_iter()
            .filter(|(project, release)| {
                project.kind == expected_kind
                    && release.version == package.manifest.version
                    && release.file.size == size
            })
            .collect();
        if let [(project, release)] = matches.as_slice() {
            return V2Ownership::AdoptedV1 {
                project_id: project.id.clone(),
                slug: project.slug.clone(),
                version: release.version.clone(),
                repository: installed::VerifiedRepository {
                    id: project.repository.id,
                    name: project.repository.name.clone(),
                    path: project.repository.path.clone(),
                    workflow: release.workflow.clone(),
                },
            };
        }
    }
    V2Ownership::Local
}

/// Returns the active takedown affecting an installed package, if any.
///
/// Exact-digest takedowns (`revoked[].sha256 = Some(...)`) apply even to local/unadopted files.
/// Whole-project takedowns (`revoked[].sha256 = None`) apply ONLY when ownership of that project
/// is proved ([`V2Ownership::Verified`] or [`V2Ownership::AdoptedV1`]).
pub fn revoked_for_package<'a>(
    index: &'a Index,
    ownership: &V2Ownership,
    digest: &str,
) -> Option<&'a Revoked> {
    if let Some(revoked) = index.revoked_by_sha256(digest) {
        return Some(revoked);
    }
    ownership
        .project_id()
        .and_then(|project_id| index.revoked(project_id, Some(digest)))
}

/// True when `package` has a non-registry `.origin.json` sidecar (GitHub or local-folder install),
/// or when reading sidecar metadata fails (fail closed so source mods are never adopted as v2).
fn has_source_origin(home: &Path, package: &ScannedPackage) -> bool {
    if package.directory {
        return false;
    }
    let present = |path: &Path| {
        !matches!(
            fs::symlink_metadata(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    };
    let beside = package.path.with_extension("origin.json");
    let native = crate::github_mods::origin_path(home, &package.manifest.id);
    present(&beside) || present(&native)
}

/// Convenience helper for pre-Play checks: classifies `package` against `rows` and checks v2
/// revocations for `digest`.
pub(crate) fn revoked_for_installed_package<'a>(
    index: &'a Index,
    rows: &installed::Installed,
    home: &Path,
    package: &ScannedPackage,
    digest: &str,
) -> Option<&'a Revoked> {
    let size = fs::metadata(&package.path).ok()?.len();
    let entry = rows
        .entries
        .iter()
        .find(|row| row.in_game_mod_id() == Some(package.manifest.id.as_str()));
    let has_github = has_source_origin(home, package);
    let ownership = classify_v2_ownership(index, home, entry, package, digest, size, has_github);
    revoked_for_package(index, &ownership, digest)
}

/// Turn off any enabled mods in `home` revoked by the signed v2 index, leaving their files on disk.
pub fn turn_off_revoked_v2(index: &Index, home: &Path) -> Result<Vec<String>, String> {
    let _lock = crate::browser::lock_home(home).map_err(|e| e.to_string())?;
    installed::recover_game_install(home)?;
    let packages = managed_play::scan_packages(home)?;
    let rows = installed::Installed::read(home)?;
    let mut notices = Vec::new();
    for package in &packages {
        if package.directory {
            continue;
        }
        let digest = index::sha256_file(&package.path)?;
        let Some(revoked) = revoked_for_installed_package(index, &rows, home, package, &digest)
        else {
            continue;
        };
        if mod_list::is_enabled(home, &package.manifest.id) {
            mod_list::set_enabled(home, &package.manifest.id, false)?;
            notices.push(trf(
                "{mod} was turned off because it was taken down: {reason}",
                &[("mod", &package.manifest.name), ("reason", &revoked.reason)],
            ));
        }
    }
    for entry in &rows.entries {
        if entry.source != SourceId::Sunrise
            || !matches!(entry.kind, Kind::ResourcePack | Kind::Shader)
        {
            continue;
        }
        let revoked = if index_v2::valid_project_id(&entry.project) {
            index.revoked(&entry.project, entry.sha256())
        } else {
            entry
                .sha256()
                .and_then(|hash| index.revoked_by_sha256(hash))
        };
        if let Some(revoked) = revoked {
            notices.push(trf(
                "{mod} was taken down: {reason} Remove it.",
                &[("mod", &entry.title), ("reason", &revoked.reason)],
            ));
        }
    }
    Ok(notices)
}

/// Scan every installed package in `home` and classify its verified v2 ownership against `index`.
pub fn inventory_v2(
    home: &Path,
    index: &Index,
) -> Result<BTreeMap<String, InstalledV2Mod>, String> {
    let packages = managed_play::scan_packages(home)?;
    let rows = installed::Installed::read(home)?;
    let mut by_slot: BTreeMap<String, &installed::Entry> = BTreeMap::new();
    for row in &rows.entries {
        if let Some(slot) = row.in_game_mod_id() {
            if by_slot.insert(slot.to_owned(), row).is_some() {
                return Err(trf(
                    "Several entries in {manifest} name {id}.",
                    &[("manifest", installed::MANIFEST), ("id", slot)],
                ));
            }
        }
    }
    let mut out = BTreeMap::new();
    for package in packages {
        let id = package.manifest.id.clone();
        let enabled = mod_list::is_enabled(home, &id);
        let (digest, size) = if package.directory {
            (String::new(), 0)
        } else {
            (
                index::sha256_file(&package.path)?,
                fs::metadata(&package.path)
                    .map_err(|e| at(&package.path, e))?
                    .len(),
            )
        };
        let has_github = has_source_origin(home, &package);
        let ownership = classify_v2_ownership(
            index,
            home,
            by_slot.get(&id).copied(),
            &package,
            &digest,
            size,
            has_github,
        );
        out.insert(
            id,
            InstalledV2Mod {
                package,
                digest,
                size,
                enabled,
                ownership,
            },
        );
    }
    Ok(out)
}

/// Deterministic version comparison (semver when parseable, falling back to string order).
pub(crate) fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_version(a), parse_version(b)) {
        (Ok(va), Ok(vb)) => va.cmp(&vb).then_with(|| a.cmp(b)),
        _ => a.cmp(b),
    }
}

/// A project the planner still has to place: its ID, whether it is only a dependency, the mod
/// that asked for it, and the version range that mod requires of it.
type Queued = (String, bool, String, Option<VersionReq>);

/// Whether `version` is inside `range`; a version that isn't semver is in no range.
fn in_range(range: Option<&VersionReq>, version: &str) -> bool {
    range.is_none_or(|range| parse_version(version).is_ok_and(|v| range.matches(&v)))
}

fn select_project_release<'a>(
    index: &Index,
    project: &'a Project,
    build: &str,
    pinned_version: Option<&str>,
    range: Option<&VersionReq>,
    unfit_hashes: &BTreeSet<String>,
) -> Result<&'a Release, InstallError> {
    if let Some(revoked) = index.revoked(&project.id, None) {
        return Err(InstallError::Refused(trf(
            "{mod} was taken down: {reason}",
            &[("mod", &project.name), ("reason", &revoked.reason)],
        )));
    }
    if let Some(pinned) = pinned_version {
        let Some(release) = project.releases.iter().find(|r| r.version == pinned) else {
            return Err(InstallError::NotForVersion(trf(
                "Pinned version {version} of {mod} is not in the Sunrise registry.",
                &[("version", pinned), ("mod", &project.name)],
            )));
        };
        if let Some(range) = range.filter(|_| !in_range(range, pinned)) {
            return Err(InstallError::NotForVersion(trf(
                "Pinned version {version} of {mod} is outside {range}, which another mod needs.",
                &[
                    ("version", pinned),
                    ("mod", &project.name),
                    ("range", &range.to_string()),
                ],
            )));
        }
        if let Some(revoked) = index.revoked(&project.id, Some(&release.file.sha256)) {
            return Err(InstallError::Refused(trf(
                "{mod} {version} was taken down: {reason}",
                &[
                    ("mod", &project.name),
                    ("version", pinned),
                    ("reason", &revoked.reason),
                ],
            )));
        }
        if unfit_hashes.contains(&release.file.sha256.to_ascii_lowercase()) {
            return Err(InstallError::NotForVersion(trf(
                "{mod} {version} does not fit Sunrise {build}.",
                &[
                    ("mod", &project.name),
                    ("version", pinned),
                    ("build", build),
                ],
            )));
        }
        return match release.fit_verdict(build) {
            FitVerdict::Fits => Ok(release),
            FitVerdict::DoesNotFit => Err(InstallError::NotForVersion(trf(
                "{mod} {version} does not fit Sunrise {build}.",
                &[
                    ("mod", &project.name),
                    ("version", pinned),
                    ("build", build),
                ],
            ))),
            FitVerdict::Unknown => Err(InstallError::NotForVersion(trf(
                "Fit for {mod} {version} on Sunrise {build} is not known yet.",
                &[
                    ("mod", &project.name),
                    ("version", pinned),
                    ("build", build),
                ],
            ))),
        };
    }

    let mut fitting: Vec<&Release> = Vec::new();
    let mut saw_unknown = false;
    let mut saw_does_not_fit = false;
    let mut saw_revoked = false;
    let mut saw_out_of_range = false;
    for release in &project.releases {
        if index
            .revoked(&project.id, Some(&release.file.sha256))
            .is_some()
        {
            saw_revoked = true;
            continue;
        }
        if !in_range(range, &release.version) {
            saw_out_of_range = true;
            continue;
        }
        if unfit_hashes.contains(&release.file.sha256.to_ascii_lowercase()) {
            saw_does_not_fit = true;
            continue;
        }
        match release.fit_verdict(build) {
            FitVerdict::Fits => fitting.push(release),
            FitVerdict::DoesNotFit => saw_does_not_fit = true,
            FitVerdict::Unknown => saw_unknown = true,
        }
    }
    if let Some(best) = fitting
        .into_iter()
        .max_by(|a, b| compare_versions(&a.version, &b.version))
    {
        return Ok(best);
    }
    if let Some(range) = range.filter(|_| saw_out_of_range) {
        return Err(InstallError::NotForVersion(trf(
            "No release of {mod} in {range} fits Sunrise {build}.",
            &[
                ("mod", &project.name),
                ("range", &range.to_string()),
                ("build", build),
            ],
        )));
    }
    if saw_unknown {
        Err(InstallError::NotForVersion(trf(
            "Fit for {mod} on Sunrise {build} is not known yet.",
            &[("mod", &project.name), ("build", build)],
        )))
    } else if !saw_does_not_fit && saw_revoked {
        Err(InstallError::Refused(trf(
            "All releases of {mod} were taken down.",
            &[("mod", &project.name)],
        )))
    } else {
        Err(InstallError::NotForVersion(trf(
            "{mod} does not fit Sunrise {build}.",
            &[("mod", &project.name), ("build", build)],
        )))
    }
}

fn planned_from(project: &Project, release: &Release, dependency: bool) -> PlannedV2Release {
    PlannedV2Release {
        project_id: project.id.clone(),
        slug: project.slug.clone(),
        mod_id: project.mod_id.clone(),
        name: project.name.clone(),
        kind: project.kind.clone(),
        version: release.version.clone(),
        sha256: release.file.sha256.clone(),
        size: release.file.size,
        url: release.file.url.clone(),
        attestation_url: release.file.attestation.clone(),
        workflow: release.workflow.clone(),
        repository_id: project.repository.id,
        repository_name: project.repository.name.clone(),
        repository_path: project.repository.path.clone(),
        dependency,
    }
}

/// Plan a schema-2 install of `requested_projects` (permanent 8-character project IDs) for exact
/// Sunrise build `build`.
///
/// - Uses only authoritative `release.fits` / `release.does_not_fit` verdicts (`FitVerdict::Fits`).
/// - Honors explicit version overrides and `chosen-versions.json` pins.
/// - Auto-installs ONLY required dependencies (`dep.required == true`); optional dependencies are
///   returned for display without being added to `steps`.
/// - When a required dependency has several fitting providers and no provider is already installed
///   or supplied, asks `choose_provider` or returns [`InstallError::AmbiguousProvider`] — never
///   silently picking one.
/// - Refuses with [`InstallError::ProviderConflict`] rather than silently substituting a different
///   provider in an occupied slot.
pub fn plan_v2_install(
    index: &Index,
    build: &str,
    requested_projects: &[String],
    installed: &BTreeMap<String, InstalledV2Mod>,
    chosen_pins: &BTreeMap<String, String>,
    options: &V2InstallOptions,
    choose_provider: Option<&ChooseProvider<'_>>,
) -> Result<V2Plan, InstallError> {
    if requested_projects.is_empty() {
        return Err(InstallError::Cancelled);
    }
    let unfit_hashes = known_unfit_updates(build);
    let mut seen_slots: BTreeMap<String, String> = BTreeMap::new();
    let mut queue: VecDeque<Queued> = VecDeque::new();
    for project_id in requested_projects {
        let project = index.project(project_id).ok_or_else(|| {
            InstallError::Refused(trf(
                "Project {project} is not in the Sunrise registry.",
                &[("project", project_id)],
            ))
        })?;
        if !matches!(project.kind, V2Kind::Mod | V2Kind::NativeMod) {
            return Err(InstallError::Refused(trf(
                "{mod} is not a mod package.",
                &[("mod", &project.name)],
            )));
        }
        if let Some(other_id) = seen_slots.insert(project.mod_id.clone(), project.id.clone()) {
            if other_id != project.id {
                let other_name = index
                    .project(&other_id)
                    .map_or(other_id.as_str(), |p| p.name.as_str());
                return Err(InstallError::ProviderConflict(Box::new(ProviderConflict {
                    mod_id: project.mod_id.clone(),
                    installed_project: Some(other_id.clone()),
                    installed_name: other_name.to_owned(),
                    requested_project: project.id.clone(),
                    requested_name: project.name.clone(),
                })));
            }
        }
        queue.push_back((project.id.clone(), false, project.name.clone(), None));
    }
    resolve_v2_queue(
        &ResolveContext {
            index,
            build,
            installed,
            chosen_pins,
            options,
            unfit_hashes: &unfit_hashes,
            choose_provider,
        },
        queue,
    )
}

/// Plan a schema-2 update for `request` (Strict or Bulk). Accepts either permanent project IDs or
/// in-game mod IDs, ALWAYS keeps each installed mod's chosen project, honors `chosen-versions.json`
/// pins on bulk updates, and resolves any new required dependencies using the same non-silent rules.
pub fn plan_v2_update(
    index: &Index,
    build: &str,
    request: &UpdateRequest,
    installed: &BTreeMap<String, InstalledV2Mod>,
    chosen_pins: &BTreeMap<String, String>,
    options: &V2InstallOptions,
    choose_provider: Option<&ChooseProvider<'_>>,
) -> Result<V2Plan, InstallError> {
    let (requested, strict) = match request {
        UpdateRequest::Strict(ids) => (ids.as_slice(), true),
        UpdateRequest::Bulk(ids) => (ids.as_slice(), false),
    };
    let unfit_hashes = known_unfit_updates(build);
    let mut queue: VecDeque<Queued> = VecDeque::new();
    let mut effective_options = options.clone();

    for key in requested {
        let found = installed.get(key).or_else(|| {
            installed
                .values()
                .find(|m| m.ownership.project_id() == Some(key.as_str()))
        });
        let Some(current) = found else {
            if strict {
                return Err(InstallError::Refused(trf(
                    "{mod} is not installed.",
                    &[("mod", key)],
                )));
            }
            continue;
        };
        let Some(project_id) = current.ownership.project_id() else {
            if strict {
                return Err(InstallError::Refused(trf(
                    "Sunrise couldn't confirm that {mod} came from the Sunrise registry, so it can't be updated here.",
                    &[("mod", &current.package.manifest.name)],
                )));
            }
            continue;
        };
        let Some(project) = index.project(project_id) else {
            if strict {
                return Err(InstallError::Refused(trf(
                    "{mod} is no longer listed in the Sunrise registry.",
                    &[("mod", &current.package.manifest.name)],
                )));
            }
            continue;
        };
        if let Some(revoked) = index.revoked(&project.id, None) {
            if strict {
                return Err(InstallError::Refused(trf(
                    "{mod} was taken down: {reason}",
                    &[("mod", &project.name), ("reason", &revoked.reason)],
                )));
            }
            continue;
        }
        // Respect chosen-versions.json pins during Update all (bulk).
        let pin = chosen_version_pin(chosen_pins, &project.id, &project.mod_id);
        if !strict && pin.is_some() && !effective_options.versions.contains_key(&project.id) {
            continue;
        }
        let target_version = effective_options
            .versions
            .get(&project.id)
            .map(String::as_str);
        if let Some(explicit) = target_version {
            let release =
                select_project_release(index, project, build, Some(explicit), None, &unfit_hashes)?;
            if release.version != current.package.manifest.version
                || !release.file.sha256.eq_ignore_ascii_case(&current.digest)
            {
                queue.push_back((project.id.clone(), false, project.name.clone(), None));
            }
            continue;
        }
        let current_ver = &current.package.manifest.version;
        let newer = project
            .releases
            .iter()
            .filter(|release| {
                release.fit_verdict(build) == FitVerdict::Fits
                    && index
                        .revoked(&project.id, Some(&release.file.sha256))
                        .is_none()
                    && !unfit_hashes.contains(&release.file.sha256.to_ascii_lowercase())
                    && compare_versions(&release.version, current_ver)
                        == std::cmp::Ordering::Greater
            })
            .max_by(|a, b| compare_versions(&a.version, &b.version));
        match newer {
            Some(release) => {
                effective_options
                    .versions
                    .insert(project.id.clone(), release.version.clone());
                queue.push_back((project.id.clone(), false, project.name.clone(), None));
            }
            None if strict => {
                // Report why no update could be planned (unknown vs does-not-fit vs up-to-date).
                let has_newer_unknown = project.releases.iter().any(|r| {
                    compare_versions(&r.version, current_ver) == std::cmp::Ordering::Greater
                        && r.fit_verdict(build) == FitVerdict::Unknown
                        && index.revoked(&project.id, Some(&r.file.sha256)).is_none()
                });
                let has_newer_no_fit = project.releases.iter().any(|r| {
                    compare_versions(&r.version, current_ver) == std::cmp::Ordering::Greater
                        && r.fit_verdict(build) == FitVerdict::DoesNotFit
                        && index.revoked(&project.id, Some(&r.file.sha256)).is_none()
                });
                if has_newer_unknown {
                    return Err(InstallError::NotForVersion(trf(
                        "Fit for newer {mod} on Sunrise {build} is not known yet.",
                        &[("mod", &project.name), ("build", build)],
                    )));
                } else if has_newer_no_fit {
                    return Err(InstallError::NotForVersion(trf(
                        "Newer {mod} does not fit Sunrise {build}.",
                        &[("mod", &project.name), ("build", build)],
                    )));
                }
            }
            None => {}
        }
    }

    if queue.is_empty() {
        return Ok(V2Plan {
            steps: Vec::new(),
            optional_dependencies: Vec::new(),
        });
    }
    resolve_v2_queue(
        &ResolveContext {
            index,
            build,
            installed,
            chosen_pins,
            options: &effective_options,
            unfit_hashes: &unfit_hashes,
            choose_provider,
        },
        queue,
    )
}

struct ResolveContext<'a> {
    index: &'a Index,
    build: &'a str,
    installed: &'a BTreeMap<String, InstalledV2Mod>,
    chosen_pins: &'a BTreeMap<String, String>,
    options: &'a V2InstallOptions,
    unfit_hashes: &'a BTreeSet<String>,
    choose_provider: Option<&'a ChooseProvider<'a>>,
}

fn resolve_v2_queue(
    ctx: &ResolveContext<'_>,
    mut queue: VecDeque<Queued>,
) -> Result<V2Plan, InstallError> {
    let ResolveContext {
        index,
        build,
        installed,
        chosen_pins,
        options,
        unfit_hashes,
        choose_provider,
    } = *ctx;
    let mut planned: BTreeMap<String, PlannedV2Release> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut optional_dependencies: Vec<index_v2::Dependency> = Vec::new();

    while let Some((project_id, is_dependency, required_by, range)) = queue.pop_front() {
        let project = index.project(&project_id).ok_or_else(|| {
            InstallError::Refused(trf(
                "Project {project} is not in the Sunrise registry.",
                &[("project", &project_id)],
            ))
        })?;
        if let Some(existing) = planned.get_mut(&project.mod_id) {
            if existing.project_id == project.id {
                if let Some(range) = range
                    .as_ref()
                    .filter(|r| !in_range(Some(*r), &existing.version))
                {
                    return Err(InstallError::NotForVersion(trf(
                        "{mod} needs {dep} {range}, but {version} is planned for another mod.",
                        &[
                            ("mod", &required_by),
                            ("dep", &project.name),
                            ("range", &range.to_string()),
                            ("version", &existing.version),
                        ],
                    )));
                }
                if !is_dependency {
                    existing.dependency = false;
                }
                continue;
            }
            return Err(InstallError::ProviderConflict(Box::new(ProviderConflict {
                mod_id: project.mod_id.clone(),
                installed_project: Some(existing.project_id.clone()),
                installed_name: existing.name.clone(),
                requested_project: project.id.clone(),
                requested_name: project.name.clone(),
            })));
        }
        if let Some(existing) = installed.get(&project.mod_id) {
            if existing.ownership.project_id() != Some(project.id.as_str()) {
                return Err(InstallError::ProviderConflict(Box::new(ProviderConflict {
                    mod_id: project.mod_id.clone(),
                    installed_project: existing.ownership.project_id().map(str::to_owned),
                    installed_name: existing.package.manifest.name.clone(),
                    requested_project: project.id.clone(),
                    requested_name: project.name.clone(),
                })));
            }
        }
        let version_override = options
            .versions
            .get(&project.id)
            .map(String::as_str)
            .or_else(|| chosen_version_pin(chosen_pins, &project.id, &project.mod_id));
        let release = select_project_release(
            index,
            project,
            build,
            version_override,
            range.as_ref(),
            unfit_hashes,
        )?;
        let step = planned_from(project, release, is_dependency);
        order.push(project.mod_id.clone());
        planned.insert(project.mod_id.clone(), step);

        for dep in &release.dependencies {
            if !dep.required {
                optional_dependencies.push(dep.clone());
                continue;
            }
            if let Some(dep_project_id) = dep.project.as_deref() {
                let dep_project = index.project(dep_project_id).ok_or_else(|| {
                    InstallError::Refused(trf(
                        "{mod} needs project {project}, which is not in the Sunrise registry.",
                        &[("mod", &project.name), ("project", dep_project_id)],
                    ))
                })?;
                if dep_project.mod_id != dep.mod_id {
                    return Err(InstallError::Refused(trf(
                        "{mod} lists project {project} for {dep}, which provides a different mod.",
                        &[
                            ("mod", &project.name),
                            ("project", dep_project_id),
                            ("dep", &dep.mod_id),
                        ],
                    )));
                }
                if let Some(existing) = installed.get(&dep.mod_id) {
                    if existing.ownership.project_id() != Some(dep_project.id.as_str()) {
                        return Err(InstallError::ProviderConflict(Box::new(ProviderConflict {
                            mod_id: dep.mod_id.clone(),
                            installed_project: existing.ownership.project_id().map(str::to_owned),
                            installed_name: existing.package.manifest.name.clone(),
                            requested_project: dep_project.id.clone(),
                            requested_name: dep_project.name.clone(),
                        })));
                    }
                    if existing.enabled
                        && dep_project.releases.iter().any(|r| {
                            r.file.sha256.eq_ignore_ascii_case(&existing.digest)
                                && in_range(Some(&dep.version), &r.version)
                                && r.fit_verdict(build) == FitVerdict::Fits
                                && index
                                    .revoked(&dep_project.id, Some(&r.file.sha256))
                                    .is_none()
                        })
                    {
                        continue;
                    }
                }
                queue.push_back((
                    dep_project.id.clone(),
                    true,
                    project.name.clone(),
                    Some(dep.version.clone()),
                ));
                continue;
            }

            // Unpinned slot dependency (`dep.project == None`):
            if planned.contains_key(&dep.mod_id) {
                continue;
            }
            if let Some(existing) = installed.get(&dep.mod_id) {
                if existing.enabled {
                    // Keep the profile's existing provider for this slot.
                    if let Some(existing_pid) = existing.ownership.project_id() {
                        if let Some(existing_project) = index.project(existing_pid) {
                            let current_fits = existing_project.releases.iter().any(|r| {
                                r.file.sha256.eq_ignore_ascii_case(&existing.digest)
                                    && in_range(Some(&dep.version), &r.version)
                                    && r.fit_verdict(build) == FitVerdict::Fits
                                    && index
                                        .revoked(&existing_project.id, Some(&r.file.sha256))
                                        .is_none()
                            });
                            if !current_fits {
                                queue.push_back((
                                    existing_project.id.clone(),
                                    true,
                                    project.name.clone(),
                                    Some(dep.version.clone()),
                                ));
                            }
                        }
                    }
                    continue;
                }
            }
            if let Some(chosen_pid) = options.provider_choices.get(&dep.mod_id) {
                let chosen_project = index.project(chosen_pid).ok_or_else(|| {
                    InstallError::Refused(trf(
                        "Chosen provider {project} for {dep} is not in the Sunrise registry.",
                        &[("project", chosen_pid), ("dep", &dep.mod_id)],
                    ))
                })?;
                if chosen_project.mod_id != dep.mod_id {
                    return Err(InstallError::Refused(trf(
                        "Chosen provider {project} does not provide {dep}.",
                        &[("project", chosen_pid), ("dep", &dep.mod_id)],
                    )));
                }
                queue.push_back((
                    chosen_project.id.clone(),
                    true,
                    project.name.clone(),
                    Some(dep.version.clone()),
                ));
                continue;
            }

            // Find all non-revoked projects providing `dep.mod_id` with a fitting release for `build`.
            let mut candidates: Vec<ProviderCandidate> = Vec::new();
            let mut saw_unknown = false;
            for candidate in index.projects_for_mod(&dep.mod_id) {
                if !matches!(candidate.kind, V2Kind::Mod | V2Kind::NativeMod) {
                    continue;
                }
                if index.revoked(&candidate.id, None).is_some() {
                    continue;
                }
                let best = candidate
                    .releases
                    .iter()
                    .filter(|r| {
                        r.fit_verdict(build) == FitVerdict::Fits
                            && in_range(Some(&dep.version), &r.version)
                            && index.revoked(&candidate.id, Some(&r.file.sha256)).is_none()
                            && !unfit_hashes.contains(&r.file.sha256.to_ascii_lowercase())
                    })
                    .max_by(|a, b| compare_versions(&a.version, &b.version));
                if let Some(release) = best {
                    candidates.push(ProviderCandidate {
                        project_id: candidate.id.clone(),
                        slug: candidate.slug.clone(),
                        name: candidate.name.clone(),
                        repository_name: candidate.repository.name.clone(),
                        version: release.version.clone(),
                    });
                } else if candidate.releases.iter().any(|r| {
                    r.fit_verdict(build) == FitVerdict::Unknown
                        && in_range(Some(&dep.version), &r.version)
                        && index.revoked(&candidate.id, Some(&r.file.sha256)).is_none()
                }) {
                    saw_unknown = true;
                }
            }
            // Deterministic order independent of index JSON array order.
            candidates.sort_by(|a, b| a.project_id.cmp(&b.project_id));
            match candidates.as_slice() {
                [] => {
                    return Err(InstallError::NotForVersion(if saw_unknown {
                        trf(
                            "{mod} needs {dep}, whose fit on Sunrise {build} is not known yet.",
                            &[
                                ("mod", &project.name),
                                ("dep", &dep.mod_id),
                                ("build", build),
                            ],
                        )
                    } else {
                        trf(
                            "{mod} needs {dep} {range}, which has no release fitting Sunrise {build}.",
                            &[
                                ("mod", &project.name),
                                ("dep", &dep.mod_id),
                                ("range", &dep.version.to_string()),
                                ("build", build),
                            ],
                        )
                    }));
                }
                [only] => {
                    queue.push_back((
                        only.project_id.clone(),
                        true,
                        project.name.clone(),
                        Some(dep.version.clone()),
                    ));
                }
                _ => {
                    let need = ProviderChoiceNeeded {
                        mod_id: dep.mod_id.clone(),
                        required_by: project.name.clone(),
                        candidates,
                    };
                    if let Some(picked) = choose_provider.and_then(|ask| ask(&need)) {
                        if need.candidates.iter().any(|c| c.project_id == picked) {
                            queue.push_back((
                                picked,
                                true,
                                project.name.clone(),
                                Some(dep.version.clone()),
                            ));
                            continue;
                        }
                    }
                    return Err(InstallError::AmbiguousProvider(Box::new(need)));
                }
            }
        }
    }

    let steps = order
        .into_iter()
        .filter_map(|slot| planned.remove(&slot))
        .collect();
    Ok(V2Plan {
        steps,
        optional_dependencies,
    })
}

/// Default production HTTPS byte fetcher for v2 release `.smod` archives and `.sigstore.json`
/// attestation bundles.
pub(crate) fn fetch_release_bytes(url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    if !download::github_release_asset(url) {
        return Err(format!(
            "{url} is not a GitHub release asset URL allowed by the Sunrise v2 registry."
        ));
    }
    let agent = download::agent(url)?;
    let response = agent
        .get(url)
        .call()
        .map_err(|e| format!("download failed for {url}: {e}"))?;
    let mut limited = response.into_reader().take(max_bytes.saturating_add(1));
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|e| format!("reading {url}: {e}"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "{url} exceeded maximum allowed size of {max_bytes} bytes"
        ));
    }
    Ok(bytes)
}

/// Download and verification callbacks for staging schema-2 registry releases.
#[derive(Clone, Copy)]
pub(crate) struct V2Transfer<'a> {
    pub(crate) fetch_bytes: BytesFetcher<'a>,
    pub(crate) verify_release: ReleaseVerifier<'a>,
}

/// Shared inputs for schema-2 registry install, update, and provider-switch edits.
pub(crate) struct V2EditRequest<'a> {
    pub(crate) instance: &'a Instance,
    pub(crate) identity: Identity,
    pub(crate) index: Arc<Index>,
    pub(crate) options: V2InstallOptions,
    pub(crate) transfer: V2Transfer<'a>,
}

pub(crate) fn stage_and_verify_v2_release(
    index: &Index,
    home: &Path,
    step: &PlannedV2Release,
    pin_dir: &Path,
    accepted_publisher_changes: &BTreeSet<String>,
    confirm_publisher_change: Option<ConfirmPublisherChange<'_>>,
    transfer: V2Transfer<'_>,
) -> Result<(installed::Entry, PendingPin), InstallError> {
    let V2Transfer {
        fetch_bytes,
        verify_release,
    } = transfer;
    let project = index.project(&step.project_id).ok_or_else(|| {
        InstallError::Refused(trf(
            "Project {project} is not in the Sunrise registry.",
            &[("project", &step.project_id)],
        ))
    })?;
    let release = project
        .releases
        .iter()
        .find(|r| r.version == step.version && r.file.sha256.eq_ignore_ascii_case(&step.sha256))
        .ok_or_else(|| {
            InstallError::Refused(trf(
                "Release {version} of {mod} is no longer in the Sunrise registry.",
                &[("version", &step.version), ("mod", &step.name)],
            ))
        })?;
    if let Some(revoked) = index.revoked(&project.id, Some(&release.file.sha256)) {
        return Err(InstallError::Refused(trf(
            "{mod} was taken down: {reason}",
            &[("mod", &project.name), ("reason", &revoked.reason)],
        )));
    }

    // 1. Download .smod and check exact byte length + SHA-256.
    let archive_bytes = fetch_bytes(&release.file.url, MAX_FILE).map_err(InstallError::Download)?;
    if archive_bytes.len() as u64 != release.file.size {
        return Err(InstallError::Download(trf(
            "{mod} download size did not match the registry ({got} bytes, expected {expected}).",
            &[
                ("mod", &project.name),
                ("got", &archive_bytes.len().to_string()),
                ("expected", &release.file.size.to_string()),
            ],
        )));
    }
    let actual_sha256 = index::sha256(&archive_bytes);
    if !actual_sha256.eq_ignore_ascii_case(&release.file.sha256) {
        return Err(InstallError::Download(trf(
            "{mod} failed its SHA-256 check.",
            &[("mod", &project.name)],
        )));
    }
    if let Some(revoked) = index.revoked_by_sha256(&actual_sha256) {
        return Err(InstallError::Refused(trf(
            "{mod} was taken down: {reason}",
            &[("mod", &project.name), ("reason", &revoked.reason)],
        )));
    }
    let digest_bytes = attestation::parse_sha256_hex(&actual_sha256).ok_or_else(|| {
        InstallError::Download(trf(
            "{mod} failed its SHA-256 check.",
            &[("mod", &project.name)],
        ))
    })?;

    // 2. Download .sigstore.json bundle and verify offline attestation + Sunrise release policy.
    let bundle_bytes = fetch_bytes(&release.file.attestation, MAX_ATTESTATION_BYTES)
        .map_err(InstallError::Download)?;
    let expected = ExpectedRelease {
        repository_id: project.repository.id,
        workflow: &release.workflow,
    };
    let verified = verify_release(&bundle_bytes, digest_bytes, &expected).map_err(|error| {
        InstallError::Refused(trf(
            "Could not verify the release signature for {mod}: {detail}",
            &[("mod", &project.name), ("detail", &error.to_string())],
        ))
    })?;

    // 3. Check per-computer publisher pin (without mutating disk until publication commits).
    let pin = verified.pin();
    let mut accepted_change = None;
    match attestation::check_pin_in(pin_dir, &project.id, &pin) {
        Ok(attestation::PinStatus::FirstInstall | attestation::PinStatus::Matched) => {}
        Err(attestation::Error::PublisherChanged(change)) => {
            let confirmed = accepted_publisher_changes.contains(&project.id)
                || confirm_publisher_change.is_some_and(|ask| {
                    ask(&PublisherChangeQuestion {
                        mod_name: project.name.clone(),
                        current_version: Document::read(home).ok().and_then(|doc| {
                            doc.row(&step.mod_id)
                                .ok()
                                .flatten()
                                .and_then(|row| row["version"].as_str().map(str::to_owned))
                        }),
                        incoming_version: release.version.clone(),
                        change: (*change).clone(),
                    })
                });
            if !confirmed {
                return Err(InstallError::PublisherChanged(change));
            }
            accepted_change = Some(*change);
        }
        Err(error) => return Err(InstallError::Unproved(error.to_string())),
    }

    // 4. Write archive into the candidate's target directory and inspect its manifest & layout.
    let folder = match project.kind {
        V2Kind::NativeMod => crate::native_mods::INSTALLED,
        V2Kind::Mod => mod_list::MODS_DIR,
        _ => {
            return Err(InstallError::Refused(trf(
                "{mod} is not a mod package.",
                &[("mod", &project.name)],
            )));
        }
    };
    let other_folder = if folder == crate::native_mods::INSTALLED {
        mod_list::MODS_DIR
    } else {
        crate::native_mods::INSTALLED
    };
    // If a mod changed kind (content <-> native), remove any old file in the sibling folder so
    // `scan_packages` never sees two archives for the same slot.
    let sibling = home
        .join(other_folder)
        .join(format!("{}.smod", project.mod_id));
    if sibling.exists() {
        fs::remove_file(&sibling).map_err(|e| InstallError::Unproved(at(&sibling, e)))?;
    }

    let relative = format!("{folder}/{}.smod", project.mod_id);
    let destination = home.join(&relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|e| InstallError::Unproved(at(parent, e)))?;
    }
    fs::write(&destination, &archive_bytes)
        .map_err(|e| InstallError::Unproved(at(&destination, e)))?;

    let smod_kind = match project.kind {
        V2Kind::NativeMod => index::Kind::NativeMod,
        V2Kind::Mod => index::Kind::Mod,
        _ => unreachable!(),
    };
    let facts = index::inspect(&smod_kind, &destination).map_err(|error| {
        let _ = fs::remove_file(&destination);
        InstallError::Download(trf(
            "{mod} is not a valid package: {detail}",
            &[("mod", &project.name), ("detail", &error)],
        ))
    })?;
    if facts.id.as_deref() != Some(project.mod_id.as_str())
        || facts.version.as_deref() != Some(release.version.as_str())
    {
        let _ = fs::remove_file(&destination);
        return Err(InstallError::Download(trf(
            "{mod} archive manifest did not match the registry entry.",
            &[("mod", &project.name)],
        )));
    }
    // Refuse packages that smuggle required dependencies not declared in the signed v2 release.
    for (dep_id, dep) in &facts.dependencies {
        if !dep.details().optional
            && !release
                .dependencies
                .iter()
                .any(|listed| listed.required && listed.mod_id == *dep_id)
        {
            let _ = fs::remove_file(&destination);
            return Err(InstallError::Download(trf(
                "{mod} archive declares an unlisted required dependency ({dep}).",
                &[("mod", &project.name), ("dep", dep_id)],
            )));
        }
    }

    // The signed index listed these bytes and their attestation verified: a build takes them
    // without the unreviewed-code warning.
    if matches!(project.kind, V2Kind::NativeMod) {
        backend::allow_native_files_in(pin_dir, [actual_sha256.clone()])
            .map_err(InstallError::Unproved)?;
    }
    let entry = installed::Entry {
        source: SourceId::Sunrise,
        project: project.id.clone(),
        mod_id: Some(project.mod_id.clone()),
        slug: project.slug.clone(),
        title: project.name.clone(),
        kind: Kind::Mod,
        version: release.version.clone(),
        number: release.version.clone(),
        file: relative,
        checksum: format!("SHA-256:{actual_sha256}"),
        page: format!("{SITE}/{}", project.slug),
        dependency: step.dependency,
        verdict: Some(tr("Verified").into()),
        install_hold: None,
        repository: Some(installed::VerifiedRepository {
            id: verified.attested.repository_id,
            name: verified.attested.repository_name.clone(),
            path: project.repository.path.clone(),
            workflow: release.workflow.clone(),
        }),
    };
    Ok((
        entry,
        PendingPin {
            project_id: project.id.clone(),
            pin,
            accepted_change,
        },
    ))
}

pub(crate) fn commit_pending_pins(pin_dir: &Path, pins: &[PendingPin]) -> Result<(), String> {
    for pending in pins {
        if let Some(change) = &pending.accepted_change {
            attestation::accept_pin_in(pin_dir, change).map_err(|e| e.to_string())?;
        } else {
            attestation::pin_or_check_in(pin_dir, &pending.project_id, &pending.pin)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// The index a change plans against: the signed one, with this computer's fit checks filling
/// only what it says nothing of.
fn local_fit_filled(index: &Index) -> Arc<Index> {
    Arc::new(crate::native_mods::local_fit::filled(index))
}

/// Install one or more schema-2 registry projects (by permanent 8-char project ID) into `instance`.
pub fn install_registry_v2(
    instance: &Instance,
    identity: Identity,
    requested_projects: Vec<String>,
    options: V2InstallOptions,
    choose: impl FnOnce() -> Result<FirstBuild, String>,
    hooks: V2Hooks<'_>,
) -> Result<InstalledBatch, InstallError> {
    let catalog =
        catalogue_v2::change_index().map_err(|error| InstallError::Unproved(error.to_string()))?;
    install_registry_v2_with(
        V2EditRequest {
            instance,
            identity,
            index: local_fit_filled(&catalog.index),
            options,
            transfer: V2Transfer {
                fetch_bytes: &fetch_release_bytes,
                verify_release: &attestation::verify_release,
            },
        },
        requested_projects,
        choose,
        hooks,
    )
}

/// Update installed schema-2 registry mods in `instance`, always preserving each mod's chosen
/// project and respecting `chosen-versions.json` pins during bulk updates.
pub fn update_registry_v2(
    instance: &Instance,
    identity: Identity,
    requested: UpdateRequest,
    options: V2InstallOptions,
    choose: impl FnOnce() -> Result<FirstBuild, String>,
    hooks: V2Hooks<'_>,
    approve: &dyn Fn(&ChangeImpact) -> bool,
) -> Result<InstalledBatch, InstallError> {
    let catalog =
        catalogue_v2::change_index().map_err(|error| InstallError::Unproved(error.to_string()))?;
    update_registry_v2_with(
        V2EditRequest {
            instance,
            identity,
            index: local_fit_filled(&catalog.index),
            options,
            transfer: V2Transfer {
                fetch_bytes: &fetch_release_bytes,
                verify_release: &attestation::verify_release,
            },
        },
        requested,
        choose,
        hooks,
        approve,
    )
}

/// Explicitly switch an installed mod slot (`mod_id`) to a different v2 project (`to_project_id`),
/// previewing affected dependents and saved-world risks before committing.
pub fn switch_provider_v2(
    instance: &Instance,
    identity: Identity,
    mod_id: &str,
    to_project_id: &str,
    options: V2InstallOptions,
    choose: impl FnOnce() -> Result<FirstBuild, String>,
    hooks: V2ProviderSwitchHooks<'_>,
) -> Result<InstalledBatch, InstallError> {
    let catalog =
        catalogue_v2::change_index().map_err(|error| InstallError::Unproved(error.to_string()))?;
    switch_provider_v2_with(
        V2EditRequest {
            instance,
            identity,
            index: local_fit_filled(&catalog.index),
            options,
            transfer: V2Transfer {
                fetch_bytes: &fetch_release_bytes,
                verify_release: &attestation::verify_release,
            },
        },
        mod_id,
        to_project_id,
        choose,
        hooks,
    )
}

pub(crate) fn install_registry_v2_with(
    request: V2EditRequest<'_>,
    requested_projects: Vec<String>,
    choose: impl FnOnce() -> Result<FirstBuild, String>,
    hooks: V2Hooks<'_>,
) -> Result<InstalledBatch, InstallError> {
    v2_edit_with(
        request,
        V2Action::Install(requested_projects),
        choose,
        hooks,
    )
}

pub(crate) fn update_registry_v2_with(
    request: V2EditRequest<'_>,
    requested: UpdateRequest,
    choose: impl FnOnce() -> Result<FirstBuild, String>,
    hooks: V2Hooks<'_>,
    approve: &dyn Fn(&ChangeImpact) -> bool,
) -> Result<InstalledBatch, InstallError> {
    v2_edit_with(request, V2Action::Update(requested, approve), choose, hooks)
}

enum V2Action<'a> {
    Install(Vec<String>),
    Update(UpdateRequest, &'a dyn Fn(&ChangeImpact) -> bool),
}

fn v2_edit_with(
    request: V2EditRequest<'_>,
    action: V2Action<'_>,
    choose: impl FnOnce() -> Result<FirstBuild, String>,
    hooks: V2Hooks<'_>,
) -> Result<InstalledBatch, InstallError> {
    let V2EditRequest {
        instance,
        identity,
        index,
        options,
        transfer,
    } = request;
    if instance.kind == InstanceKind::Pack {
        return Err(InstallError::Refused(
            tr("No registry mods to install in this profile.").into(),
        ));
    }
    (hooks.install.progress)(InstallProgress::Checking);
    if matches!(&action, V2Action::Install(ids) if ids.is_empty()) {
        return Err(InstallError::Cancelled);
    }

    let mut profile =
        Profile::open_for(&instance.home, None, Some(identity)).map_err(String::from)?;
    let edit = profile.begin_edit(choose, |f| {
        (hooks.install.progress)(InstallProgress::Downloading(f))
    })?;
    let base_build = edit.base().to_owned();
    let default_cancelled = native_mods::Cancellation::default();
    let cancelled = hooks.install.cancellation.unwrap_or(&default_cancelled);
    let old = edit.candidate()?;
    let before = inventory_v2(old.path(), &index)?;
    let chosen_pins = chosen(&instance.home);

    let plan = match &action {
        V2Action::Install(projects) => plan_v2_install(
            &index,
            &base_build,
            projects,
            &before,
            &chosen_pins,
            &options,
            hooks.choose_provider,
        )?,
        V2Action::Update(request, _) => plan_v2_update(
            &index,
            &base_build,
            request,
            &before,
            &chosen_pins,
            &options,
            hooks.choose_provider,
        )?,
    };
    if plan.steps.is_empty() {
        return Ok(InstalledBatch::default());
    }

    let pin_dir = options.pin_dir.clone().unwrap_or_else(backend::data_dir);
    let next = edit.candidate()?;
    let mut doc = Document::read(next.path())?;
    let mut entries: BTreeMap<String, installed::Entry> = BTreeMap::new();
    let mut pending_pins: Vec<PendingPin> = Vec::new();
    let mut native = false;
    let mut replacements: Vec<(String, String)> = Vec::new();
    let mut retired_holds: BTreeSet<String> = BTreeSet::new();

    for (slot_idx, step) in plan.steps.iter().enumerate() {
        (hooks.install.progress)(InstallProgress::Downloading(
            slot_idx as f32 / plan.steps.len().max(1) as f32,
        ));
        let (mut entry, pin) = stage_and_verify_v2_release(
            &index,
            next.path(),
            step,
            &pin_dir,
            &options.accepted_publisher_changes,
            hooks.confirm_publisher_change,
            transfer,
        )?;
        // Preserve player-owned dependency flag if previously explicitly installed.
        if let Ok(Some(prev_row)) = doc.row(&step.mod_id) {
            if prev_row["dependency"].as_bool() == Some(false) {
                entry.dependency = false;
            }
        }
        // Check active Switch hold authority before replacing a held archive.
        if let Some(held) = edit.held(&step.mod_id) {
            let can_replace = matches!(action, V2Action::Update(..))
                && before.get(&step.mod_id).is_some_and(|prev| {
                    prev.ownership.is_owned()
                        && matches!(held.source(), managed_play::HeldSource::Registry { .. })
                        && prev.digest.eq_ignore_ascii_case(&{
                            let d = sha2::digest::Output::<sha2::Sha256>::from(*held.digest());
                            format!("{d:x}")
                        })
                });
            if !can_replace {
                return Err(held_change_refusal(&step.mod_id));
            }
            retired_holds.insert(step.mod_id.clone());
        }
        doc.replace(&entry)?;
        mod_list::set_enabled(next.path(), &step.mod_id, true)?;
        native |= step.kind == V2Kind::NativeMod;
        if let Some(prev) = before.get(&step.mod_id) {
            replacements.push((prev.package.manifest.name.clone(), step.mod_id.clone()));
        }
        entries.insert(step.mod_id.clone(), entry);
        pending_pins.push(pin);
    }
    doc.write(next.path())?;

    // Check enabled manifests and v2 revocations on the staged candidate before any build.
    check_staged_v2_candidate(next.path(), &index, |id| entries.contains_key(id))?;

    // A reviewed registry release asks nothing before its build: its attestation and the
    // signed index were verified while it was fetched and staged above.
    if native {
        if let Some(error) = native_mods::known_host_block(Some(&base_build)) {
            return Err(InstallError::Host(error));
        }
        cancelled.check().map_err(InstallError::Unproved)?;
    }

    (hooks.install.progress)(InstallProgress::Preparing);
    edit.prepare_first(&|_| {})
        .map_err(InstallError::Unproved)?;
    let proof = match prove_edit(&edit, next.path(), instance, cancelled, &hooks.install) {
        Ok(proof) => proof,
        Err(native_mods::Failed::Mods {
            left,
            error,
            compiling: false,
            ..
        }) => {
            for failed in &left {
                if matches!(failed.why, native_mods::Why::Unfit(_)) {
                    if let Some(step) = plan.steps.iter().find(|step| {
                        step.kind == V2Kind::NativeMod
                            && step.mod_id == failed.id
                            && step.version == failed.version
                    }) {
                        remember_unfit_sha256(&base_build, &step.sha256);
                    }
                }
            }
            return Err(InstallError::Unproved(error));
        }
        Err(_) if cancelled.is_stopped() => return Err(InstallError::Cancelled),
        Err(error) => return Err(InstallError::Unproved(error.to_string())),
    };

    // World impact review for updates (or guard against unexpected replacements on Install).
    let (suspect, reviewed_worlds) = match &action {
        V2Action::Install(_) => {
            // A dependency that replaces an installed mod may drop what saved worlds use.
            let suspect = if replacements.is_empty() {
                crate::worlds::SuspectMods::new()
            } else {
                crate::worlds::unproved_mod_effects(
                    old.path(),
                    edit.current(),
                    next.path(),
                    edit.current(),
                    replacements.iter().map(|(_, id)| id.as_str()),
                )?
            };
            let reviewed = guard_replacements(
                &instance.home,
                &suspect,
                replacements.iter().map(|(name, _)| name.as_str()),
            )?;
            (suspect, reviewed)
        }
        V2Action::Update(_, approve) => {
            let old_packages: Vec<&ScannedPackage> = entries
                .keys()
                .filter_map(|id| before.get(id).map(|m| &m.package))
                .collect();
            let (suspect, unchecked) = mutations::update_worlds(next.path(), old_packages)?;
            let reviewed_updates: Vec<mutations::ReviewedUpdate> = entries
                .iter()
                .map(|(id, entry)| mutations::ReviewedUpdate {
                    name: entry.title.clone(),
                    installed: before.get(id).map(|p| p.package.manifest.version.clone()),
                    replacement: entry.version.clone(),
                })
                .collect();
            let impact = ChangeImpact::read(
                &instance.home,
                mutations::Change::Update(reviewed_updates),
                &suspect,
            )?
            .with_unchecked_update(unchecked);
            if !approve(&impact) {
                return Err(InstallError::Cancelled);
            }
            (suspect, impact.into_evidence())
        }
    };

    (hooks.install.progress)(InstallProgress::Publishing);
    cancelled.check().map_err(|_| InstallError::Cancelled)?;
    // Persist verified publisher pins before publishing so a pin-store failure aborts the
    // install cleanly instead of failing after the mods are already live.
    commit_pending_pins(&pin_dir, &pending_pins).map_err(InstallError::Unproved)?;
    let authorize = || mutations::bind_worlds(&instance.home, &suspect, &reviewed_worlds);
    edit.publish_install(proof, cancelled, &retired_holds, authorize)?;

    let mut result = InstalledBatch::default();
    for (id, entry) in entries {
        if let Some(previous) = before.get(&id) {
            result
                .updated
                .push((previous.package.manifest.version.clone(), entry));
        } else {
            result.added.push(entry);
        }
    }
    Ok(result)
}

/// Internal testable entrypoint for explicit provider switching.
pub(crate) fn switch_provider_v2_with(
    request: V2EditRequest<'_>,
    mod_id: &str,
    to_project_id: &str,
    choose: impl FnOnce() -> Result<FirstBuild, String>,
    hooks: V2ProviderSwitchHooks<'_>,
) -> Result<InstalledBatch, InstallError> {
    let V2EditRequest {
        instance,
        identity,
        index,
        options,
        transfer,
    } = request;
    if instance.kind == InstanceKind::Pack {
        return Err(InstallError::Refused(
            tr("No registry mods to install in this profile.").into(),
        ));
    }
    let to_project = index.project(to_project_id).ok_or_else(|| {
        InstallError::Refused(trf(
            "Project {project} is not in the Sunrise registry.",
            &[("project", to_project_id)],
        ))
    })?;
    if to_project.mod_id != mod_id {
        return Err(InstallError::Refused(trf(
            "{project} provides {actual}, not {expected}.",
            &[
                ("project", &to_project.name),
                ("actual", &to_project.mod_id),
                ("expected", mod_id),
            ],
        )));
    }
    if !matches!(to_project.kind, V2Kind::Mod | V2Kind::NativeMod) {
        return Err(InstallError::Refused(trf(
            "{mod} is not a mod package.",
            &[("mod", &to_project.name)],
        )));
    }

    (hooks.install.install.progress)(InstallProgress::Checking);
    let mut profile =
        Profile::open_for(&instance.home, None, Some(identity)).map_err(String::from)?;
    let edit = profile.begin_edit(choose, |f| {
        (hooks.install.install.progress)(InstallProgress::Downloading(f))
    })?;
    let base_build = edit.base().to_owned();
    let default_cancelled = native_mods::Cancellation::default();
    let cancelled = hooks
        .install
        .install
        .cancellation
        .unwrap_or(&default_cancelled);
    let old = edit.candidate()?;
    let before = inventory_v2(old.path(), &index)?;

    let current = before.get(mod_id).ok_or_else(|| {
        InstallError::Refused(trf(
            "{mod} is not installed in this profile.",
            &[("mod", mod_id)],
        ))
    })?;
    if current.ownership.project_id() == Some(to_project.id.as_str()) {
        return Err(InstallError::Refused(trf(
            "{mod} is already using {project}.",
            &[("mod", mod_id), ("project", &to_project.name)],
        )));
    }

    let unfit_hashes = known_unfit_updates(&base_build);
    let version_override = options.versions.get(&to_project.id).map(String::as_str);
    let release = select_project_release(
        &index,
        to_project,
        &base_build,
        version_override,
        None,
        &unfit_hashes,
    )?;
    let step = planned_from(to_project, release, false);

    // An enabled dependent pinned to the old project, or whose requirement the new release can't
    // meet, refuses the switch before anything downloads.
    if let Some(incompatible) =
        find_affected_dependents(&index, &before, mod_id, &to_project.id, &release.version).first()
    {
        return Err(InstallError::Refused(trf(
            "Cannot switch {mod} to {project}: {dependent} ({reason}).",
            &[
                ("mod", mod_id),
                ("project", &to_project.name),
                ("dependent", &incompatible.name),
                ("reason", &incompatible.reason),
            ],
        )));
    }

    // Plan any new required dependencies of the incoming provider.
    let mut dep_queue: VecDeque<Queued> = VecDeque::new();
    for dep in &release.dependencies {
        if !dep.required || dep.mod_id == mod_id {
            continue;
        }
        let range = Some(dep.version.clone());
        let installed = before.get(&dep.mod_id).filter(|m| m.enabled);
        let in_range_now =
            installed.is_some_and(|m| in_range(Some(&dep.version), &m.package.manifest.version));
        if let Some(dep_pid) = dep.project.as_deref() {
            if installed.is_none_or(|m| m.ownership.project_id() != Some(dep_pid)) || !in_range_now
            {
                dep_queue.push_back((dep_pid.to_owned(), true, to_project.name.clone(), range));
            }
        } else if let Some(owner) = installed
            .filter(|_| !in_range_now)
            .and_then(|m| m.ownership.project_id())
        {
            // Keep the profile's provider, at a release in the range.
            dep_queue.push_back((owner.to_owned(), true, to_project.name.clone(), range));
        } else if installed.is_none() {
            if let Some(picked) = options.provider_choices.get(&dep.mod_id) {
                dep_queue.push_back((picked.clone(), true, to_project.name.clone(), range));
            } else {
                let mut candidates: Vec<ProviderCandidate> = index
                    .projects_for_mod(&dep.mod_id)
                    .filter(|p| {
                        matches!(p.kind, V2Kind::Mod | V2Kind::NativeMod)
                            && index.revoked(&p.id, None).is_none()
                    })
                    .filter_map(|p| {
                        let best = p
                            .releases
                            .iter()
                            .filter(|r| {
                                r.fit_verdict(&base_build) == FitVerdict::Fits
                                    && in_range(Some(&dep.version), &r.version)
                                    && index.revoked(&p.id, Some(&r.file.sha256)).is_none()
                            })
                            .max_by(|a, b| compare_versions(&a.version, &b.version))?;
                        Some(ProviderCandidate {
                            project_id: p.id.clone(),
                            slug: p.slug.clone(),
                            name: p.name.clone(),
                            repository_name: p.repository.name.clone(),
                            version: best.version.clone(),
                        })
                    })
                    .collect();
                candidates.sort_by(|a, b| a.project_id.cmp(&b.project_id));
                match candidates.as_slice() {
                    [] => {
                        return Err(InstallError::NotForVersion(trf(
                            "{mod} needs {dep} {range}, which has no release fitting Sunrise {build}.",
                            &[
                                ("mod", &to_project.name),
                                ("dep", &dep.mod_id),
                                ("range", &dep.version.to_string()),
                                ("build", &base_build),
                            ],
                        )));
                    }
                    [only] => dep_queue.push_back((
                        only.project_id.clone(),
                        true,
                        to_project.name.clone(),
                        range,
                    )),
                    _ => {
                        let need = ProviderChoiceNeeded {
                            mod_id: dep.mod_id.clone(),
                            required_by: to_project.name.clone(),
                            candidates,
                        };
                        if let Some(picked) =
                            hooks.install.choose_provider.and_then(|ask| ask(&need))
                        {
                            if need.candidates.iter().any(|c| c.project_id == picked) {
                                dep_queue.push_back((picked, true, to_project.name.clone(), range));
                                continue;
                            }
                        }
                        return Err(InstallError::AmbiguousProvider(Box::new(need)));
                    }
                }
            }
        }
    }
    let dep_steps = if dep_queue.is_empty() {
        Vec::new()
    } else {
        let chosen_pins = chosen(&instance.home);
        resolve_v2_queue(
            &ResolveContext {
                index: &index,
                build: &base_build,
                installed: &before,
                chosen_pins: &chosen_pins,
                options: &options,
                unfit_hashes: &unfit_hashes,
                choose_provider: hooks.install.choose_provider,
            },
            dep_queue,
        )?
        .steps
    };

    let pin_dir = options.pin_dir.clone().unwrap_or_else(backend::data_dir);
    let next = edit.candidate()?;
    let mut doc = Document::read(next.path())?;
    let mut entries: BTreeMap<String, installed::Entry> = BTreeMap::new();
    let mut pending_pins: Vec<PendingPin> = Vec::new();
    let mut native = false;

    for planned_step in std::iter::once(&step).chain(dep_steps.iter()) {
        let (entry, pin) = stage_and_verify_v2_release(
            &index,
            next.path(),
            planned_step,
            &pin_dir,
            &options.accepted_publisher_changes,
            hooks.install.confirm_publisher_change,
            transfer,
        )?;
        doc.replace(&entry)?;
        mod_list::set_enabled(next.path(), &planned_step.mod_id, true)?;
        native |= planned_step.kind == V2Kind::NativeMod;
        entries.insert(planned_step.mod_id.clone(), entry);
        pending_pins.push(pin);
    }
    doc.write(next.path())?;

    check_staged_v2_candidate(next.path(), &index, |id| entries.contains_key(id))?;

    // A reviewed registry release asks nothing before its build: its attestation and the
    // signed index were verified while it was fetched and staged above.
    if native {
        if let Some(error) = native_mods::known_host_block(Some(&base_build)) {
            return Err(InstallError::Host(error));
        }
        cancelled.check().map_err(InstallError::Unproved)?;
    }

    (hooks.install.install.progress)(InstallProgress::Preparing);
    edit.prepare_first(&|_| {})
        .map_err(InstallError::Unproved)?;
    let proof = prove_edit(
        &edit,
        next.path(),
        instance,
        cancelled,
        &hooks.install.install,
    )
    .map_err(|error| {
        if cancelled.is_stopped() {
            InstallError::Cancelled
        } else {
            InstallError::Unproved(error.to_string())
        }
    })?;

    let (suspect, _) = mutations::update_worlds(next.path(), std::iter::once(&current.package))?;
    let worlds = mutations::world_evidence(&instance.home, &suspect)?;
    let preview = ProviderSwitchPreview {
        mod_id: mod_id.to_owned(),
        from_project: current.ownership.project_id().map(str::to_owned),
        from_name: current.package.manifest.name.clone(),
        to_project: to_project.id.clone(),
        to_name: to_project.name.clone(),
        to_version: release.version.clone(),
        worlds: worlds.clone(),
    };
    if !(hooks.approve_switch)(&preview) {
        return Err(InstallError::Cancelled);
    }

    (hooks.install.install.progress)(InstallProgress::Publishing);
    cancelled.check().map_err(|_| InstallError::Cancelled)?;
    commit_pending_pins(&pin_dir, &pending_pins).map_err(InstallError::Unproved)?;
    let authorize = || mutations::bind_worlds(&instance.home, &suspect, &worlds);
    edit.publish_install(proof, cancelled, &BTreeSet::new(), authorize)?;

    let mut result = InstalledBatch::default();
    for (id, entry) in entries {
        if let Some(previous) = before.get(&id) {
            result
                .updated
                .push((previous.package.manifest.version.clone(), entry));
        } else {
            result.added.push(entry);
        }
    }
    Ok(result)
}

fn find_affected_dependents(
    index: &Index,
    installed: &BTreeMap<String, InstalledV2Mod>,
    slot_mod_id: &str,
    to_project_id: &str,
    to_version: &str,
) -> Vec<V2AffectedDependent> {
    let parsed_new = parse_version(to_version).ok();
    let mut affected = Vec::new();
    for (id, item) in installed {
        if id == slot_mod_id || !item.enabled {
            continue;
        }
        // Check v2 release-level pinned-project dependency.
        if let Some(dep_pid) = item.ownership.project_id() {
            if let Some(project) = index.project(dep_pid) {
                if let Some(rel) = project
                    .releases
                    .iter()
                    .find(|r| r.file.sha256.eq_ignore_ascii_case(&item.digest))
                {
                    for dep in &rel.dependencies {
                        if let Some(req) = dep.project.as_deref() {
                            if dep.required && dep.mod_id == slot_mod_id && req != to_project_id {
                                affected.push(V2AffectedDependent {
                                    mod_id: id.clone(),
                                    name: item.package.manifest.name.clone(),
                                    reason: trf(
                                        "requires project {project} specifically",
                                        &[("project", req)],
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }
        // Check manifest semver constraint against the new provider's version.
        if let Some(dep) = item.package.manifest.dependencies.get(slot_mod_id) {
            let details = dep.details();
            if !details.optional {
                let fits = parsed_new
                    .as_ref()
                    .is_none_or(|ver| details.version.matches(ver));
                if !fits {
                    let req = details.version.to_string();
                    affected.push(V2AffectedDependent {
                        mod_id: id.clone(),
                        name: item.package.manifest.name.clone(),
                        reason: trf(
                            "needs {mod} {req}, not {version}",
                            &[("mod", slot_mod_id), ("req", &req), ("version", to_version)],
                        ),
                    });
                }
            }
        }
    }
    affected
}

/// Validate that all enabled packages in staged candidate `home` have no manifest conflicts and no
/// active v2 revocations.
pub(crate) fn check_staged_v2_candidate(
    home: &Path,
    index: &Index,
    staged: impl Fn(&str) -> bool,
) -> Result<(), InstallError> {
    let current = inventory_v2(home, index)?;
    let enabled_manifests: BTreeMap<&str, &sunrise_launcher_smod::manifest::Manifest> = current
        .iter()
        .filter(|(_, m)| m.enabled)
        .map(|(k, m)| (k.as_str(), &m.package.manifest))
        .collect();
    for item in current.values() {
        if !item.enabled || item.package.directory {
            continue;
        }
        if let Some(revoked) = revoked_for_package(index, &item.ownership, &item.digest) {
            return Err(InstallError::Refused(trf(
                "{mod} was taken down: {reason}",
                &[
                    ("mod", &item.package.manifest.name),
                    ("reason", &revoked.reason),
                ],
            )));
        }
        if let Some(missing) = missing_required(&item.package.manifest, &enabled_manifests) {
            return Err(InstallError::Refused(trf(
                "{mod} needs {dep} to be installed and enabled.",
                &[("mod", &item.package.manifest.name), ("dep", missing)],
            )));
        }
    }
    if let Some(conflict) = manifest_conflict(home, Side::Both, staged)? {
        return Err(InstallError::Refused(conflict.reason));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
