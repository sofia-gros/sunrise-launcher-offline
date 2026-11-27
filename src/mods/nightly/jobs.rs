//! Read-only inspection and explicit update offers. Only the shared installer can publish.
use super::*;
use crate::mods_page::install::{SourcePicks, SourceTarget};
use std::collections::BTreeMap;
use sunrise_launcher::browser::sunrise::{PreparedSource, SourceError, SourceProof, SourceRequest};
use sunrise_launcher::github_mods::{Origin, SourceTrack};
use sunrise_launcher::native_mods::Cancellation;

pub(in crate::mods) enum Event {
    Catalogue(Catalogue),
    Detail(u64, Box<Inspection>),
    Icon(sources::SourceIcon, Option<PathBuf>),
    Updates(Box<CheckedUpdates>),
}

pub(in crate::mods) struct Open {
    pub(in crate::mods) source: Source,
    pub(super) from: Page,
    pub(super) scope: OpenScope,
    pub(super) request: u64,
    pub(super) inspection: Option<Inspection>,
    /// Add was pressed after a refusal: add the source once it has been read again.
    pub(super) add_when_read: bool,
}

pub(in crate::mods) struct Inspection {
    pub(super) data: sources::Details,
}

pub(in crate::mods) struct CheckedUpdates {
    scope: OpenScope,
    /// The installed mods' fingerprint the check ran against (`native_status::fingerprint`):
    /// a finding holds only while the mods are as they were.
    inputs: String,
    /// The installer's own proof (profile identity, base, active state, exact mod inputs hash
    /// and cached base build). Only offers proved against the exact same state carry over.
    proof: Option<SourceProof>,
    only: Option<String>,
    target: Option<SourceTarget>,
    candidates: BTreeMap<String, (Origin, PreparedSource)>,
    issues: BTreeMap<String, SourceError>,
    notice: Option<String>,
}

#[derive(Default)]
pub(in crate::mods) struct Work {
    pub(super) icons: HashMap<sources::SourceIcon, Option<PathBuf>>,
    icon_misses: sources::IconMisses,
    pub(super) release_pins: HashMap<String, sources::SourceCommit>,
    checking: bool,
    updates: Option<CheckedUpdates>,
    pub(super) log: Option<(String, std::sync::Arc<str>)>,
}

impl Launcher {
    pub(in crate::mods) fn nightly_event(&mut self, event: Event) {
        let mut add = false;
        let state = &mut self.ui.mods.nightly;
        match event {
            Event::Catalogue(catalogue) => {
                state.reading = false;
                if state
                    .catalogue
                    .as_ref()
                    .is_none_or(|old| catalogue.checked_at >= old.checked_at)
                {
                    state.catalogue = Some(catalogue);
                }
            }
            Event::Detail(request, detail) => {
                let Some(open) = state.open.as_mut().filter(|open| open.request == request) else {
                    return;
                };
                if matches!(open.source.tracker(), sources::Tracker::Release) {
                    let pins = &mut state.work.release_pins;
                    if let Some(pin) = &detail.data.commit {
                        pins.insert(open.source.id().to_owned(), pin.clone());
                    } else {
                        pins.remove(open.source.id());
                    }
                }
                open.inspection = Some(*detail);
                add = std::mem::take(&mut open.add_when_read);
            }
            Event::Icon(icon, path) => {
                state.work.icons.insert(icon, path);
            }
            Event::Updates(checked) => {
                state.work.checking = false;
                state.work.updates = Some(merge_checked(state.work.updates.take(), *checked));
            }
        }
        if add {
            self.build_nightly();
        }
    }

    /// Add after a refusal: the author may have fixed the source since, so it is read again
    /// first, and added at the commit that read finds.
    pub(in crate::mods) fn reread_and_add(&mut self, source: Source) {
        self.inspect_nightly(source, true);
        if let Some(open) = &mut self.ui.mods.nightly.open {
            open.add_when_read = true;
        }
    }

    pub(in crate::mods) fn refresh_nightly_scope(&mut self) {
        let state = &self.ui.mods.nightly;
        if state.open.is_none() && state.work.updates.is_none() {
            return;
        }
        let scope = self.mods_page_scope();
        let state = &mut self.ui.mods.nightly;
        if state.open.as_ref().is_some_and(|open| open.scope != scope) {
            let from = state.open.take().map_or(Page::Discover, |open| open.from);
            if self.ui.page == Page::Project {
                self.ui.page = from;
            }
        }
        if state
            .work
            .updates
            .as_ref()
            .is_some_and(|updates| updates.scope != scope)
        {
            state.work.updates = None;
        }
    }

    pub(in crate::mods) fn inspect_nightly(&mut self, source: Source, refresh: bool) {
        let from = self
            .ui
            .mods
            .nightly
            .open
            .as_ref()
            .filter(|_| self.ui.page == Page::Project)
            .map_or(Page::Discover, |open| open.from);
        self.inspect_nightly_from(source, refresh, from);
    }

