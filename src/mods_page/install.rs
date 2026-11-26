//! One visible registry install batch, and requests that arrived after it was sealed.
use super::*;
use crate::github_mods_page::{Feedback, GithubInput, GithubSelection, LocalInput};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
};
use sunrise_launcher::{
    browser::sunrise::{
        BuildNotice, ChangeImpact, InstallError, InstallHooks, InstallProgress, InstalledBatch,
        PreparedSource, ProviderChoiceNeeded, PublisherChangeQuestion, SourceDependency,
        SourceError, SourceHooks, Target, V2Hooks, V2InstallOptions,
    },
    github_mods::{AddedFrom, Origin},
    managed_play::FirstBuild,
    native_mods::Cancellation,
    profile_txn::Identity,
};

use sunrise_launcher::browser::sunrise::sources::{self, Source, SourceCommit};

mod batch;
mod cards;
mod requests;
mod source_queue;
use cards::{failure_line, notice_failure};
pub(crate) use cards::{stage_fraction, stage_label};

/// Whether installs have a player to install for: an account or an offline name is
/// who plays (`Accounts::player`), or the developer's offline mode is on — the same
/// test Play uses (`launch_state`'s `play_has_player`), so who can play can also
/// install the mods they play with. The game's own made-up name is nobody the
/// launcher chose, so with nothing selected installs still ask for a sign-in.
/// Nothing behind the install path checks a credential: the registry index and the
/// packages are public downloads, so this test is the whole gate.
pub(crate) fn install_has_player(accounts: &Accounts, developer_offline: bool) -> bool {
    !matches!(accounts.player(), account::Player::GameName) || developer_offline
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SourcePicks {
    One(Box<SourceCommit>),
    Collection {
        id: String,
        name: String,
        sources: Vec<Source>,
    },
}

impl SourcePicks {
    pub(crate) fn listings(&self) -> &[Source] {
        match self {
            Self::One(pin) => std::slice::from_ref(pin.source()),
            Self::Collection { sources, .. } => sources,
        }
    }

    pub(super) fn resolve(
        &self,
    ) -> impl Iterator<Item = Result<SourceCommit, sources::Failure>> + '_ {
        let (one, collection) = match self {
            Self::One(pin) => (Some(Ok((**pin).clone())), &[][..]),
            Self::Collection { sources, .. } => (None, sources.as_slice()),
        };
        one.into_iter().chain(sources::resolve_each(collection))
    }
}

#[derive(Default)]
pub(crate) struct Installs {
    running: Option<Running>,
    waiting: VecDeque<Request>,
    pub(crate) question: Option<(BuildNotice, Sender<bool>)>,
    pub(crate) world: Option<(WorldQuestion, Sender<bool>)>,
    source_question: Option<(SourceQuestion, Sender<Result<bool, String>>)>,
    source_outcomes: BTreeMap<(Identity, String), Result<(), SourceError>>,
    outcomes: HashMap<(PathBuf, String), Outcome>,
    play: Option<(PathBuf, launcher_play::PlayIntent)>,
}

struct Running {
    profile: instances::Instance,
    identity: Identity,
    target: Option<String>,
    requests: Vec<RequestView>,
    stage: InstallProgress,
    eta: crate::eta::StageEta,
    cancellation: Cancellation,
    github: Option<Arc<GithubInput>>,
    local: Option<Arc<LocalInput>>,
    source: Option<RunningSource>,
}

struct Request {
    profile: instances::Instance,
    identity: Identity,
    profile_target: Target,
    target: Option<String>,
    release: Option<backend::Release>,
    displayed_names: BTreeMap<String, String>,
    view: RequestView,
    github: Option<Arc<GithubInput>>,
    local: Option<Arc<LocalInput>>,
    sources: Vec<PreparedSource>,
    picks: Option<SourcePicks>,
}

impl Request {
    fn source_inputs(&self) -> &[PreparedSource] {
        match self.github.as_deref().map(|input| &input.selection) {
            Some(GithubSelection::Source(sources)) => sources,
            _ => &self.sources,
        }
    }
}

/// Bound before the link can start a network lookup, not when its answer comes back.
#[derive(Clone)]
pub(crate) struct GithubTarget {
    profile: instances::Instance,
    identity: Identity,
    profile_target: Target,
    target: Option<String>,
    release: Option<backend::Release>,
}

impl GithubTarget {
    pub(crate) fn same_destination(&self, current: &Self) -> bool {
        self.identity == current.identity
            && self.profile.home == current.profile.home
            && self.profile.id == current.profile.id
            && self.profile.incarnation == current.profile.incarnation
            && self.profile.native_profile == current.profile.native_profile
            && self.target == current.target
            && match (&self.release, &current.release) {
                (Some(a), Some(b)) => a.nightly == b.nightly && a.asset == b.asset,
                _ => true, // A successful exact cache-only download can satisfy the retry.
            }
    }

    pub(crate) fn profile_name(&self) -> &str {
        &self.profile.name
    }