    pub(in crate::mods) fn inspect_nightly_from(
        &mut self,
        source: Source,
        refresh: bool,
        from: Page,
    ) {
        let scope = self.mods_page_scope();
        self.ui.page = Page::Project;
        if from != Page::Installed {
            self.ui.mods.open = None;
        }
        let state = &mut self.ui.mods.nightly;
        if state.open.as_ref().is_none_or(|open| open.source != source) {
            crate::frame::scrolls("project").set_offset(gpui::point(px(0.), px(0.)));
        }
        state.detail_request += 1;
        let request = state.detail_request;
        state.collection = None;
        state.open = Some(Open {
            source: source.clone(),
            from,
            scope,
            request,
            inspection: None,
            add_when_read: false,
        });
        if refresh {
            self.read_nightly_catalogue(true);
        }
        let tx = self.ui.mods.tx.clone();
        std::thread::spawn(move || {
            let data = sources::details(&source, refresh);
            let _ = tx.send(super::super::Event::Nightly(Event::Detail(
                request,
                Box::new(Inspection { data }),
            )));
        });
    }

    pub(super) fn request_nightly_icon(&mut self, icon: sources::SourceIcon) {
        use std::collections::hash_map::Entry;
        let Entry::Vacant(entry) = self.ui.mods.nightly.work.icons.entry(icon.clone()) else {
            return;
        };
        entry.insert(None);
        let tx = self.ui.mods.tx.clone();
        let misses = self.ui.mods.nightly.work.icon_misses.clone();
        std::thread::spawn(move || {
            let path = icon.load(&misses);
            let _ = tx.send(super::super::Event::Nightly(Event::Icon(icon, path)));
        });
    }

    pub(super) fn nightly_busy(&self) -> bool {
        self.mods_locked().is_some()
            || (!self.mutating()
                && self
                    .ui
                    .mods
                    .snapshot
                    .as_ref()
                    .is_none_or(|snap| !snap.ready()))
    }

    pub(crate) fn source_picks_listed(&self, picks: &[Source]) -> bool {
        self.ui
            .mods
            .nightly
            .catalogue
            .as_ref()
            .is_some_and(|list| picks.iter().all(|source| list.sources.contains(source)))
    }

    pub(super) fn build_nightly(&mut self) {
        if !self.nightly_catalogue_current() || self.nightly_busy() {
            return;
        }
        let scope = self.mods_page_scope();
        let Some(open) = self
            .ui
            .mods
            .nightly
            .open
            .as_ref()
            .filter(|open| open.scope == scope)
        else {
            return;
        };
        if !self
            .discover_installed()
            .is_some_and(|rows| rows.source_presence(&open.source) == SourcePresence::Missing)
        {
            return;
        }
        if !self.source_picks_listed(std::slice::from_ref(&open.source)) {
            self.ui.mods.notice = Some(Err(tr(
                "This source listing changed. Open it again from Discover.",
            )
            .into()));
            return;
        }
        if let Some(pin) = open
            .inspection
            .as_ref()
            .and_then(|inspection| inspection.data.checked_commit())
            .cloned()
        {
            self.add_sources(SourcePicks::One(Box::new(pin)));
        }
    }

    pub(crate) fn nightly_catalogue_current(&self) -> bool {
        self.ui.mods.nightly.catalogue.as_ref().is_some_and(|list| {
            list.saved_at.is_some()
                && list.error.is_none()
                && list.checked_at.elapsed().is_ok_and(|age| age < FRESH)
        })
    }

    pub(in crate::mods) fn check_source_updates(&mut self, only: Option<&Origin>) {
        if self.ui.mods.nightly.work.checking || !self.guard() {
            return;
        }
        let scope = self.mods_page_scope();
        let target = match self.source_target() {
            Ok(target) => target,
            Err(error) => {
                self.ui.mods.notice = Some(Err(error));
                return;
            }
        };
        let Some(rows) = self.discover_installed() else {
            return;
        };
        let Some(inputs) = self.ui.mods.snapshot.as_ref().map(|s| s.key.clone()) else {
            return;
        };
        let origins = tracked_origins(&rows.local, only);
        if origins.is_empty() {
            return;
        }
        let only = only.map(|origin| origin.id.clone());
        // The last findings stay until this check answers (`merge_checked`).
        self.ui.mods.nightly.work.checking = true;
        let refresh_catalogue = !self.nightly_catalogue_current();
        let tx = self.ui.mods.tx.clone();
        std::thread::spawn(move || {
            let proof =
                SourceProof::capture(target.instance(), target.identity(), target.base()).ok();
            let mut checked = CheckedUpdates {
                scope,
                inputs,
                proof,
                only,
                target: None,
                candidates: BTreeMap::new(),
                issues: BTreeMap::new(),
                notice: None,
            };
            let catalogue = sources::catalogue(refresh_catalogue);
            if catalogue.error.is_some() {
                checked.notice = Some(tr("No source updates were checked while offline.").into());
            } else {
                let cancellation = Cancellation::default();
                let mut requested = Vec::new();
                let mut predecessors = Vec::new();
                for origin in origins {
                    match source_for_update(&origin) {
                        Ok(Some(source)) => {
                            requested.push(source);
                            predecessors.push(origin);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            checked.issues.insert(origin.id, SourceError::Github(error));
                        }
                    }
                }
                for (origin, pin) in predecessors
                    .into_iter()
                    .zip(sources::resolve_batch(&requested))
                {
                    let pin = match pin {
                        Ok(pin) if pin.sha() == origin.commit => continue,
                        Ok(pin) => pin,
                        Err(error) => {
                            checked.issues.insert(origin.id, SourceError::Github(error));
                            continue;
                        }
                    };
                    match sunrise::prepare_source(
                        target.instance(),
                        target.identity(),
                        target.base(),
                        SourceRequest::Update {
                            pin,
                            predecessor: Box::new(origin.clone()),
                        },
                        &cancellation,
                    ) {
                        Ok(prepared) => {
                            if checked.proof.as_ref() == Some(prepared.proof()) {
                                checked
                                    .candidates
                                    .insert(origin.id.clone(), (origin, prepared));
                            } else {
                                checked.issues.insert(
                                    origin.id,
                                    SourceError::Changed(
                                        tr("The profile or game build changed. Check the source again; nothing was changed.")
                                            .into(),
                                    ),
                                );
                            }
                        }
                        Err(error) => {
                            checked.issues.insert(origin.id, error);
                        }
                    }
                }
                checked.notice = Some(
                    if checked.candidates.is_empty() && checked.issues.is_empty() {
                        tr("No source updates available.").into()
                    } else {
                        tr("Finished checking for source updates.").into()
                    },
                );
                checked.target = Some(target);
            }
            for (id, error) in &checked.issues {
                sunrise_launcher::elog!("Nightly source update preflight ({id}): {error}");
            }
            let _ = tx.send(super::super::Event::Nightly(Event::Catalogue(catalogue)));
            let _ = tx.send(super::super::Event::Nightly(Event::Updates(Box::new(
                checked,
            ))));
        });
    }

    pub(in crate::mods) fn source_updates_checking(&self) -> bool {
        self.ui.mods.nightly.work.checking
    }

    pub(in crate::mods) fn source_update_version(&self, origin: &Origin) -> Option<&str> {
        let (predecessor, prepared) = self.source_updates_here()?.candidates.get(&origin.id)?;
        (predecessor == origin).then_some(prepared.origin().version.as_str())
    }

    pub(in crate::mods) fn source_update_issue(&self, id: &str) -> Option<&SourceError> {
        self.source_updates_here()?.issues.get(id)
    }

    pub(in crate::mods) fn source_update_notice(&self, only: Option<&str>) -> Option<&str> {
        // A check under way says so itself; the last one's outcome would contradict it.
        if self.ui.mods.nightly.work.checking {
            return None;
        }
        let checked = self.source_updates_here()?;
        if checked.only.as_deref() == only {
            if let Some(notice) = checked.notice.as_deref() {
                return Some(notice);
            }
        }
        // Last-known offers stay visible while the catalogue is stale; say so beside them.
        if !self.nightly_catalogue_current()
            && only.map_or(!checked.candidates.is_empty(), |id| {
                checked.candidates.contains_key(id)
            })
        {
            return Some(tr("No source updates were checked while offline."));
        }
        None
    }

    /// The last findings, while the installed mods are still the ones they were proved against.
    fn source_updates_here(&self) -> Option<&CheckedUpdates> {
        let now = &self.ui.mods.snapshot.as_ref()?.key;
        self.ui
            .mods
            .nightly
            .work
            .updates
            .as_ref()
            .filter(|checked| checked.inputs == *now)
    }

    pub(in crate::mods) fn submit_source_update(&mut self, id: &str) {
        if !self.guard() {
            return;
        }
        if let Some((target, sources)) = self.take_source_updates(Some(id)) {
            if let Err(error) = self.queue_sources(target, sources) {
                self.ui.mods.notice = Some(Err(error));
            }
        }
    }

    pub(in crate::mods) fn submit_mod_update_all(
        &mut self,
        displayed_names: BTreeMap<String, String>,
    ) {
        if !self.guard() {
            return;
        }
        let registry: Vec<_> = self
            .rows()
            .into_iter()
            .filter(model::bulk_updatable)
            .map(|row| row.id)
            .collect();
        // Source mods and registry mods update as two jobs, sources first.
        if let Some((target, sources)) = self.take_source_updates(None) {
            self.ui.mods.notice = None;
            self.mods.message = None;
            if let Err(error) = self.queue_sources(target, sources) {
                self.ui.mods.notice = Some(Err(error));
                return;
            }
        }
        self.submit_registry_change(
            crate::mods_page::install::Kind::UpdateAllV2,
            "update-all",
            registry,
            displayed_names,
        );
    }