    pub(crate) fn source(&self) -> Result<SourceTarget, String> {
        Ok(SourceTarget {
            profile: self.profile.clone(),
            identity: self.identity,
            profile_target: self.profile_target.clone(),
            base: self
                .target
                .clone()
                .ok_or_else(backend::no_version_installed)?,
        })
    }
}

#[cfg(test)]
impl GithubTarget {
    pub(crate) fn fixture(home: &Path) -> Self {
        let request = tests::request(home, "fixture", "test-base");
        Self {
            profile: request.profile,
            identity: request.identity,
            profile_target: request.profile_target,
            target: request.target,
            release: None,
        }
    }
}

/// The exact installed base and profile that a read-only source fit check covers.
#[derive(Clone)]
pub(crate) struct SourceTarget {
    profile: instances::Instance,
    identity: Identity,
    profile_target: Target,
    base: String,
}

#[cfg(test)]
impl SourceTarget {
    /// The profile in `home` (an existing folder) on build `base`.
    pub(crate) fn fixture(home: &Path, base: &str) -> Self {
        let request = tests::request(home, "fixture", base);
        Self {
            profile: request.profile,
            identity: request.identity,
            profile_target: request.profile_target,
            base: base.into(),
        }
    }
}

impl SourceTarget {
    pub(crate) fn profile_target(&self) -> &Target {
        &self.profile_target
    }
    pub(crate) fn identity(&self) -> Identity {
        self.identity
    }
    pub(crate) fn base(&self) -> &str {
        &self.base
    }
}

pub(crate) struct SourceQuestion {
    pub(crate) base: String,
    pub(crate) mods: Vec<(String, Origin)>,
    pub(crate) dependencies: Vec<SourceDependency>,
}

impl SourceQuestion {
    pub(crate) fn sources(&self) -> impl Iterator<Item = (&str, &Origin)> {
        self.mods
            .iter()
            .map(|(name, origin)| (name.as_str(), origin))
            .chain(
                self.dependencies
                    .iter()
                    .filter_map(|dep| Some((dep.name.as_str(), dep.origin.as_ref()?))),
            )
    }
}

pub(crate) struct SourceActivity<'a> {
    pub(crate) ids: &'a [String],
    pub(crate) names: &'a [String],
    pub(crate) progress: InstallProgress,
    pub(crate) stopping: bool,
}

struct RunningSource {
    ids: Vec<String>,
    names: Vec<String>,
    /// Each mod asked for: its id, the name people know, and its `owner/repo`.
    members: Vec<(String, String, String)>,
    /// It updates installed mods rather than adding new ones.
    update: bool,
    cancellation: Cancellation,
    rejected: BTreeMap<String, SourceError>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Projects from the signed v2 index, by project ID.
    InstallV2,
    Github,
    Local,
    Source,
    /// Update mods the signed v2 index installed: one asked for, or every one with an update.
    UpdateV2,
    UpdateAllV2,
    Remove,
}

#[derive(Clone)]
struct RequestView {
    kind: Kind,
    card: String,
    ids: Vec<String>,
}

pub(crate) struct WorldQuestion {
    pub(crate) removal: bool,
    pub(crate) cancel_default: bool,
    pub(crate) heading: String,
    pub(crate) detail: String,
}

impl WorldQuestion {
    pub(crate) fn new(impact: &ChangeImpact, displayed_names: &BTreeMap<String, String>) -> Self {
        let (heading, detail) = impact.gui_review(displayed_names);
        Self {
            removal: impact.is_removal(),
            cancel_default: impact.is_removal() || impact.risks_worlds(),
            heading,
            detail,
        }
    }
}

pub(crate) enum Worker {
    SourceRejected(String, SourceError),
    Progress(InstallProgress),
    Source(SourceQuestion, Sender<Result<bool, String>>),
    Decide(BuildNotice, Sender<bool>),
    World(WorldQuestion, Sender<bool>),
    Done(Result<InstalledBatch, InstallError>),
}

pub(crate) enum Card<'a> {
    Progress(InstallProgress),
    Waiting,
    Done(&'a Outcome),
}

#[derive(Clone, Default)]
pub(crate) struct Outcome {
    pub(crate) added: Vec<String>,
    pub(crate) updated: Vec<(String, String, String)>,
    pub(crate) failure: Option<(String, String)>,
}

impl Outcome {
    /// The sentence under a card. A failure says why on the line under it, so a card explains
    /// itself without opening the mod's page.
    fn note(self) -> Option<String> {
        if self.failure.is_some() {
            return None;
        }
        if !self.added.is_empty() {
            return Some(trf(
                "Also installed: {mods}",
                &[("mods", &crate::mods::names(&self.added))],
            ));
        }
        self.updated.first().map(|(name, from, to)| {
            trf(
                "Also updated: {mod} {from} → {to}",
                &[("mod", name), ("from", from), ("to", to)],
            )
        })
    }
}

#[cfg(test)]
mod tests;