    fn take_source_updates(
        &mut self,
        id: Option<&str>,
    ) -> Option<(SourceTarget, Vec<PreparedSource>)> {
        if !self.nightly_catalogue_current() {
            return None;
        }
        let scope = self.mods_page_scope();
        let installed = self
            .ui
            .mods
            .snapshot
            .as_ref()
            .into_iter()
            .flat_map(|snapshot| &snapshot.rows.local)
            .filter_map(|local| local.github.as_ref())
            .filter(|origin| !origin.is_manual());
        let checked = self
            .ui
            .mods
            .nightly
            .work
            .updates
            .as_mut()
            .filter(|updates| updates.scope == scope)
            .filter(|updates| {
                self.ui.mods.snapshot.as_ref().map(|s| &s.key) == Some(&updates.inputs)
            })?;
        let target = checked.target.clone()?;
        let wanted: Vec<_> = installed
            .filter(|origin| id.is_none_or(|id| id == origin.id))
            .filter(|origin| {
                checked
                    .candidates
                    .get(&origin.id)
                    .is_some_and(|(predecessor, _)| predecessor == *origin)
            })
            .map(|origin| origin.id.clone())
            .collect();
        let inputs: Vec<_> = wanted
            .into_iter()
            .filter_map(|id| checked.candidates.remove(&id))
            .map(|(_, prepared)| prepared)
            .collect();
        (!inputs.is_empty()).then_some((target, inputs))
    }
}

/// What a check found, laid over the last one. Only findings proved against the exact same
/// profile identity, active state, mod inputs and cached base build carry over. A profile-wide
/// check replaces them; one project's check replaces only that project's offer or issue. A check
/// that couldn't run (offline) keeps them and only says so.
fn merge_checked(previous: Option<CheckedUpdates>, checked: CheckedUpdates) -> CheckedUpdates {
    let same_target = |earlier: &Option<SourceTarget>| match (earlier, &checked.target) {
        (Some(earlier), Some(now)) => {
            earlier.identity() == now.identity() && earlier.base() == now.base()
        }
        (_, None) => true,
        (None, Some(_)) => false,
    };
    let Some(now_proof) = checked.proof.as_ref() else {
        return checked;
    };
    let Some(mut merged) = previous.filter(|earlier| {
        earlier.scope == checked.scope
            && earlier.inputs == checked.inputs
            && earlier.proof.as_ref() == Some(now_proof)
            && same_target(&earlier.target)
    }) else {
        return checked;
    };
    merged
        .candidates
        .retain(|_, (_, prepared)| prepared.proof() == now_proof);
    if checked.target.is_some() {
        let Some(id) = checked.only.clone() else {
            return checked;
        };
        merged.candidates.remove(&id);
        merged.issues.remove(&id);
        merged.target = checked.target;
        merged.proof = checked.proof;
        merged.candidates.extend(checked.candidates);
        merged.issues.extend(checked.issues);
    }
    merged.only = checked.only;
    merged.notice = checked.notice;
    merged
}

fn tracked_origins(local: &[model::Local], only: Option<&Origin>) -> Vec<Origin> {
    local
        .iter()
        .filter_map(|local| local.github.as_ref())
        .filter(|origin| !origin.is_manual())
        .filter(|origin| only.is_none_or(|only| only == *origin))
        .filter(|origin| {
            origin
                .source
                .as_ref()
                .is_some_and(|source| !matches!(source.track, SourceTrack::Pinned))
        })
        .cloned()
        .collect()
}

fn source_for_update(predecessor: &Origin) -> Result<Option<Source>, sources::Failure> {
    let Some(source) = predecessor.source.as_ref() else {
        return Ok(None);
    };
    let tracker = match &source.track {
        SourceTrack::Branch(branch) => sources::Tracker::Branch(branch.clone()),
        SourceTrack::Release => sources::Tracker::Release,
        SourceTrack::Pinned => return Ok(None),
    };
    Source::for_origin(
        &predecessor.id,
        &format!("{}/{}", predecessor.owner, predecessor.repo),
        source.repo_id,
        &source.manifest,
        tracker,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    #![cfg_attr(not(debug_assertions), allow(dead_code, unused_imports))]
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::SystemTime;
    use sunrise_launcher::browser::mod_list;
    use sunrise_launcher::github_mods::{self, AddedFrom, SourceOrigin};

    const CLIENT_BIN: &str = if cfg!(windows) {
        "sunrise-client.exe"
    } else {
        "sunrise-client"
    };

    fn run_in_scratch(test_name: &str, body: impl FnOnce(&Path)) {
        const CHILD: &str = "SUNRISE_NIGHTLY_OFFER_TEST";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let nowhere = "http://127.0.0.1:9";
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name])
                .env(CHILD, "1")
                .env("SUNRISE_LAUNCHER_HOME", root.path().join("launcher"))
                .env("SUNRISE_HOME", root.path().join("home"))
                .env(
                    "SUNRISE_LAUNCHER_LOCATION_FILE",
                    root.path().join("location.json"),
                )
                .env("SUNRISE_NO_KEYCHAIN", "1")
                .env("SUNRISE_TELEMETRY_URL", "off")
                .env("SUNRISE_LAUNCHER_NO_SELF_UPDATE", "1")
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("HTTPS_PROXY", nowhere)
                .env("HTTP_PROXY", nowhere)
                .env("ALL_PROXY", nowhere)
                .env_remove("NO_PROXY")
                .env_remove("SUNRISE_GITHUB_MIRROR")
                .env("SUNRISE_SOURCE_FIXTURES", root.path().join("github"))
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        body(&backend::game_home());
    }

    fn setup_prepared_offer(
        home: &Path,
        base: &str,
    ) -> (SourceTarget, Origin, PreparedSource, PathBuf, PathBuf) {
        let base_dir = backend::version_dir(base);
        fs::create_dir_all(&base_dir).unwrap();
        fs::write(base_dir.join(".installed"), b"").unwrap();
        let client_path = base_dir.join(CLIENT_BIN);
        fs::write(&client_path, b"base-x64-binary").unwrap();
        let pin_rev = "a".repeat(40);
        fs::write(
            base_dir.join("developer.json"),
            format!(r#"{{"revision":"{pin_rev}"}}"#),
        )
        .unwrap();
        fs::write(
            base_dir.join("research-lock.json"),
            format!(
                r#"{{"minecraft":{{"version":"{}"}}}}"#,
                backend::PINNED_GAME
            ),
        )
        .unwrap();

        fs::create_dir_all(home.join("mods")).unwrap();
        let target = SourceTarget::fixture(home, base);

        let fixtures = PathBuf::from(std::env::var_os("SUNRISE_SOURCE_FIXTURES").unwrap());
        let repo = fixtures.join("repos/Fixture/OfferMod");
        fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&repo)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q", "--initial-branch=main", "--template="]);
        let write_mod = |version: &str| {
            let manifest = format!(
                "id = 'offer_mod'\nname = 'Offer Mod'\nversion = '{version}'\napi = '0.2'\nside = 'both'\ncapabilities = ['registries']\n[native]\nsunrise = '{pin_rev}'\nmixins = '{}'\ngame = '{}'\n",
                "c".repeat(40),
                backend::PINNED_GAME,
            );
            fs::create_dir_all(repo.join("src")).unwrap();
            fs::write(repo.join("sunrise-mod.toml"), &manifest).unwrap();
            fs::write(
                repo.join("Cargo.toml"),
                format!("[package]\nname = 'offer_mod'\nversion = '{version}'\n"),
            )
            .unwrap();
            fs::write(repo.join("src/lib.rs"), b"pub fn fixture() {}\n").unwrap();
            manifest
        };
        let manifest1 = write_mod("1.0.0");
        git(&["add", "."]);
        git(&["commit", "-qm", "v1"]);
        let sha1 = git(&["rev-parse", "HEAD"]);

        let api = fixtures.join("api/repos/Fixture");
        let heads = api.join("OfferMod/git/ref/heads");
        let commits = api.join("OfferMod/git/commits");
        fs::create_dir_all(&heads).unwrap();
        fs::create_dir_all(&commits).unwrap();
        fs::write(
            api.join("OfferMod.json"),
            r#"{"id":77,"full_name":"Fixture/OfferMod","default_branch":"main","private":false}"#,
        )
        .unwrap();
        let write_ref = |sha: &str, manifest: &str| {
            let raw = fixtures.join("raw/Fixture/OfferMod").join(sha);
            fs::create_dir_all(&raw).unwrap();
            fs::write(raw.join("sunrise-mod.toml"), manifest).unwrap();
            fs::write(
                heads.join("main.json"),
                format!(
                    r#"{{"ref":"refs/heads/main","object":{{"type":"commit","sha":"{sha}"}}}}"#
                ),
            )
            .unwrap();
            fs::write(
                commits.join(format!("{sha}.json")),
                format!(r#"{{"sha":"{sha}"}}"#),
            )
            .unwrap();
        };
        write_ref(&sha1, &manifest1);

        // Install v1.0.0 through the real source installer so receipt, archive and mod list match.
        let source = Source::for_origin(
            "offer_mod",
            "Fixture/OfferMod",
            77,
            "sunrise-mod.toml",
            sources::Tracker::Branch("main".into()),
        )
        .unwrap();
        let pin1 = sources::resolve(&source, true).unwrap();
        let cancel = Cancellation::default();
        let packed = sunrise::pack_sources(&[pin1], AddedFrom::Discover, &cancel)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        sunrise::add_sources(
            target.instance(),
            target.identity(),
            target.base(),
            packed,
            sunrise::SourceHooks {
                install: sunrise::InstallHooks {
                    progress: &|_| {},
                    continue_after_failure: &|_| false,
                    cancellation: Some(&cancel),
                },
                cancellation: &cancel,
                authorize: &|_, _| Ok(true),
                approve_worlds: &|_| true,
            },
        )
        .unwrap();
        let origin = github_mods::origin(home, "offer_mod").unwrap();
        assert_eq!(origin.version, "1.0.0");
        assert!(matches!(
            origin.source.as_ref().map(|s: &SourceOrigin| &s.track),
            Some(SourceTrack::Branch(b)) if b == "main"
        ));

        // Add an unpacked content mod whose internal file edits don't change native_status::fingerprint.
        let unpacked = home.join("sunrise-mods/unpacked_content");
        fs::create_dir_all(unpacked.join("assets")).unwrap();
        fs::write(
            unpacked.join("sunrise-mod.toml"),
            "id = 'unpacked_content'\nname = 'Unpacked Content'\nversion = '1.0.0'\napi = '0.2'\nside = 'both'\ncapabilities = ['registries']\n",
        )
        .unwrap();
        let unpacked_data = unpacked.join("assets/data.txt");
        fs::write(&unpacked_data, b"before").unwrap();
        mod_list::set_enabled(home, "unpacked_content", true).unwrap();

        // Advance the fixture repo to v1.1.0 and prepare a real update offer.
        let manifest2 = write_mod("1.1.0");
        git(&["add", "."]);
        git(&["commit", "-qm", "v2"]);
        let sha2 = git(&["rev-parse", "HEAD"]);
        write_ref(&sha2, &manifest2);

        let pin2 = sources::resolve(&source, true).unwrap();
        let prepared = sunrise::prepare_source(
            target.instance(),
            target.identity(),
            target.base(),
            SourceRequest::Update {
                pin: pin2,
                predecessor: Box::new(origin.clone()),
            },
            &cancel,
        )
        .unwrap();
        (target, origin, prepared, unpacked_data, client_path)
    }

    #[cfg(debug_assertions)] // source fixtures are off in release builds
    #[test]
    fn a_check_keeps_only_findings_proved_against_the_same_mods() {
        run_in_scratch(
            "mods::nightly::jobs::tests::a_check_keeps_only_findings_proved_against_the_same_mods",
            |home| {
                const BASE: &str = "developer-local";
                let (target, origin, prepared, unpacked_data, client_path) =
                    setup_prepared_offer(home, BASE);
                let proof1 =
                    SourceProof::capture(target.instance(), target.identity(), BASE).unwrap();
                assert_eq!(prepared.proof(), &proof1);
                let fingerprint1 = native_status::fingerprint(home, Some(BASE));

                let scope = || OpenScope {
                    profile: Some("p".into()),
                    home: home.to_path_buf(),
                    target: None,
                    game: BASE.into(),
                };
                let issue = |why: &str| SourceError::Unproved(why.into());
                let make_checked = |only: Option<&str>,
                                    inputs: &str,
                                    proof: Option<SourceProof>,
                                    online: bool,
                                    issues: &[(&str, &str)]| {
                    CheckedUpdates {
                        scope: scope(),
                        inputs: inputs.into(),
                        proof,
                        only: only.map(str::to_owned),
                        target: online.then(|| target.clone()),
                        candidates: BTreeMap::new(),
                        issues: issues
                            .iter()
                            .map(|(id, why)| ((*id).to_owned(), issue(why)))
                            .collect(),
                        notice: (!online)
                            .then(|| tr("No source updates were checked while offline.").into()),
                    }
                };
                let with_offer = |prepared: PreparedSource| {
                    let mut c = make_checked(
                        None,
                        &fingerprint1,
                        Some(proof1.clone()),
                        true,
                        &[("a", "old a"), ("b", "b")],
                    );
                    c.candidates
                        .insert(origin.id.clone(), (origin.clone(), prepared));
                    c
                };

                // Re-prepare helper since PreparedSource isn't Clone.
                let re_prepare = || {
                    let source = source_for_update(&origin).unwrap().unwrap();
                    let pin = sources::resolve(&source, true).unwrap();
                    sunrise::prepare_source(
                        target.instance(),
                        target.identity(),
                        BASE,
                        SourceRequest::Update {
                            pin,
                            predecessor: Box::new(origin.clone()),
                        },
                        &Cancellation::default(),
                    )
                    .unwrap()
                };

                // 1. An unchanged single-project check keeps the other mod's real offer and issue.
                let merged = merge_checked(
                    Some(with_offer(prepared)),
                    make_checked(Some("a"), &fingerprint1, Some(proof1.clone()), true, &[]),
                );
                assert!(merged.candidates.contains_key(&origin.id));
                assert_eq!(merged.issues.get("b"), Some(&issue("b")));
                assert_eq!(merged.issues.get("a"), None);

                // 2. Offline, nothing was checked: the real offer survives both offline paths.
                let mut carried = merged;
                for only in [None, Some(origin.id.as_str())] {
                    carried = merge_checked(
                        Some(carried),
                        make_checked(only, &fingerprint1, Some(proof1.clone()), false, &[]),
                    );
                    assert!(carried.candidates.contains_key(&origin.id));
                    assert_eq!(
                        carried.notice.as_deref(),
                        Some(tr("No source updates were checked while offline."))
                    );
                }

                // 3. A successful check for that project that finds no update drops the offer.
                let merged = merge_checked(
                    Some(carried),
                    make_checked(
                        Some(&origin.id),
                        &fingerprint1,
                        Some(proof1.clone()),
                        true,
                        &[],
                    ),
                );
                assert!(!merged.candidates.contains_key(&origin.id));

                // 4. A profile-wide online check that finds no update drops the offer.
                let merged = merge_checked(
                    Some(with_offer(re_prepare())),
                    make_checked(None, &fingerprint1, Some(proof1.clone()), true, &[]),
                );
                assert!(merged.candidates.is_empty());
                assert!(merged.issues.is_empty());

                // 5. Editing a file inside an unpacked content mod doesn't change the Installed
                //    fingerprint, but changes the installer's proof and drops the stale offer.
                let before_edit = with_offer(re_prepare());
                fs::write(&unpacked_data, b"edited-content").unwrap();
                assert_eq!(native_status::fingerprint(home, Some(BASE)), fingerprint1);
                let proof_after_edit =
                    SourceProof::capture(target.instance(), target.identity(), BASE).unwrap();
                assert_ne!(proof_after_edit, proof1);
                let merged = merge_checked(
                    Some(before_edit),
                    make_checked(Some("a"), &fingerprint1, Some(proof_after_edit), true, &[]),
                );
                assert!(merged.candidates.is_empty());
                assert!(merged.issues.is_empty());

                // 6. Reinstalling the same game tag for another architecture doesn't change the
                //    Installed fingerprint, but changes the cached base build and drops the offer.
                fs::write(&unpacked_data, b"before").unwrap();
                let before_reinstall = with_offer(re_prepare());
                fs::write(&client_path, b"base-arm64-binary").unwrap();
                assert_eq!(native_status::fingerprint(home, Some(BASE)), fingerprint1);
                let proof_after_reinstall =
                    SourceProof::capture(target.instance(), target.identity(), BASE).unwrap();
                assert_ne!(proof_after_reinstall, proof1);
                let merged = merge_checked(
                    Some(before_reinstall),
                    make_checked(
                        Some("a"),
                        &fingerprint1,
                        Some(proof_after_reinstall),
                        true,
                        &[],
                    ),
                );
                assert!(merged.candidates.is_empty());
                assert!(merged.issues.is_empty());
            },
        );
    }

    #[cfg(debug_assertions)] // source fixtures are off in release builds
    #[test]
    fn last_known_source_offer_stays_visible_while_offline_but_requires_fresh_catalogue_to_apply() {
        run_in_scratch(
            "mods::nightly::jobs::tests::last_known_source_offer_stays_visible_while_offline_but_requires_fresh_catalogue_to_apply",
            |home| {
                const BASE: &str = "developer-local";
                let (target, origin, prepared, _, _) = setup_prepared_offer(home, BASE);
                let proof =
                    SourceProof::capture(target.instance(), target.identity(), BASE).unwrap();
                let key = native_status::fingerprint(home, Some(BASE));

                let mut app = gpui::TestAppContext::single();
                let (shell, cx) = app.add_window_view(crate::window_root::Shell::new_for_test);
                let launcher = shell.read_with(cx, |shell, _| shell.launcher());
                launcher.update(cx, |launcher, _| {
                    let scope = launcher.mods_page_scope();
                    let local = model::Local {
                        id: origin.id.clone(),
                        version: origin.version.clone(),
                        name: "Offer Mod".into(),
                        file: "mods/offer_mod.smod".into(),
                        github: Some(origin.clone()),
                        native: false,
                        served_by: None,
                        unreadable: false,
                    };
                    let mut rows = InstalledRows::default();
                    rows.local.push(local);
                    launcher.ui.mods.snapshot = Some(Snapshot {
                        home: home.to_path_buf(),
                        key: key.clone(),
                        rows,
                        reading: false,
                        error: None,
                        backup_notice: None,
                    });
                    let mut candidates = BTreeMap::new();
                    candidates.insert(origin.id.clone(), (origin.clone(), prepared));
                    launcher.ui.mods.nightly.work.updates = Some(CheckedUpdates {
                        scope,
                        inputs: key.clone(),
                        proof: Some(proof),
                        only: None,
                        target: Some(target.clone()),
                        candidates,
                        issues: BTreeMap::new(),
                        notice: Some(
                            tr("No source updates were checked while offline.").into(),
                        ),
                    });
                    // Catalogue failed / offline: nightly_catalogue_current() is false.
                    launcher.ui.mods.nightly.catalogue = Some(Catalogue {
                        sources: Vec::new().into(),
                        collections: Vec::new().into(),
                        saved_at: None,
                        checked_at: SystemTime::now(),
                        error: Some("offline".into()),
                    });
                    assert!(!launcher.nightly_catalogue_current());

                    // Last-known offer is still visible beside the offline notice.
                    assert_eq!(launcher.source_update_version(&origin), Some("1.1.0"));
                    assert_eq!(
                        launcher.source_update_notice(None),
                        Some(tr("No source updates were checked while offline."))
                    );
                    // Applying still requires a fresh catalogue.
                    assert!(launcher.take_source_updates(Some(&origin.id)).is_none());
                    assert!(launcher.take_source_updates(None).is_none());

                    // Once the catalogue is current again, applying takes the prepared offer.
                    launcher.ui.mods.nightly.catalogue = Some(Catalogue {
                        sources: Vec::new().into(),
                        collections: Vec::new().into(),
                        saved_at: Some(SystemTime::now()),
                        checked_at: SystemTime::now(),
                        error: None,
                    });
                    assert!(launcher.nightly_catalogue_current());
                    let taken = launcher.take_source_updates(Some(&origin.id));
                    assert!(taken.is_some());
                    assert_eq!(taken.unwrap().1.len(), 1);
                });
            },
        );
    }

    #[test]
    fn project_check_uses_only_its_exact_current_receipt() {
        let (_, first) = super::super::tests::receipt_fixture();
        let mut second = first.clone();
        second.id = "other".into();
        second.github.as_mut().unwrap().id = second.id.clone();
        let mut pinned = first.clone();
        pinned
            .github
            .as_mut()
            .unwrap()
            .source
            .as_mut()
            .unwrap()
            .track = SourceTrack::Pinned;
        let only = first.github.clone().unwrap();
        let other = second.github.clone().unwrap();
        let locals = [first, second, pinned];
        assert_eq!(tracked_origins(&locals, None), [only.clone(), other]);
        assert_eq!(
            tracked_origins(&locals, Some(&only)),
            std::slice::from_ref(&only)
        );
        let stale = Origin {
            commit: "9".repeat(40),
            ..only
        };
        assert!(tracked_origins(&locals, Some(&stale)).is_empty());
    }

    #[test]
    fn manual_from_source_mods_are_excluded_from_tracked_source_updates() {
        use sunrise_launcher::github_mods::AddedFrom;
        let (_, mut manual_github) = super::super::tests::receipt_fixture();
        manual_github.github.as_mut().unwrap().added_from = Some(AddedFrom::Github);

        let mut manual_folder = manual_github.clone();
        manual_folder.id = "folder_mod".into();
        manual_folder.github = Some(Origin::local_folder(
            "folder_mod".into(),
            "0.4.0".into(),
            "/tmp/camp-lanterns".into(),
            "0".repeat(64),
        ));

        let mut discover_source = manual_github.clone();
        discover_source.id = "discover_mod".into();
        discover_source.github.as_mut().unwrap().id = "discover_mod".into();
        discover_source.github.as_mut().unwrap().added_from = Some(AddedFrom::Discover);

        let locals = [manual_github, manual_folder, discover_source.clone()];
        let tracked = tracked_origins(&locals, None);
        assert_eq!(tracked, [discover_source.github.unwrap()]);
    }

    #[test]
    fn legacy_pasted_branch_and_release_receipts_stay_out_of_update_all() {
        use sunrise_launcher::github_mods::AddedFrom;
        let (_, discover_source) = super::super::tests::receipt_fixture();

        let mut legacy_branch = discover_source.clone();
        legacy_branch.id = "legacy_branch".into();
        legacy_branch.github.as_mut().unwrap().id = "legacy_branch".into();
        legacy_branch.github.as_mut().unwrap().added_from = None;

        let mut legacy_release = discover_source.clone();
        legacy_release.id = "legacy_release".into();
        legacy_release.github.as_mut().unwrap().id = "legacy_release".into();
        legacy_release.github.as_mut().unwrap().added_from = None;
        legacy_release
            .github
            .as_mut()
            .unwrap()
            .source
            .as_mut()
            .unwrap()
            .track = SourceTrack::Release;

        let mut collection_source = discover_source.clone();
        collection_source.id = "collection_mod".into();
        collection_source.github.as_mut().unwrap().id = "collection_mod".into();
        collection_source.github.as_mut().unwrap().added_from = Some(AddedFrom::Collection {
            id: "starter".into(),
            name: "Starter".into(),
        });

        let locals = [
            legacy_branch,
            legacy_release,
            discover_source.clone(),
            collection_source.clone(),
        ];
        let tracked = tracked_origins(&locals, None);
        assert_eq!(
            tracked,
            [
                discover_source.github.unwrap(),
                collection_source.github.unwrap(),
            ]
        );
    }
}
