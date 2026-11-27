//! The Skins tab: how the account that plays looks. A 3D preview of the player (drag to turn
//! it) beside the choices: the skin (the one worn now, the saved ones, the game's default
//! skins, or a new PNG), wide or slim arms, and the cape (none, or one of the Minecraft capes
//! the account owns). Choices show on the preview at once; Save sends them to the Minecraft
//! services, which every client and server then shows.
//!
//! The skin, its arms and the cape are separate edits: one not touched is never sent, and a
//! cape the page doesn't know yet (the profile still being read) is never taken for "no cape".
//! A skin chosen is the picture clicked ([`SkinPick`]): the one an account wore at a URL, a
//! saved skin, a default skin; never "whatever is worn" resolved later. What the account wears
//! comes from one reading of its profile, made each time the page opens, so a change made in
//! another launcher shows here.
//!
//! Each account has its own page ([`SkinsPages`]): switching to another account and back finds
//! the first one as it was left, a Save still under way included, so an account never has two
//! Saves at once, and a Save that ends while another account is shown is reconciled with its
//! own account's page. Choices made while a Save runs are kept as they are, and compared with
//! what the account wears only once the Save has said what that is.
//!
//! Every request runs off the UI thread and comes back as a message (`Msg::SkinsProfile`,
//! `Picture`, `LookSaved`, `SkinsAdded`, `SkinFile`, `SkinKept`) carrying the account it was
//! for and, for a reading or a Save, its own number, unique for the whole run: an answer for
//! another account, or one a later request replaced, changes nothing on the page. Pictures from
//! Mojang and files from disk go through two [`Queue`]s, one thread each with a bounded queue;
//! the Save itself is [`account::look::save`].
//!
//! In an offline developer session the page only shows a player in the account's default skin
//! (the game's offline look): no requests, no edits. The mode is the one in force now, read at
//! every drawing, choice and answer, not the one the page was opened in: an answer to a
//! request made online is still taken in (a Save that went through is recorded), but starts
//! nothing.
use super::*;
use crate::frame::Reach as _;
use crate::paint::palette;
use account::look::{self, Change, Kept};
use gpui::{img, rgb, ExternalPaths, MouseButton, MouseDownEvent, MouseMoveEvent, RenderImage};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use sunrise_launcher::account::capes::{self, Profile};
use sunrise_launcher::player_model::{self, Pose, Texture};
use sunrise_launcher::skin_library::{self, Saved};

/// The preview's size, in points.
const PREVIEW_W: f32 = 280.;
const PREVIEW_H: f32 = 420.;
/// A tile's picture: a skin's model or a cape's face (10 by 16 texels at 6 points).
const PICTURE_W: f32 = 60.;
const PICTURE_H: f32 = 96.;
const TILE_W: f32 = 104.;
/// The game's default skins, each with wide and slim arms.
const DEFAULT_SKINS: [&str; 9] = [
    "steve", "alex", "ari", "efe", "kai", "makena", "noor", "sunny", "zuri",
];
/// How much work a [`Queue`] holds before it refuses more.
const QUEUED: usize = 64;

/// A number for one request, never given twice in a run: an answer from before an account
/// switch can't pass for one asked after it.
fn request_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// What decides whether the offline developer view is on, besides the environment: the build
/// the selected profile plays and every file `developer_offline::enabled` reads for it (its
/// lineage records, then its base build's install and developer records, with their times and
/// sizes). The answer is kept while these stay the same.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OfflineKey {
    played: Option<String>,
    own: Vec<(PathBuf, sunrise_launcher::developer_offline::Stamp)>,
    base: String,
    base_files: Vec<(PathBuf, sunrise_launcher::developer_offline::Stamp)>,
}

/// Runs `work` on a thread of its own: false when no thread could be started (the caller says
/// so), rather than a panic on the UI thread.
fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> bool {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(work)
        .is_ok()
}

/// Sends what `work` makes, or what `fallback` makes when `work` panics: every job and thread of
/// the page answers, so nothing it counts or waits on (a write under way, a reading, a picture
/// asked for) is left waiting for an answer that never comes.
fn answer(tx: &Sender<Msg>, work: impl FnOnce() -> Msg, fallback: impl FnOnce() -> Msg) {
    let msg =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_| fallback());
    let _ = tx.send(msg);
}

/// The texture queue's job for the picture at `url`: as it starts it reads `offline`, the
/// page's own latest answer to whether the offline developer view is on
/// ([`Launcher::skins_offline`] stores it), and if the view came on since the job was queued,
/// nothing is fetched. That counts as a picture not had, asked for again once online.
fn texture_job(
    tx: Sender<Msg>,
    url: String,
    offline: Arc<AtomicBool>,
    fetch: fn(&str) -> Option<Vec<u8>>,
) -> Job {
    Box::new(move || {
        answer(
            &tx,
            || {
                let png = (!offline.load(Ordering::Relaxed))
                    .then(|| fetch(&url))
                    .flatten();
                Msg::Picture(url.clone(), png)
            },
            || Msg::Picture(url.clone(), None),
        )
    })
}

/// What a job that stopped unexpectedly says.
fn stopped() -> String {
    tr("The launcher stopped this unexpectedly. Try again.").to_string()
}

type Job = Box<dyn FnOnce() + Send>;

/// Why a [`Queue`] didn't take some work.
#[derive(Debug, PartialEq, Eq)]
enum Refused {
    /// It holds [`QUEUED`] jobs already.
    Full,
    /// Its thread couldn't be started.
    NoThread,
}

/// Work done in turn by one thread, started when first needed, that holds at most [`QUEUED`]
/// jobs: a profile with many capes, or a page of saved skins, costs one thread, not one each.
/// Work it refuses is reported by whoever asked, never waited for.
#[derive(Default)]
pub(crate) struct Queue {
    jobs: Option<SyncSender<Job>>,
}

impl Queue {
    fn push(&mut self, name: &str, job: Job) -> Result<(), Refused> {
        let job = match &self.jobs {
            Some(jobs) => match jobs.try_send(job) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(_)) => return Err(Refused::Full),
                // Its thread is gone: another one takes over.
                Err(TrySendError::Disconnected(job)) => job,
            },
            None => job,
        };
        let (jobs, queued) = mpsc::sync_channel::<Job>(QUEUED);
        jobs.try_send(job).map_err(|_| Refused::Full)?;
        spawn(name, move || {
            for job in queued {
                // One job's panic doesn't take the rest of the queue with it.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }
        })
        .then_some(())
        .ok_or(Refused::NoThread)?;
        self.jobs = Some(jobs);
        Ok(())
    }
}

/// Which skin is chosen: always a picture, never "the one worn" resolved later, so a choice
/// made while a Save changes what is worn stays the picture clicked.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SkinPick {
    /// A skin on Mojang's texture server, by its URL: one the account wore.
    Texture(String),
    /// The default skin the game gives the account's id.
    AccountDefault,
    Saved(String),
    Default(&'static str),
}

/// Which cape is chosen.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CapePick {
    None,
    Minecraft(String),
}

/// The edits not saved yet, each on its own; none leaves that part as the account wears it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Edit {
    pub skin: Option<SkinPick>,
    pub slim: Option<bool>,
    pub cape: Option<CapePick>,
}

impl Edit {
    fn is_empty(&self) -> bool {
        *self == Edit::default()
    }

    fn touches_skin(&self) -> bool {
        self.skin.is_some() || self.slim.is_some()
    }
}

/// What the account wears, as the page's choices say it. Its cape is unknown (none) until the
/// profile has been read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Worn {
    pub skin: SkinPick,
    pub slim: bool,
    pub cape: Option<CapePick>,
    /// The URL of the skin the profile (else accounts.json) says is worn; none for the
    /// account's default skin. What a Save of new arms alone checks is still worn.
    pub texture: Option<String>,
}

/// A picture from Mojang's texture server.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Picture {
    Pending,
    Ready(Vec<u8>),
    /// It couldn't be had: asked for again at the next reading of the profile.
    Failed,
}

/// A picture read from disk (a saved or default skin).
#[derive(Clone, Debug)]
enum File {
    /// Being read by the request numbered so: only its answer counts.
    Pending(u64),
    Ready(Rc<Vec<u8>>),
    /// None to be had: read again when the page is opened again.
    Missing,
}

/// A skin picture as the preview needs it.
enum Png {
    Ready(Rc<Vec<u8>>),
    /// On its way: the preview says it's loading.
    Pending,
    /// Not to be had: the preview says so, rather than loading forever.
    Missing,
}

/// The skin part of a Save, as the page decides it ([`Skins::skin_sent`]).
#[derive(Clone, Debug, PartialEq, Eq)]
enum SkinSent {
    /// New arms for the skin worn: its URL, or none for the account's default skin.
    Arms(Option<String>),
    /// The picture chosen.
    Picture(SkinPick),
}

/// Changes the services took that no profile has shown yet, as they were sent, and what was
/// worn before them. Each profile read after them is checked: what it shows is recorded in
/// accounts.json (as the Save itself records a change it saw take). One that still shows what
/// was worn before is a profile behind: the change stays awaited, for [`AWAITED_READINGS`]
/// readings at most. One that shows something else (another skin or cape) drops it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Awaited {
    /// The skin uploaded (its id, [`skin_library::id_of_png`]) and its arms.
    skin: Option<(String, bool)>,
    /// The skin worn before it (URL and model; none for a default skin).
    skin_before: Option<(String, String)>,
    /// The skin a profile then showed with those arms (its URL and model): whether it is the
    /// one uploaded is known once its picture is here.
    skin_at: Option<(String, String)>,
    /// The cape asked for, by id, or none.
    cape: Option<Option<String>>,
    /// The cape worn before it.
    cape_before: Option<String>,
    /// How many more profiles may still be behind before the changes are dropped.
    left: u8,
}

/// How many profiles read after a Save may still show what was worn before it, before the
/// changes it left unconfirmed are taken not to have happened.
const AWAITED_READINGS: u8 = 3;

/// A Save under way: its number and what it sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Submitted {
    pub id: u64,
    pub skin: Option<SkinPick>,
    pub slim: Option<bool>,
    pub cape: Option<CapePick>,
}

/// One account's page.
pub(crate) struct Skins {
    /// The account shown.
    uuid: Option<String>,
    /// What the account wears, from its profile: none while the first reading is under way.
    profile: Option<Result<Profile, String>>,
    /// Changes the services accepted that no profile has shown yet: the account is taken to
    /// wear them over `profile` until the next reading says what it wears.
    unconfirmed: Edit,
    /// The same changes as they were sent, checked against each profile read after them.
    awaited: Awaited,
    /// What later profiles showed of them, to record in accounts.json.
    confirmed: look::Record,
    /// Those being recorded now: a Save waits for them, so records are written in Save order.
    recording: usize,
    /// The reading whose answer counts.
    reading: Option<u64>,
    /// The Save under way.
    saving: Option<Submitted>,
    edit: Edit,
    /// Pictures from Mojang's texture server by URL.
    pictures: HashMap<String, Picture>,
    saved: Vec<Saved>,
    /// Why the saved skins couldn't be listed, if they couldn't.
    library_error: Option<String>,
    /// Why the skin worn couldn't be kept among them, if it couldn't: until one is kept.
    keep_error: Option<String>,
    /// The number of the latest listing of the saved skins taken in: an older one changes
    /// nothing.
    listed: u64,
    /// The saved skin that keeps each skin worn, by its URL: every skin worn is kept, so one
    /// replaced by a Save stays in the list to wear again.
    kept: HashMap<String, String>,
    /// Skins worn (by URL) being kept now.
    keeping: HashSet<String>,
    /// What the last Save said, a line for each change; or why it couldn't start.
    said: Vec<Result<String, String>>,
    /// The files of the last skins added that couldn't be, each with why: shown beside the
    /// skins, until more are added.
    added: Vec<String>,
    yaw: f32,
    pitch: f32,
    /// Where a drag on the preview began, and the turn then.
    drag: Option<(gpui::Point<gpui::Pixels>, f32, f32)>,
    /// Pictures read from disk (default and saved skins), by key.
    files: RefCell<HashMap<String, File>>,
    /// Files asked for while the page was drawn, by key: read once it has been.
    wanted: RefCell<Vec<(String, PathBuf, u64)>>,
    /// Pictures decoded, by key.
    textures: RefCell<HashMap<String, Option<Rc<Texture>>>>,
    /// Tile pictures as the window draws them, by key and scale.
    baked: RefCell<HashMap<(String, u32), Arc<RenderImage>>>,
    /// The preview drawn last, by what it shows.
    shown: Option<(u64, Arc<RenderImage>)>,
}

impl Default for Skins {
    fn default() -> Self {
        Skins {
            uuid: None,
            profile: None,
            unconfirmed: Edit::default(),
            awaited: Awaited::default(),
            confirmed: look::Record::default(),
            recording: 0,
            reading: None,
            saving: None,
            edit: Edit::default(),
            pictures: HashMap::new(),
            saved: Vec::new(),
            library_error: None,
            keep_error: None,
            listed: 0,
            kept: HashMap::new(),
            keeping: HashSet::new(),
            said: Vec::new(),
            added: Vec::new(),
            yaw: 0.55,
            pitch: 0.12,
            drag: None,
            files: RefCell::default(),
            wanted: RefCell::default(),
            textures: RefCell::default(),
            baked: RefCell::default(),
            shown: None,
        }
    }
}

/// Every account's page: the one shown, and those of the accounts shown before, kept as they
/// were left (a Save under way, edits, what the profile said). The shown page is reached
/// through `Deref`.
#[derive(Default)]
pub(crate) struct SkinsPages {
    current: Skins,
    parked: HashMap<String, Skins>,
    /// Pictures fetched from Mojang's texture server.
    net: Queue,
    /// Skin files read for the pictures drawn.
    reads: Queue,
    /// The saved skins' index listed and changed (skins added, kept, removed): apart from the
    /// reads, so a page of pictures never crowds out a change.
    writes: Queue,
    /// Saved skins being added, kept or removed now: a data move waits for them.
    disk_writes: usize,
    /// Whether the page was last opened in the offline developer view: drawn in the other
    /// mode, it is opened anew.
    opened_offline: Option<bool>,
    /// A Save that ended in the offline view changed a profile head not fetched since.
    heads_stale: bool,
    /// Whether the offline developer view is on, and what that was worked out from.
    offline: RefCell<Option<(OfflineKey, bool)>>,
    /// The latest answer to the same, for the texture jobs queued: one that starts once it is
    /// on fetches nothing.
    offline_flag: Arc<AtomicBool>,
    /// The prepared game files the default skins come from and the default skins they have,
    /// for the profile build and the builds installed they were looked for with.
    resources: RefCell<Option<ResourcesFound>>,
    /// The offline developer view, as a test sets it.
    #[cfg(test)]
    offline_for_test: Option<bool>,
}

/// [`SkinsPages::resources`]: the build played, the builds installed, and what they have.
type ResourcesFound = (
    Option<String>,
    Vec<String>,
    Option<(PathBuf, Vec<&'static str>)>,
);

impl std::ops::Deref for SkinsPages {
    type Target = Skins;
    fn deref(&self) -> &Skins {
        &self.current
    }
}

impl std::ops::DerefMut for SkinsPages {
    fn deref_mut(&mut self) -> &mut Skins {
        &mut self.current
    }
}

impl SkinsPages {
    /// Shows the page of account `uuid`: the one it had, else a new one. Pages of accounts no
    /// longer `signed_in` are forgotten, unless a Save of theirs is still under way.
    fn show(&mut self, uuid: &str, signed_in: impl Fn(&str) -> bool) {
        if self.current.uuid.as_deref() != Some(uuid) {
            let page = self
                .parked
                .remove(uuid)
                .unwrap_or_else(|| Skins::new_for(uuid));
            let before = std::mem::replace(&mut self.current, page);
            if let Some(was) = before.uuid.clone() {
                self.parked.insert(was, before);
            }
        }
        self.parked
            .retain(|uuid, page| page.saving.is_some() || signed_in(uuid));
    }

    /// The page of account `uuid`, shown or not.
    fn page_mut(&mut self, uuid: &str) -> Option<&mut Skins> {
        if self.current.uuid.as_deref() == Some(uuid) {
            Some(&mut self.current)
        } else {
            self.parked.get_mut(uuid)
        }
    }

    fn pages_mut(&mut self) -> impl Iterator<Item = &mut Skins> {
        std::iter::once(&mut self.current).chain(self.parked.values_mut())
    }

    /// A picture arrived: every page that asked for it gets it.
    fn picture(&mut self, url: &str, png: Option<Vec<u8>>) {
        for page in self.pages_mut() {
            if page.pictures.get(url) == Some(&Picture::Pending) {
                page.picture(url.to_owned(), png.clone());
            }
        }
    }

    /// A file was read: every page that asked for it gets it.
    fn file_read(&mut self, key: &str, id: u64, png: Option<Vec<u8>>) {
        let png = png.map(Rc::new);
        for page in self.pages_mut() {
            let files = page.files.get_mut();
            if matches!(files.get(key), Some(File::Pending(asked)) if *asked == id) {
                files.insert(
                    key.to_owned(),
                    png.clone().map_or(File::Missing, File::Ready),
                );
                page.forget(key);
            }
        }
    }

    /// A Save or a skin being written to disk is under way, for any account, shown or not: a
    /// data move or a launcher restart would cut it off (a Mojang change left without its
    /// record in accounts.json, a skin half added).
    pub(crate) fn writing(&self) -> bool {
        self.disk_writes > 0
            || self.current.saving.is_some()
            || self.parked.values().any(|page| page.saving.is_some())
    }
}

impl Skins {
    fn new_for(uuid: &str) -> Skins {
        Skins {
            uuid: Some(uuid.to_owned()),
            ..Default::default()
        }
    }

    /// What the account wears. `recorded` is the skin accounts.json has for it, used until the
    /// profile has been read; `default_slim` the arms of its default skin, when it wears none.
    /// Changes accepted but not yet shown by a profile count as worn.
    fn worn(&self, recorded: Option<&(String, String)>, default_slim: bool) -> Worn {
        let (skin, cape) = match &self.profile {
            Some(Ok(profile)) => (
                profile.skin.as_ref(),
                Some(match profile.worn_cape() {
                    Some(c) => CapePick::Minecraft(c.id.clone()),
                    None => CapePick::None,
                }),
            ),
            _ => (recorded, None),
        };
        let slim = match skin {
            Some((_, model)) => model == "slim",
            None => default_slim,
        };
        let texture = skin.map(|(url, _)| url.clone());
        let pick = match &texture {
            Some(url) => self.picked(SkinPick::Texture(url.clone())),
            None => SkinPick::AccountDefault,
        };
        Worn {
            skin: self.unconfirmed.skin.clone().unwrap_or(pick),
            slim: self.unconfirmed.slim.unwrap_or(slim),
            cape: self.unconfirmed.cape.clone().or(cape),
            texture,
        }
    }

    /// `skin` as the page lists it: a skin worn at a URL is the saved skin that keeps it, once
    /// one does.
    fn picked(&self, skin: SkinPick) -> SkinPick {
        match &skin {
            SkinPick::Texture(url) => self
                .kept
                .get(url)
                .map_or(skin.clone(), |id| SkinPick::Saved(id.clone())),
            _ => skin,
        }
    }

    /// The look the preview shows: each edit over what is worn.
    fn chosen(&self, worn: &Worn) -> (SkinPick, bool, Option<CapePick>) {
        (
            self.edit.skin.clone().unwrap_or_else(|| worn.skin.clone()),
            self.edit.slim.unwrap_or(worn.slim),
            self.edit.cape.clone().or_else(|| worn.cape.clone()),
        )
    }

    /// Forgets the edits that ask for what is worn already. A cape edit stays while the worn
    /// cape is unknown: it is an explicit choice. While a Save runs nothing is forgotten: what
    /// is worn is about to change, and a choice that matches it now may not match it after.
    fn settle(&mut self, worn: &Worn) {
        if self.saving.is_some() {
            return;
        }
        self.edit.skin = self.edit.skin.take().map(|skin| self.picked(skin));
        if self.edit.skin.as_ref() == Some(&worn.skin) {
            self.edit.skin = None;
        }
        if self.edit.skin.is_none() && self.edit.slim == Some(worn.slim) {
            self.edit.slim = None;
        }
        if worn.cape.is_some() && self.edit.cape == worn.cape {
            self.edit.cape = None;
        }
    }

    /// Chooses `change` of the look, the account wearing `worn`: what the last Save said gives
    /// way to the choice, but not the files the last import couldn't add.
    fn choose(&mut self, change: impl FnOnce(&mut Edit), worn: &Worn) {
        change(&mut self.edit);
        self.settle(worn);
        self.said.clear();
    }

    /// A reading asked for.
    fn read_asked(&mut self) -> u64 {
        let id = request_id();
        self.reading = Some(id);
        id
    }

    /// A reading answered: kept when it is for this page's account and the last one asked, the
    /// page isn't `offline` now, and no Save is under way (its answer is newer than any
    /// reading made before it ends).
    fn read_answered(
        &mut self,
        uuid: &str,
        id: u64,
        result: Result<Profile, String>,
        offline: bool,
    ) -> bool {
        if self.uuid.as_deref() != Some(uuid) || self.reading != Some(id) {
            return false;
        }
        self.reading = None;
        // Answered: in the offline view it is taken in no more than during a Save, and a later
        // opening reads again.
        if offline || self.saving.is_some() {
            return false;
        }
        if let Ok(profile) = &result {
            // The reading says what is worn: accepted changes are in it, or didn't take.
            self.profile_seen(profile);
            self.unconfirmed = Edit::default();
        }
        // A reading that failed after one that worked keeps what was known.
        if result.is_ok() || !matches!(self.profile, Some(Ok(_))) {
            self.profile = Some(result);
        }
        true
    }

    /// Checks the changes still awaited against `profile`. A cape worn as asked is confirmed (to
    /// record); a skin worn with the arms sent is compared with the one uploaded once its
    /// picture is here ([`Self::skin_checked`]). A profile still showing what was worn before is
    /// behind: the change stays awaited, [`AWAITED_READINGS`] profiles at most. One showing
    /// anything else drops it, the profile being what the account wears.
    fn profile_seen(&mut self, profile: &Profile) {
        let awaited = &mut self.awaited;
        if awaited.skin.is_none() && awaited.cape.is_none() {
            return;
        }
        if let Some(sent) = awaited.cape.clone() {
            let worn = profile.worn_cape();
            let worn_id = worn.map(|c| c.id.clone());
            if worn_id == sent {
                self.confirmed.cape = Some(worn.map(|c| c.url.clone()));
                awaited.cape = None;
            } else if worn_id != awaited.cape_before {
                awaited.cape = None;
            }
        }
        if let Some((_, slim)) = &awaited.skin {
            match &profile.skin {
                // Behind: what was worn before.
                shown if *shown == awaited.skin_before => {}
                Some((url, model)) if (model == "slim") == *slim => {
                    awaited.skin_at = Some((url.clone(), model.clone()));
                }
                _ => {
                    awaited.skin = None;
                    awaited.skin_at = None;
                }
            }
        }
        if awaited.skin.is_some() || awaited.cape.is_some() {
            awaited.left = awaited.left.saturating_sub(1);
            if awaited.left == 0 && awaited.skin_at.is_none() {
                *awaited = Awaited::default();
            }
        }
        self.skin_checked();
    }

    /// Compares the skin awaited with the one a profile showed, once that one's picture is
    /// here: the same skin is confirmed (to record), another is dropped. A picture that
    /// couldn't be had leaves it awaited: it is asked for again at the next reading.
    fn skin_checked(&mut self) {
        self.skin_compared();
        // Nothing left awaited: nothing of it is kept either.
        if self.awaited.skin.is_none() && self.awaited.cape.is_none() {
            self.awaited = Awaited::default();
        }
    }

    fn skin_compared(&mut self) {
        let (Some((id, _)), Some((url, model))) = (&self.awaited.skin, &self.awaited.skin_at)
        else {
            return;
        };
        let same = match self.pictures.get(url) {
            Some(Picture::Ready(png)) => skin_library::id_of_png(png).as_ref() == Some(id),
            Some(Picture::Failed | Picture::Pending) | None => return,
        };
        if same {
            self.confirmed.skin = Some(Some((url.clone(), model.clone())));
        }
        self.awaited.skin = None;
        self.awaited.skin_at = None;
        self.awaited.skin_before = None;
    }

    /// Asks for a picture from Mojang's texture server: true when it is to be fetched (not
    /// here or on its way already, and the page isn't `offline`). One that couldn't be had
    /// before is asked for again.
    fn ask_picture(&mut self, url: &str, offline: bool) -> bool {
        if offline
            || matches!(
                self.pictures.get(url),
                Some(Picture::Pending | Picture::Ready(_))
            )
        {
            return false;
        }
        self.pictures.insert(url.to_owned(), Picture::Pending);
        true
    }

    /// A Save of the edits asked for.
    fn submit(&mut self, offline: bool) -> Option<Submitted> {
        if offline || self.edit.is_empty() || self.saving.is_some() || self.recording > 0 {
            return None;
        }
        // A change this Save makes again is no longer one to record from an earlier Save.
        if self.edit.touches_skin() {
            self.awaited.skin = None;
            self.awaited.skin_at = None;
        }
        if self.edit.cape.is_some() {
            self.awaited.cape = None;
        }
        let submitted = Submitted {
            id: request_id(),
            skin: self.edit.skin.clone(),
            slim: self.edit.slim,
            cape: self.edit.cape.clone(),
        };
        self.saving = Some(submitted.clone());
        self.said.clear();
        Some(submitted)
    }

    /// The Save `id` answered, if it is this page's. Each change the services accepted is done:
    /// its edit is forgotten unless the player changed it again meanwhile, and it isn't sent
    /// again. What a profile showed is what is worn; what was accepted but not shown is taken
    /// as worn until the next reading. What failed stays to do. Choices made meanwhile are the
    /// pictures clicked, and stay as they are. The caller then settles the edits against what
    /// is worn now.
    fn save_answered(&mut self, id: u64, outcome: &look::Outcome) -> bool {
        let Some(submitted) = self.saving.take_if(|s| s.id == id) else {
            return false;
        };
        // A reading asked before this Save ended may say what was worn before it.
        self.reading = None;
        // What was worn before this Save: read before `outcome.profile` replaces `self.profile`.
        let before = match &self.profile {
            Some(Ok(profile)) => Some(profile.clone()),
            _ => None,
        };
        if let Some(profile) = &outcome.profile {
            // A change an earlier Save left unconfirmed may show in it.
            self.profile_seen(profile);
            self.profile = Some(Ok(profile.clone()));
            self.unconfirmed = Edit::default();
        }
        if let Kept::Kept { url, saved } = &outcome.kept {
            self.kept.insert(url.clone(), saved.id.clone());
            self.forget_file(&saved_key(self.uuid.as_deref(), &saved.id));
        }
        let accepted = |change: &Option<Change>| change.as_ref().is_some_and(Change::accepted);
        if matches!(outcome.skin, Some(Change::Unconfirmed(_))) {
            self.unconfirmed.skin = submitted.skin.clone();
            self.unconfirmed.slim = submitted.slim;
            self.awaited.skin = outcome.uploaded.clone();
            self.awaited.skin_before = before.as_ref().and_then(|p| p.skin.clone());
            self.awaited.skin_at = None;
            self.awaited.left = AWAITED_READINGS;
        }
        if matches!(outcome.cape, Some(Change::Unconfirmed(_))) {
            self.unconfirmed.cape = submitted.cape.clone();
            self.awaited.cape = submitted.cape.clone().map(|cape| match cape {
                CapePick::Minecraft(id) => Some(id),
                CapePick::None => None,
            });
            self.awaited.cape_before = before
                .as_ref()
                .and_then(|p| p.worn_cape().map(|c| c.id.clone()));
            self.awaited.left = AWAITED_READINGS;
        }
        if accepted(&outcome.skin) {
            // A newer accepted write supersedes any earlier confirmed record still waiting on the
            // write queue for the same field.
            self.confirmed.skin = None;
            if self.edit.skin == submitted.skin {
                self.edit.skin = None;
            }
            if self.edit.slim == submitted.slim {
                self.edit.slim = None;
            }
        }
        if accepted(&outcome.cape) {
            self.confirmed.cape = None;
            if self.edit.cape == submitted.cape {
                self.edit.cape = None;
            }
        }
        self.said = said(outcome);
        true
    }

    /// The pictures not here or on their way: those never asked for, and those that couldn't
    /// be had.
    fn unasked<'a>(&self, urls: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        urls.into_iter()
            .filter(|url| {
                !matches!(
                    self.pictures.get(*url),
                    Some(Picture::Pending | Picture::Ready(_))
                )
            })
            .cloned()
            .collect()
    }

    fn picture(&mut self, url: String, png: Option<Vec<u8>>) {
        self.forget(&url);
        self.pictures
            .insert(url, png.map_or(Picture::Failed, Picture::Ready));
        self.skin_checked();
    }

    fn picture_png(&self, url: &str) -> Png {
        match self.pictures.get(url) {
            Some(Picture::Ready(png)) => Png::Ready(Rc::new(png.clone())),
            Some(Picture::Failed) => Png::Missing,
            Some(Picture::Pending) | None => Png::Pending,
        }
    }

    /// Drops what was drawn from the picture `key`.
    fn forget(&mut self, key: &str) {
        self.textures.get_mut().remove(key);
        self.baked.get_mut().retain(|(k, _), _| !k.starts_with(key));
    }

    /// Drops the file `key` and what was drawn from it: it is read again when next drawn.
    fn forget_file(&mut self, key: &str) {
        self.files.get_mut().remove(key);
        self.forget(key);
    }

    /// The picture at `path` (none: there is none to look for), read once, on a worker: asked
    /// for here and read once the page has been drawn.
    fn file(&self, key: &str, path: impl FnOnce() -> Option<PathBuf>) -> Png {
        if let Some(file) = self.files.borrow().get(key) {
            return match file {
                File::Pending(_) => Png::Pending,
                File::Ready(png) => Png::Ready(png.clone()),
                File::Missing => Png::Missing,
            };
        }
        let Some(path) = path() else {
            self.files
                .borrow_mut()
                .insert(key.to_owned(), File::Missing);
            return Png::Missing;
        };
        let id = request_id();
        self.files
            .borrow_mut()
            .insert(key.to_owned(), File::Pending(id));
        self.wanted.borrow_mut().push((key.to_owned(), path, id));
        Png::Pending
    }

    /// The picture with `key`, decoded once by `decode`.
    fn texture(
        &self,
        key: &str,
        png: &[u8],
        decode: fn(&[u8]) -> Option<Texture>,
    ) -> Option<Rc<Texture>> {
        if let Some(known) = self.textures.borrow().get(key) {
            return known.clone();
        }
        let texture = decode(png).map(Rc::new);
        self.textures
            .borrow_mut()
            .insert(key.to_owned(), texture.clone());
        texture
    }

    /// A listing of the saved skins arrived (numbered `id`): taken in unless a newer one was.
    /// It says only whether the list could be read; why the skin worn couldn't be kept stays.
    fn listing(&mut self, (id, listed): skin_library::Listing) {
        if id <= self.listed {
            return;
        }
        self.listed = id;
        match listed {
            Ok(saved) => {
                self.saved = saved;
                self.library_error = None;
            }
            Err(why) => self.library_error = Some(why),
        }
    }

    /// Skin files were added: those that couldn't be are kept to show (a choice made after
    /// doesn't hide them); the last one that was is returned, to be chosen.
    fn files_added(&mut self, added: Vec<Result<Saved, String>>) -> Option<Saved> {
        let mut last = None;
        self.added.clear();
        for result in added {
            match result {
                Ok(saved) => {
                    // Its picture may have been written anew.
                    self.forget_file(&saved_key(self.uuid.as_deref(), &saved.id));
                    last = Some(saved);
                }
                Err(why) => self.added.push(why),
            }
        }
        last
    }

    /// What a Save sends as the skin, when the skin or its arms were edited: new arms alone
    /// (the picture chosen is the one worn, and no accepted change still stands in for it) are
    /// sent as such, with the URL of the skin the page shows as worn (none: the account's
    /// default skin), so the Save keeps what the account wears when it starts; anything else
    /// is the picture chosen.
    fn skin_sent(&self, worn: &Worn) -> Option<SkinSent> {
        if !self.edit.touches_skin() {
            return None;
        }
        let (skin, _, _) = self.chosen(worn);
        Some(if skin == worn.skin && self.unconfirmed.skin.is_none() {
            SkinSent::Arms(worn.texture.clone())
        } else {
            SkinSent::Picture(skin)
        })
    }

    /// A saved skin was removed: no skin worn is taken for it any more, and its picture is
    /// forgotten.
    fn removed(&mut self, id: &str) {
        self.kept.retain(|_, kept| kept != id);
        self.forget_file(&saved_key(self.uuid.as_deref(), id));
    }
}

/// What a Save says it did: a line for the skin and one for the cape, each done, taken but not
/// confirmed, or not done and why; then anything it couldn't keep track of.
fn said(outcome: &look::Outcome) -> Vec<Result<String, String>> {
    let mut lines = Vec::new();
    match &outcome.skin {
        None => {}
        Some(Change::Done) => lines.push(Ok(tr("Skin saved. Everyone sees it.").to_string())),
        Some(Change::Unconfirmed(why)) => lines.push(Err(trf(
            "Your new skin isn't confirmed yet, so it won't be sent again: {why}",
            &[("why", why)],
        ))),
        Some(Change::Failed(why)) => lines.push(Err(match &outcome.kept {
            Kept::Failed(_) => trf(
                "The skin you wear now couldn't be kept, so it wasn't replaced: {why}",
                &[("why", why)],
            ),
            _ => trf("Your skin wasn't changed: {why}", &[("why", why)]),
        })),
    }
    match &outcome.cape {
        None => {}
        Some(Change::Done) => lines.push(Ok(tr("Cape changed. Everyone sees it.").to_string())),
        Some(Change::Unconfirmed(why)) => lines.push(Err(trf(
            "Your cape change isn't confirmed yet, so it won't be sent again: {why}",
            &[("why", why)],
        ))),
        Some(Change::Failed(why)) => {
            lines.push(Err(trf("Your cape wasn't changed: {why}", &[("why", why)])))
        }
    }
    if let Some(why) = &outcome.arms {
        lines.push(Err(trf(
            "The saved skin's arms couldn't be kept: {why}",
            &[("why", why)],
        )));
    }
    if let Some(why) = &outcome.recorded {
        lines.push(Err(trf(
            "The launcher couldn't note down what was saved: {why}",
            &[("why", why)],
        )));
    }
    lines
}

/// The key of the saved skin `id` of account `uuid` among the files a page reads: each
/// account's own, as each has its own folder.
fn saved_key(uuid: Option<&str>, id: &str) -> String {
    format!("saved:{}:{id}", uuid.unwrap_or_default())
}

/// A default skin's file in the prepared game files.
fn default_skin_path(resources: &Path, name: &str, slim: bool) -> PathBuf {
    resources
        .join("assets/minecraft/textures/entity/player")
        .join(if slim { "slim" } else { "wide" })
        .join(format!("{name}.png"))
}

/// Reads the skin files at `paths` into `uuid`'s saved skins, off the UI thread: only regular
/// files (a pipe dropped on the page would block a read), none bigger than a skin. A file that
/// can't be added says so by its name.
fn add_files(uuid: &str, paths: &[PathBuf]) -> Vec<Result<Saved, String>> {
    let dir = match skin_library::folder(uuid) {
        Ok(dir) => dir,
        Err(why) => return vec![Err(why)],
    };
    paths
        .iter()
        .map(|path| {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            skin_library::read_skin_file(path)
                .and_then(|png| skin_library::add(&dir, &png, &name, None))
                .map_err(|why| {
                    let file = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    trf(
                        "{file} couldn't be added: {why}",
                        &[("file", &file), ("why", &why)],
                    )
                })
        })
        .collect()
}

/// What a Save that stopped unexpectedly reports: each change it was asked for may have gone
/// through already, so none is taken as refused (sent again) or done; the profile is read again.
fn stopped_outcome((skin, cape): (bool, bool)) -> look::Outcome {
    let change = |asked: bool| asked.then(|| Change::Unconfirmed(stopped()));
    look::Outcome {
        kept: Kept::Nothing,
        skin: change(skin),
        cape: change(cape),
        profile: None,
        arms: None,
        recorded: None,
        uploaded: None,
    }
}

/// The file of the default skin the game gives `uuid`, with slim or classic arms: the same
/// skin's picture for the arms asked, since each default skin comes in both.
fn account_default_path(resources: &Path, uuid: &str, slim: bool) -> Option<PathBuf> {
    let (path, _) = sunrise_launcher::heads::default_skin_file(resources, uuid)?;
    let name = path.file_stem()?.to_str()?;
    Some(default_skin_path(resources, name, slim))
}

/// Lists `uuid`'s saved skins, numbered `id`: what a job on the writes queue sends once it is
/// done, so the page shows the list as that job left it.
fn listing(uuid: &str, id: u64) -> skin_library::Listing {
    (
        id,
        skin_library::folder(uuid).and_then(|dir| skin_library::list(&dir)),
    )
}

impl Launcher {
    /// The prepared game files the default skins are read from, and the default skins they
    /// have: the version the selected profile plays when it has them, else the first installed
    /// version that does. None when no game is prepared: nothing is looked for relative to the
    /// working folder. Looked for once for the build played and the builds installed, not at
    /// every drawing.
    fn skin_resources_found(&self) -> Option<(PathBuf, Vec<&'static str>)> {
        let selected = self.selected().cloned();
        if let Some((played, installed, found)) = &*self.skins.resources.borrow() {
            if *played == selected && *installed == self.installed {
                return found.clone();
            }
        }
        let has = |resources: &Path| default_skin_path(resources, "steve", false).is_file();
        let found = selected
            .iter()
            .chain(self.installed.iter())
            .map(|tag| backend::resources(tag))
            .find(|resources| has(resources))
            .map(|resources| {
                let defaults = DEFAULT_SKINS
                    .into_iter()
                    .filter(|name| default_skin_path(&resources, name, false).is_file())
                    .collect();
                (resources, defaults)
            });
        *self.skins.resources.borrow_mut() =
            Some((selected, self.installed.clone(), found.clone()));
        found
    }

    fn skin_resources(&self) -> Option<PathBuf> {
        self.skin_resources_found().map(|(resources, _)| resources)
    }

    /// Whether the page is in the offline developer view now: the mode in force, not the one
    /// the page was opened in, since another profile can be chosen while it is open. The
    /// environment is checked first, so a launcher not started for offline play reads no files
    /// for it. In one that was, the build the selected profile plays is asked
    /// (`developer_offline::enabled`) again only when that build, or a file `enabled` reads for
    /// it (`developer_offline::inputs`), changed.
    ///
    /// Each answer is also stored for the page's queued texture jobs, which read it as they
    /// start, so they go by the same answer as the page.
    pub(super) fn skins_offline(&self) -> bool {
        let offline = self.skins_offline_worked_out();
        self.skins.offline_flag.store(offline, Ordering::Relaxed);
        offline
    }

    fn skins_offline_worked_out(&self) -> bool {
        #[cfg(test)]
        if let Some(offline) = self.skins.offline_for_test {
            return offline;
        }
        if !sunrise_launcher::developer_offline::requested() {
            return false;
        }
        self.offline_cached(sunrise_launcher::developer_offline::enabled)
    }

    /// The offline developer mode for the build played, as `enabled` says, asked only when what
    /// it depends on ([`OfflineKey`]) changed since it was last asked.
    ///
    /// At a drawing this costs four file times, no reads: the base build named by the build's
    /// lineage records is read again only when those records change.
    fn offline_cached(&self, enabled: impl FnOnce(&str) -> bool) -> bool {
        use sunrise_launcher::developer_offline as offline_play;
        let played = self.selected().cloned();
        let Some(tag) = played.clone() else {
            return false;
        };
        let own: Vec<_> = offline_play::records(&tag)
            .into_iter()
            .map(offline_play::stamp)
            .collect();
        let known_base = self.skins.offline.borrow().as_ref().and_then(|(key, _)| {
            (key.played == played && key.own == own).then(|| key.base.clone())
        });
        let base = known_base.unwrap_or_else(|| offline_play::base(&tag));
        let base_files = offline_play::base_records(&base)
            .into_iter()
            .map(offline_play::stamp)
            .collect();
        let key = OfflineKey {
            played,
            own,
            base,
            base_files,
        };
        if let Some((known, offline)) = &*self.skins.offline.borrow() {
            if *known == key {
                return *offline;
            }
        }
        let offline = enabled(&tag);
        *self.skins.offline.borrow_mut() = Some((key, offline));
        offline
    }

    /// The account whose look the page shows: the one that plays (the selected one, else the
    /// first), as the game picks it.
    fn look_account(&self) -> Option<account::Account> {
        self.accounts
            .selected()
            .or_else(|| self.accounts.accounts.first())
            .cloned()
    }

    /// The page opened, another account plays, or the offline developer mode changed: what
    /// the account wears is read again, and what couldn't be had before is asked for again.
    /// The files the last import couldn't add stay listed until the next import.
    pub(crate) fn skins_open(&mut self) {
        // Looked for again: a game may have been prepared since.
        *self.skins.resources.get_mut() = None;
        *self.skins.offline.get_mut() = None;
        let offline = self.skins_offline();
        self.skins.opened_offline = Some(offline);
        let Some(account) = self.look_account() else {
            return;
        };
        let accounts = &self.accounts.accounts;
        self.skins.show(&account.uuid, |uuid| {
            accounts.iter().any(|a| a.uuid == uuid)
        });
        self.skins
            .files
            .get_mut()
            .retain(|_, file| !matches!(file, File::Missing));
        if offline {
            // A reading under way is dropped: its answer starts nothing.
            self.skins.reading = None;
            return;
        }
        if std::mem::take(&mut self.skins.heads_stale) {
            self.load_profile_heads();
        }
        self.skins_list(&account.uuid);
        if self.skins.saving.is_none() {
            self.profile_read(&account);
        }
        if let Some((url, _)) = &account.skin {
            self.picture_ask(url.clone());
        }
    }

    /// Lists `uuid`'s saved skins on the writes queue, after the changes queued before it.
    fn skins_list(&mut self, uuid: &str) {
        let (tx, account, id) = (self.tx.clone(), uuid.to_owned(), request_id());
        let job = Box::new(move || {
            answer(
                &tx,
                || Msg::SkinsListed(account.clone(), listing(&account, id)),
                || Msg::SkinsListed(account.clone(), (id, Err(stopped()))),
            )
        });
        if self.skins.writes.push("skins-index", job).is_err() {
            if let Some(page) = self.skins.page_mut(uuid) {
                page.listing((
                    id,
                    Err(tr(
                        "The launcher is busy with your saved skins. Open Skins again in a moment.",
                    )
                    .to_string()),
                ));
            }
        }
    }

    /// The saved skins were listed (`Msg::SkinsListed`).
    pub(crate) fn skins_listed(&mut self, uuid: String, listed: skin_library::Listing) {
        if let Some(page) = self.skins.page_mut(&uuid) {
            page.listing(listed);
        }
    }

    /// Reads what the shown page's account wears, on a thread of its own: one reading at a
    /// time for a page, so asking again while one is under way (Try again, the page opened
    /// again) waits for that one's answer instead of starting another.
    fn profile_read(&mut self, account: &account::Account) {
        if self.skins_offline()
            || self.skins.uuid.as_deref() != Some(&account.uuid)
            || self.skins.reading.is_some()
        {
            return;
        }
        let id = self.skins.read_asked();
        let (account, tx) = (account.clone(), self.tx.clone());
        let other =
            tr("The Minecraft services answered with another account's profile.").to_string();
        let started = spawn("skins-profile", move || {
            let uuid = account.uuid.clone();
            answer(
                &tx,
                || {
                    let ep = account::Endpoints::live();
                    let result = account::token_for(
                        &ep,
                        account::client_id().as_deref(),
                        account::store(),
                        &account,
                    )
                    .and_then(|token| capes::read(&ep, &token))
                    .and_then(|profile| {
                        profile
                            .is_for(&account.uuid)
                            .then_some(profile)
                            .ok_or(other)
                    });
                    Msg::SkinsProfile(account.uuid.clone(), id, result)
                },
                || Msg::SkinsProfile(uuid, id, Err(stopped())),
            )
        });
        if !started {
            self.skins.reading = None;
            if !matches!(self.skins.profile, Some(Ok(_))) {
                self.skins.profile = Some(Err(tr(
                    "The launcher couldn't start looking up your profile. Try again.",
                )
                .to_string()));
            }
        }
    }

    /// Fetches a picture from Mojang's texture server, on the texture queue; one it won't take
    /// counts as one that couldn't be had (asked for again at the next reading).
    fn picture_ask(&mut self, url: String) {
        let offline = self.skins_offline();
        if !self.skins.ask_picture(&url, offline) {
            return;
        }
        let job = texture_job(
            self.tx.clone(),
            url.clone(),
            self.skins.offline_flag.clone(),
            capes::picture,
        );
        if self.skins.net.push("skins-textures", job).is_err() {
            self.skins_picture(url, None);
        }
    }

    /// A reading answered (`Msg::SkinsProfile`): taken in by the page that asked for it, shown
    /// or not, and what it confirmed is recorded; only the shown page, online, asks for the
    /// pictures it names.
    pub(crate) fn skins_profile(&mut self, uuid: String, id: u64, result: Result<Profile, String>) {
        let offline = self.skins_offline();
        let shown = self.skins.uuid.as_deref() == Some(&uuid);
        let Some(page) = self.skins.page_mut(&uuid) else {
            return;
        };
        if !page.read_answered(&uuid, id, result, offline) {
            return;
        }
        self.record_confirmed(&uuid);
        if shown {
            self.after_profile();
        }
    }

    /// Records in accounts.json what profiles read after a Save showed of the changes it left
    /// unconfirmed ([`Skins::confirmed`]): only those fields, on the writes queue, and the
    /// page's next Save waits until they are written, so an account's records stay in the order
    /// of its Saves. One the queue won't take stays to record, and is tried again.
    fn record_confirmed(&mut self, uuid: &str) {
        let Some(page) = self.skins.page_mut(uuid) else {
            return;
        };
        if page.confirmed == look::Record::default() {
            return;
        }
        let record = page.confirmed.clone();
        let (tx, account, job_record) = (self.tx.clone(), uuid.to_owned(), record.clone());
        let job = Box::new(move || {
            answer(
                &tx,
                || {
                    let saved = account::Accounts::save_look(
                        &account,
                        job_record.skin.clone(),
                        job_record.cape.clone(),
                    );
                    Msg::LookRecorded(account.clone(), job_record.clone(), saved)
                },
                || Msg::LookRecorded(account.clone(), job_record.clone(), Err(stopped())),
            )
        });
        let queued = self.skins.writes.push("skins-index", job).is_ok();
        let Some(page) = self.skins.page_mut(uuid) else {
            return;
        };
        if queued {
            page.confirmed = look::Record::default();
            page.recording += 1;
            self.skins.disk_writes += 1;
        }
        // Not queued: it stays to record, tried again at the next reading or picture.
    }

    /// What a later profile confirmed was recorded, or couldn't be (`Msg::LookRecorded`). Only
    /// once it is in accounts.json does the account chip show it, so the chip never shows a
    /// look the launcher hasn't kept.
    pub(crate) fn look_recorded(
        &mut self,
        uuid: String,
        record: look::Record,
        recorded: Result<(), String>,
    ) {
        self.skins.disk_writes = self.skins.disk_writes.saturating_sub(1);
        if let Some(page) = self.skins.page_mut(&uuid) {
            page.recording = page.recording.saturating_sub(1);
            if let Err(why) = &recorded {
                page.said.push(Err(trf(
                    "The launcher couldn't note down what was saved: {why}",
                    &[("why", why)],
                )));
            }
        }
        if recorded.is_err() {
            return;
        }
        if let Some(a) = self.accounts.accounts.iter_mut().find(|a| a.uuid == uuid) {
            if let Some(skin) = record.skin {
                a.skin = skin;
            }
            if let Some(cape) = record.cape {
                a.cape = cape;
            }
        }
        if self.skins_offline() {
            self.skins.heads_stale = true;
        } else {
            self.load_profile_heads();
        }
    }

    fn after_profile(&mut self) {
        if self.skins_offline() {
            return;
        }
        if let Some(Ok(profile)) = &self.skins.profile {
            let urls: Vec<String> = profile
                .skin
                .iter()
                .map(|(url, _)| url)
                .chain(profile.capes.iter().map(|c| &c.url))
                .cloned()
                .collect();
            for url in self.skins.unasked(&urls) {
                self.picture_ask(url);
            }
        }
        self.remember_worn();
        self.settle_edits();
    }

    /// What account `uuid`'s look is worked out from besides its page: the skin accounts.json
    /// has for it, and the arms of its default skin.
    fn worn_inputs(&self, uuid: &str) -> (Option<(String, String)>, bool) {
        let recorded = self
            .accounts
            .accounts
            .iter()
            .find(|a| a.uuid == uuid)
            .and_then(|a| a.skin.clone());
        (recorded, self.default_slim(uuid))
    }

    /// Whether the default skin the game gives `uuid` has slim arms.
    fn default_slim(&self, uuid: &str) -> bool {
        self.skin_resources().is_some_and(|resources| {
            sunrise_launcher::heads::default_skin_file(&resources, uuid)
                .is_some_and(|(_, slim)| slim)
        })
    }

    fn worn_look(&self, account: &account::Account) -> Worn {
        let (recorded, default_slim) = self.worn_inputs(&account.uuid);
        self.skins.worn(recorded.as_ref(), default_slim)
    }

    /// Settles the shown page's edits against what its own account wears (not the account
    /// that plays, which a switch may have changed before the page is drawn again).
    fn settle_edits(&mut self) {
        let Some(uuid) = self.skins.uuid.clone() else {
            return;
        };
        let (recorded, default_slim) = self.worn_inputs(&uuid);
        let worn = self.skins.worn(recorded.as_ref(), default_slim);
        self.skins.settle(&worn);
    }

    /// A picture from the texture server arrived (`Msg::Picture`), or couldn't be had.
    pub(crate) fn skins_picture(&mut self, url: String, png: Option<Vec<u8>>) {
        self.skins.picture(&url, png);
        // A picture can confirm a skin a Save left unconfirmed, on any page that waits for it.
        let pages: Vec<String> = std::iter::once(&self.skins.current)
            .chain(self.skins.parked.values())
            .filter(|page| page.confirmed != look::Record::default())
            .filter_map(|page| page.uuid.clone())
            .collect();
        for uuid in pages {
            self.record_confirmed(&uuid);
        }
        self.remember_worn();
    }

    /// A skin file was read (`Msg::SkinFile`).
    pub(crate) fn skins_file(&mut self, key: String, id: u64, png: Option<Vec<u8>>) {
        self.skins.file_read(&key, id, png);
    }

    /// The skin the shown page's account wears now, as its profile says (else accounts.json).
    fn worn_skin(&self) -> Option<(String, String)> {
        match &self.skins.profile {
            Some(Ok(profile)) => profile.skin.clone(),
            _ => {
                let uuid = self.skins.uuid.as_deref()?;
                let account = self.accounts.accounts.iter().find(|a| a.uuid == uuid)?;
                account.skin.clone()
            }
        }
    }

    /// Keeps the skin worn now among the saved skins, with the arms it is worn with, on the
    /// writes queue (once: the same skin is saved once).
    fn remember_worn(&mut self) {
        if self.skins_offline() {
            return;
        }
        let (Some(uuid), Some((url, model))) = (self.skins.uuid.clone(), self.worn_skin()) else {
            return;
        };
        if self.skins.kept.contains_key(&url) || self.skins.keeping.contains(&url) {
            return;
        }
        let Some(Picture::Ready(png)) = self.skins.pictures.get(&url) else {
            return;
        };
        let (png, tx, address, id) = (png.clone(), self.tx.clone(), url.clone(), request_id());
        let job = Box::new(move || {
            answer(
                &tx,
                || {
                    // Kept without a name: its tile shows the skin alone.
                    let kept = skin_library::folder(&uuid)
                        .and_then(|dir| skin_library::keep_seen(&dir, &png, model == "slim"));
                    Msg::SkinKept(uuid.clone(), address.clone(), kept, listing(&uuid, id))
                },
                || {
                    let listed = (id, Err(stopped()));
                    Msg::SkinKept(uuid.clone(), address.clone(), Err(stopped()), listed)
                },
            )
        });
        match self.skins.writes.push("skins-index", job) {
            Ok(()) => {
                self.skins.keeping.insert(url);
                self.skins.disk_writes += 1;
            }
            Err(_) => {
                self.skins.keep_error = Some(
                    tr("The skin you wear couldn't be kept among your saved skins just now. Open Skins again to retry.")
                        .to_string(),
                );
            }
        }
    }

    /// The skin worn was kept, or couldn't be (`Msg::SkinKept`).
    pub(crate) fn skins_kept(
        &mut self,
        uuid: String,
        url: String,
        kept: Result<Saved, String>,
        listed: skin_library::Listing,
    ) {
        self.skins.disk_writes = self.skins.disk_writes.saturating_sub(1);
        let (recorded, default_slim) = self.worn_inputs(&uuid);
        let Some(page) = self.skins.page_mut(&uuid) else {
            return;
        };
        page.keeping.remove(&url);
        page.listing(listed);
        match kept {
            Ok(saved) => {
                page.keep_error = None;
                page.forget_file(&saved_key(Some(&uuid), &saved.id));
                page.kept.insert(url, saved.id);
                let worn = page.worn(recorded.as_ref(), default_slim);
                page.settle(&worn);
            }
            Err(why) => page.keep_error = Some(why),
        }
    }

    /// Chooses `change` of the look; an edit that asks for what is worn is forgotten.
    fn choose(&mut self, change: impl FnOnce(&mut Edit)) {
        let Some(uuid) = self.skins.uuid.clone() else {
            return;
        };
        if self.skins_offline() {
            return;
        }
        let (recorded, default_slim) = self.worn_inputs(&uuid);
        let worn = self.skins.worn(recorded.as_ref(), default_slim);
        self.skins.choose(change, &worn);
    }

    /// The key and picture of `skin` with wide or slim arms, for account `uuid`.
    fn skin_png(&self, uuid: &str, skin: &SkinPick, slim: bool) -> (String, Png) {
        match skin {
            SkinPick::Texture(url) => (url.clone(), self.skins.picture_png(url)),
            SkinPick::AccountDefault => {
                let Some(resources) = self.skin_resources() else {
                    return (String::new(), Png::Missing);
                };
                // The account's default skin with the arms shown: each comes in both.
                let key = format!("uuid-default:{}:{uuid}:{slim}", resources.display());
                let png = self
                    .skins
                    .file(&key, || account_default_path(&resources, uuid, slim));
                (key, png)
            }
            SkinPick::Saved(id) => {
                let key = saved_key(Some(uuid), id);
                let png = self.skins.file(&key, || {
                    Some(skin_library::folder(uuid).ok()?.join(format!("{id}.png")))
                });
                (key, png)
            }
            SkinPick::Default(name) => {
                let Some(resources) = self.skin_resources() else {
                    return (String::new(), Png::Missing);
                };
                // Keyed by the game files they come from: another install or profile has its own.
                let key = format!("default:{}:{name}:{slim}", resources.display());
                let png = self
                    .skins
                    .file(&key, || Some(default_skin_path(&resources, name, slim)));
                (key, png)
            }
        }
    }

    /// Reads the files asked for while the page was drawn, on the reads queue. One it won't
    /// take now is asked for again when the page is next drawn (a file read ending redraws it).
    fn skins_read_files(&mut self) {
        let wanted = std::mem::take(self.skins.wanted.get_mut());
        for (key, path, id) in wanted {
            let (tx, name) = (self.tx.clone(), key.clone());
            let job = Box::new(move || {
                answer(
                    &tx,
                    || Msg::SkinFile(name.clone(), id, skin_library::read_skin_file(&path).ok()),
                    || Msg::SkinFile(name.clone(), id, None),
                )
            });
            match self.skins.reads.push("skins-files", job) {
                Ok(()) => {}
                Err(Refused::Full) => {
                    self.skins.files.get_mut().remove(&key);
                }
                Err(Refused::NoThread) => {
                    self.skins.files.get_mut().insert(key, File::Missing);
                }
            }
        }
    }

    /// The skin's texture, or why there is none: still loading (true), or not to be had.
    fn skin_texture(
        &self,
        uuid: &str,
        skin: &SkinPick,
        slim: bool,
    ) -> Result<(String, Rc<Texture>), bool> {
        let (key, png) = self.skin_png(uuid, skin, slim);
        let png = match png {
            Png::Ready(png) => png,
            Png::Pending => return Err(true),
            Png::Missing => return Err(false),
        };
        let texture = self
            .skins
            .texture(&key, &png, player_model::skin)
            .ok_or(false)?;
        Ok((key, texture))
    }

    fn cape_texture(&self, cape: &CapePick) -> Option<(String, Rc<Texture>)> {
        let CapePick::Minecraft(id) = cape else {
            return None;
        };
        let Some(Ok(profile)) = &self.skins.profile else {
            return None;
        };
        let url = &profile.capes.iter().find(|c| &c.id == id)?.url;
        let Some(Picture::Ready(png)) = self.skins.pictures.get(url) else {
            return None;
        };
        let texture = self.skins.texture(url, png, player_model::cape)?;
        Some((url.clone(), texture))
    }

    /// Sends the edits to the Minecraft services ([`look::save`]): the skin when it (or its
    /// arms) changed, then the cape when it did; nothing that wasn't edited. New arms alone are
    /// sent as such, so the Save keeps the picture the account wears when it starts.
    fn look_save(&mut self) {
        let offline = self.skins_offline();
        let Some(account) = self.look_account() else {
            return;
        };
        if offline
            || self.skins.uuid.as_deref() != Some(&account.uuid)
            || self.skins.saving.is_some()
            || self.skins.edit.is_empty()
        {
            return;
        }
        let worn = self.worn_look(&account);
        let (_, slim, _) = self.skins.chosen(&worn);
        let png = |skin: &SkinPick| match self.skin_png(&account.uuid, skin, slim).1 {
            Png::Ready(png) => Ok(png.as_ref().clone()),
            Png::Pending => Err(tr("This skin's picture isn't here yet.").to_string()),
            Png::Missing => {
                Err(tr("This skin's picture couldn't be loaded, so it can't be saved.").to_string())
            }
        };
        let picture = match self.skins.skin_sent(&worn) {
            None => None,
            Some(SkinSent::Arms(Some(url))) => {
                Some(Ok(look::Picture::Worn(look::WornPicture::Texture(url))))
            }
            Some(SkinSent::Arms(None)) => Some(
                png(&SkinPick::AccountDefault)
                    .map(|png| look::Picture::Worn(look::WornPicture::Default(png))),
            ),
            Some(SkinSent::Picture(skin)) => Some(png(&skin).map(|png| look::Picture::New {
                png,
                saved: match &skin {
                    SkinPick::Saved(id) => Some(id.clone()),
                    _ => None,
                },
            })),
        };
        let sent = match picture {
            None => None,
            Some(Ok(picture)) => Some(look::SkinSent { picture, slim }),
            Some(Err(why)) => {
                self.skins.said = vec![Err(why)];
                return;
            }
        };
        let Some(submitted) = self.skins.submit(offline) else {
            return;
        };
        let request = look::Request {
            uuid: account.uuid.clone(),
            skin: sent,
            cape: submitted.cape.clone().map(|cape| match cape {
                CapePick::Minecraft(id) => Some(id),
                CapePick::None => None,
            }),
        };
        // Pictures already here aren't fetched again; each one fetched is fetched once.
        let known: HashMap<String, Vec<u8>> = self
            .skins
            .pictures
            .iter()
            .filter_map(|(url, picture)| match picture {
                Picture::Ready(png) => Some((url.clone(), png.clone())),
                _ => None,
            })
            .collect();
        let (id, tx) = (submitted.id, self.tx.clone());
        let started = spawn("skins-save", move || {
            let uuid = account.uuid.clone();
            // Stopped unexpectedly, the Save may have changed something already: nothing it
            // asked for is sent again, and the profile is read again (`stopped_outcome`).
            let asked = (request.skin.is_some(), request.cape.is_some());
            answer(
                &tx,
                || {
                    let ep = account::Endpoints::live();
                    let token = account::token_for(
                        &ep,
                        account::client_id().as_deref(),
                        account::store(),
                        &account,
                    );
                    let known = RefCell::new(known);
                    let fetch = |url: &str| {
                        if let Some(png) = known.borrow().get(url) {
                            return Some(png.clone());
                        }
                        let png = capes::picture(url)?;
                        known.borrow_mut().insert(url.to_owned(), png.clone());
                        Some(png)
                    };
                    let record = |record: &look::Record| {
                        account::Accounts::save_look(
                            &account.uuid,
                            record.skin.clone(),
                            record.cape.clone(),
                        )
                    };
                    let outcome = look::save(
                        &request,
                        &look::Tools {
                            ep: &ep,
                            token,
                            library: skin_library::folder(&account.uuid),
                            fetch: &fetch,
                            record: &record,
                        },
                    );
                    Msg::LookSaved(account.uuid.clone(), id, Box::new(outcome))
                },
                || Msg::LookSaved(uuid, id, Box::new(stopped_outcome(asked))),
            )
        });
        if !started {
            self.skins.saving = None;
            self.skins.said = vec![Err(tr(
                "The launcher couldn't start saving. Nothing was sent: try again.",
            )
            .to_string())];
        }
    }

    /// A Save ended (`Msg::LookSaved`). It is reconciled with its own account's page, shown or
    /// not, and what it did is in that account's record (written by the Save itself, before it
    /// ended, so an account's records are written in the order of its Saves). In the offline
    /// view it starts nothing more.
    pub(crate) fn look_saved(&mut self, uuid: String, id: u64, outcome: look::Outcome) {
        let offline = self.skins_offline();
        if let Some(profile) = &outcome.profile {
            let skin = (outcome.skin == Some(Change::Done)).then(|| profile.skin.clone());
            let cape = (outcome.cape == Some(Change::Done))
                .then(|| profile.worn_cape().map(|c| c.url.clone()));
            if skin.is_some() || cape.is_some() {
                if let Some(a) = self.accounts.accounts.iter_mut().find(|a| a.uuid == uuid) {
                    if let Some(skin) = skin {
                        a.skin = skin;
                    }
                    if let Some(cape) = cape {
                        a.cape = cape;
                    }
                }
                if offline {
                    self.skins.heads_stale = true;
                } else {
                    self.load_profile_heads();
                }
            }
        }
        let (recorded, default_slim) = self.worn_inputs(&uuid);
        let Some(page) = self.skins.page_mut(&uuid) else {
            return;
        };
        if !page.save_answered(id, &outcome) {
            return;
        }
        let worn = page.worn(recorded.as_ref(), default_slim);
        page.settle(&worn);
        self.record_confirmed(&uuid);
        if offline || self.skins.uuid.as_deref() != Some(&uuid) {
            // Shown again later, online, the page reads the profile and the saved skins anew.
            return;
        }
        self.skins_list(&uuid);
        if !matches!(self.skins.profile, Some(Ok(_))) || !self.skins.unconfirmed.is_empty() {
            if let Some(account) = self.look_account() {
                self.profile_read(&account);
            }
        }
        self.after_profile();
    }

    /// Adds skin files (chosen or dropped) to the saved skins of the account shown, on a worker.
    fn skins_add(&mut self, paths: Vec<PathBuf>) {
        let Some(uuid) = self.skins.uuid.clone() else {
            return;
        };
        self.skins_add_for(uuid, paths);
    }

    fn skins_add_for(&mut self, uuid: String, paths: Vec<PathBuf>) {
        if self.skins_offline() || paths.is_empty() {
            return;
        }
        if let Some(page) = self.skins.page_mut(&uuid) {
            page.added.clear();
        }
        let (tx, account, id) = (self.tx.clone(), uuid.clone(), request_id());
        let job = Box::new(move || {
            answer(
                &tx,
                || {
                    let added = add_files(&account, &paths);
                    Msg::SkinsAdded(account.clone(), added, listing(&account, id))
                },
                || {
                    let listed = (id, Err(stopped()));
                    Msg::SkinsAdded(account.clone(), vec![Err(stopped())], listed)
                },
            )
        });
        match self.skins.writes.push("skins-index", job) {
            Ok(()) => self.skins.disk_writes += 1,
            Err(_) => {
                if let Some(page) = self.skins.page_mut(&uuid) {
                    page.added = vec![tr(
                        "The launcher is busy with your saved skins. Add them again in a moment.",
                    )
                    .to_string()];
                }
            }
        }
    }

    /// Skin files were added (`Msg::SkinsAdded`): those that couldn't be say so beside the
    /// skins, and the last one added is chosen, if the page still shows the account they were
    /// added for.
    pub(crate) fn skins_added(
        &mut self,
        uuid: String,
        added: Vec<Result<Saved, String>>,
        listed: skin_library::Listing,
    ) {
        self.skins.disk_writes = self.skins.disk_writes.saturating_sub(1);
        let shown = self.skins.uuid.as_deref() == Some(&uuid);
        let Some(page) = self.skins.page_mut(&uuid) else {
            return;
        };
        page.listing(listed);
        let last = page.files_added(added);
        if !shown {
            return;
        }
        if let Some(saved) = last {
            self.choose(|edit| {
                edit.skin = Some(SkinPick::Saved(saved.id.clone()));
                edit.slim = Some(saved.slim);
            });
        }
    }

    /// The file picker, for the account shown when it opened: what it picks goes to that
    /// account, even if another plays by the time it closes.
    fn skins_pick_file(&mut self, cx: &mut Context<Self>) {
        let Some(uuid) = self.skins.uuid.clone() else {
            return;
        };
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(tr("Add").into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this, cx| {
                    this.skins_add_for(uuid, paths);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Removes the saved skin `id` of the account shown, on the writes queue.
    fn skin_forget(&mut self, id: &str) {
        let Some(uuid) = self.skins.uuid.clone() else {
            return;
        };
        let (tx, account, skin, listed) =
            (self.tx.clone(), uuid.clone(), id.to_owned(), request_id());
        let job = Box::new(move || {
            answer(
                &tx,
                || {
                    let removed = skin_library::folder(&account)
                        .and_then(|dir| skin_library::remove(&dir, &skin));
                    Msg::SkinRemoved(
                        account.clone(),
                        skin.clone(),
                        removed,
                        listing(&account, listed),
                    )
                },
                || {
                    let after = (listed, Err(stopped()));
                    Msg::SkinRemoved(account.clone(), skin.clone(), Err(stopped()), after)
                },
            )
        });
        match self.skins.writes.push("skins-index", job) {
            Ok(()) => self.skins.disk_writes += 1,
            Err(_) => {
                self.skins.said = vec![Err(tr(
                    "The launcher is busy with your saved skins. Try again in a moment.",
                )
                .to_string())]
            }
        }
    }

    /// A saved skin was removed, or couldn't be (`Msg::SkinRemoved`).
    pub(crate) fn skin_removed(
        &mut self,
        uuid: String,
        id: String,
        removed: Result<(), String>,
        listed: skin_library::Listing,
    ) {
        self.skins.disk_writes = self.skins.disk_writes.saturating_sub(1);
        let shown = self.skins.uuid.as_deref() == Some(&uuid);
        let Some(page) = self.skins.page_mut(&uuid) else {
            return;
        };
        page.listing(listed);
        if let Err(why) = removed {
            page.said = vec![Err(why)];
            return;
        }
        page.removed(&id);
        let chosen = page.edit.skin == Some(SkinPick::Saved(id.clone()));
        if chosen && shown {
            self.choose(|edit| {
                edit.skin = None;
                edit.slim = None;
            });
        } else if chosen {
            page.edit.skin = None;
            page.edit.slim = None;
        }
    }

    pub(crate) fn skins_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let page = self.skins_view(window, cx);
        // The files the page asked for while it was drawn.
        self.skins_read_files();
        crate::scrollbar::wrap_page("skins-page", page, &self.theme)
    }

    fn skins_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let page = div()
            .id("skins-page")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .p(px(24.))
            .gap(px(20.))
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(px(tokens::TITLE))
                    .font_weight(FontWeight::BOLD)
                    .child(tr("Skins")),
            );
        let offline = self.skins_offline();
        let account = self.look_account();
        // Another account plays now (switched in the account menu while the page is open), or
        // another profile turned the offline developer view on or off.
        let other_account = account
            .as_ref()
            .is_some_and(|a| self.skins.uuid.as_deref() != Some(&a.uuid));
        if self.skins.opened_offline != Some(offline) || other_account {
            self.skins_open();
        }
        let account = match account {
            Some(account) if !offline => account,
            account => {
                // A player of the page's own, from the game files alone: in the offline developer
                // view the game's offline look (the account's default skin, Steve's with no
                // account), and Steve signed out. Nothing the page knows from online (a profile,
                // edits) is shown. With no game prepared there is no picture to show.
                let (uuid, skin, slim) = match account.as_ref().filter(|_| offline) {
                    Some(a) => (
                        a.uuid.clone(),
                        SkinPick::AccountDefault,
                        self.default_slim(&a.uuid),
                    ),
                    None => (String::new(), SkinPick::Default("steve"), false),
                };
                let preview = self
                    .skin_resources()
                    .is_some()
                    .then(|| self.look_preview(&uuid, (&skin, slim, None), true, window, cx));
                let beside = if offline {
                    crate::settings::hint(tr(
                        "Offline developer session: skins and capes can't be changed here.",
                    ))
                } else {
                    crate::home::card()
                    .flex_1()
                    .min_w(px(280.))
                    .p(px(20.))
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(12.))
                    .child(crate::settings::hint(tr(
                        "Sign in with the Microsoft account that owns Minecraft: Java Edition to choose its skin and cape.",
                    )))
                    .child(crate::frame::button("skins-sign-in", tr("Sign in with Microsoft")).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.sign_in_to_play();
                            cx.notify();
                        }),
                    ))
                };
                return self.turned_by_dragging(
                    page.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_start()
                            .gap(px(24.))
                            .children(preview)
                            .child(beside),
                    ),
                    cx,
                );
            }
        };
        let worn = self.worn_look(&account);
        let (skin, slim, cape) = self.skins.chosen(&worn);
        let preview = self.look_preview(
            &account.uuid,
            (&skin, slim, cape.as_ref()),
            false,
            window,
            cx,
        );
        let choices = div()
            .flex_1()
            .min_w(px(280.))
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(self.skin_choices(&account, &worn, &skin, slim, cx))
            .child(self.cape_choices(cape.as_ref(), cx));
        self.turned_by_dragging(
            page.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .gap(px(24.))
                    .child(preview)
                    .child(choices),
            ),
            cx,
        )
        // A skin file dropped anywhere on the page is saved and chosen.
        .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(rgb(palette().hover)))
        .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
            this.skins_add(paths.paths().to_vec());
            cx.stop_propagation();
            cx.notify();
        }))
    }

    /// A drag begun on the preview turns it wherever the pointer goes on `page`.
    fn turned_by_dragging(&self, page: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        page.on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
            let Some((from, yaw, pitch)) = this.skins.drag else {
                return;
            };
            if ev.pressed_button != Some(MouseButton::Left) {
                this.skins.drag = None;
                cx.notify();
                return;
            }
            let (dx, dy) = (
                f32::from(ev.position.x - from.x),
                f32::from(ev.position.y - from.y),
            );
            this.skins.yaw = yaw + dx * 0.012;
            this.skins.pitch = (pitch + dy * 0.008).clamp(-0.5, 0.6);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                if this.skins.drag.take().is_some() {
                    cx.notify();
                }
            }),
        )
    }

    /// The player in the chosen look, turned by dragging, with the arm model and Save under it.
    /// `look` is the skin, its arms and the cape, on account `uuid`'s page; `read_only` shows
    /// the preview alone, without the arm model or Save.
    fn look_preview(
        &mut self,
        uuid: &str,
        look: (&SkinPick, bool, Option<&CapePick>),
        read_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let (skin, slim, cape) = look;
        let p = palette();
        let texture = self.skin_texture(uuid, skin, slim);
        let cape = cape.and_then(|cape| self.cape_texture(cape));
        let image = texture.map(|(skin_key, skin)| {
            let (w, h) = (
                (PREVIEW_W * self.scale).round() as usize,
                (PREVIEW_H * self.scale).round() as usize,
            );
            let key = {
                use std::hash::{Hash, Hasher};
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                (
                    &skin_key,
                    slim,
                    cape.as_ref().map(|c| &c.0),
                    self.skins.yaw.to_bits(),
                    self.skins.pitch.to_bits(),
                    w,
                    h,
                )
                    .hash(&mut hash);
                hash.finish()
            };
            match &self.skins.shown {
                Some((shown, image)) if *shown == key => image.clone(),
                _ => {
                    let texels = player_model::render(
                        &Pose {
                            skin: &skin,
                            slim,
                            cape: cape.as_ref().map(|c| c.1.as_ref()),
                            yaw: self.skins.yaw,
                            pitch: self.skins.pitch,
                        },
                        w,
                        h,
                    );
                    let image = art::bake(&texels, w, h, 1. / self.scale, self.scale);
                    if let Some((_, old)) = self.skins.shown.replace((key, image.clone())) {
                        let _ = window.drop_image(old);
                    }
                    image
                }
            }
        });
        let stage = div()
            .id("skin-preview")
            .w(px(PREVIEW_W))
            .h(px(PREVIEW_H))
            .flex()
            .items_center()
            .justify_center()
            .cursor(if self.skins.drag.is_some() {
                gpui::CursorStyle::ClosedHand
            } else {
                gpui::CursorStyle::OpenHand
            })
            .map(|d| match image {
                Ok(image) => d.child(img(image).w(px(PREVIEW_W)).h(px(PREVIEW_H))),
                Err(true) => d.child(crate::settings::hint(tr("Loading your skin…"))),
                Err(false) => d.child(crate::settings::hint(tr(
                    "This skin's picture couldn't be loaded.",
                ))),
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.skins.drag = Some((ev.position, this.skins.yaw, this.skins.pitch));
                    cx.notify();
                }),
            );
        let card = crate::home::card()
            .flex_none()
            .w(px(PREVIEW_W + 32.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(stage)
            .child(
                div()
                    .text_center()
                    .child(crate::settings::hint(tr("Drag to turn"))),
            );
        if read_only {
            return card;
        }
        let arms = |wanted: bool, label: &'static str| {
            let on = slim == wanted;
            div()
                .id(label)
                .reach()
                .flex_1()
                .h(px(32.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(crate::tokens::r(crate::tokens::R_CONTROL))
                .text_size(px(tokens::SMALL))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(p.selected)))
                .when(!on, |d| {
                    d.text_color(rgb(p.muted)).hover(|s| s.bg(rgb(p.hover)))
                })
                .child(tr(label))
                // Only an edit: the saved skin takes these arms once it is worn with them.
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.choose(|edit| edit.slim = Some(wanted));
                    cx.notify();
                }))
        };
        let changed = !self.skins.edit.is_empty();
        // A Save also waits while what an earlier one confirmed is being recorded.
        let saving = self.skins.saving.is_some() || self.skins.recording > 0;
        let actions = div()
            .flex()
            .gap(px(8.))
            .child(
                crate::frame::primary(
                    "skins-save",
                    if saving { tr("Saving…") } else { tr("Save") },
                )
                .flex_1()
                .when(!changed || saving, |d| d.opacity(0.5).cursor_default())
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.look_save();
                    cx.notify();
                })),
            )
            .when(changed && !saving, |d| {
                d.child(
                    crate::frame::button("skins-undo", tr("Undo")).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.skins.edit = Edit::default();
                            this.skins.said.clear();
                            cx.notify();
                        },
                    )),
                )
            });
        card
            // The arms the skin is drawn for: wide (4 pixels, like Steve) or slim (3, like Alex),
            // named as the official launcher names them.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(tokens::SMALL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(p.muted))
                            .child(tr("Player model")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .p(px(3.))
                            .rounded(crate::tokens::r(crate::tokens::R_CONTROL))
                            .bg(rgb(p.field))
                            .child(arms(false, "Wide"))
                            .child(arms(true, "Slim")),
                    ),
            )
            .child(actions)
            .children(self.skins.said.iter().map(|line| match line.clone() {
                Ok(done) => crate::settings::hint(done),
                Err(why) => crate::settings::warning(why),
            }))
    }

    fn section(title: &str, about: &str) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(crate::home::heading(tr(title)))
            .when(!about.is_empty(), |d| {
                d.child(crate::settings::hint(tr(about)))
            })
    }

    /// One choice: a picture and its name (none for an empty one), the chosen one edged in the
    /// accent. A row's tiles stretch to its tallest, so a two-line name keeps them lined up.
    fn tile(
        &self,
        id: SharedString,
        picture: AnyElement,
        label: SharedString,
        chosen: bool,
    ) -> Stateful<Div> {
        let p = palette();
        div()
            .id(id.clone())
            .group(id)
            .reach_edge()
            .relative()
            .w(px(TILE_W))
            .p(px(10.))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.))
            .rounded(crate::tokens::r(crate::tokens::R_CARD))
            .border_1()
            .border_color(rgb(if chosen { p.accent } else { p.divider }))
            .bg(palette().card())
            .cursor_pointer()
            .when(!chosen, |d| d.hover(|s| s.bg(rgb(p.hover))))
            .child(
                div()
                    .w(px(PICTURE_W))
                    .h(px(PICTURE_H))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(picture),
            )
            .when(!label.is_empty(), |d| {
                d.child(
                    div()
                        .w_full()
                        .text_center()
                        .text_size(px(tokens::SMALL))
                        .font_weight(FontWeight::SEMIBOLD)
                        .line_height(px(17.))
                        .overflow_hidden()
                        .line_clamp(2)
                        .child(label),
                )
            })
    }

    /// A small picture of the model in `skin`, for its tile.
    fn skin_thumb(&self, uuid: &str, skin: &SkinPick, slim: bool) -> AnyElement {
        let Ok((key, texture)) = self.skin_texture(uuid, skin, slim) else {
            return div()
                .size_full()
                .rounded(crate::tokens::r(crate::tokens::R_SMALL))
                .bg(self.theme.well)
                .into_any_element();
        };
        let (w, h) = (
            (PICTURE_W * self.scale).round() as usize,
            (PICTURE_H * self.scale).round() as usize,
        );
        let image = self
            .skins
            .baked
            .borrow_mut()
            .entry((format!("{key}:thumb:{slim}"), self.scale.to_bits()))
            .or_insert_with(|| {
                let texels = player_model::render(
                    &Pose {
                        skin: &texture,
                        slim,
                        cape: None,
                        yaw: 0.45,
                        pitch: 0.1,
                    },
                    w,
                    h,
                );
                art::bake(&texels, w, h, 1. / self.scale, self.scale)
            })
            .clone();
        img(image)
            .w(px(PICTURE_W))
            .h(px(PICTURE_H))
            .into_any_element()
    }

    fn skin_choices(
        &mut self,
        account: &account::Account,
        worn: &Worn,
        chosen: &SkinPick,
        chosen_slim: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut tiles = div().flex().flex_wrap().gap(px(10.));
        let pick = |this: &mut Launcher, skin: SkinPick, slim: Option<bool>| {
            this.choose(|edit| {
                edit.skin = Some(skin);
                if let Some(slim) = slim {
                    edit.slim = Some(slim);
                }
            });
        };
        let worn_slim = worn.slim;
        let uuid = account.uuid.as_str();
        // The skin worn, while no saved skin keeps it: the picture it is now, so a click on it
        // during a Save stays that picture whatever the Save leaves worn.
        if matches!(worn.skin, SkinPick::Texture(_) | SkinPick::AccountDefault) {
            let skin = worn.skin.clone();
            tiles = tiles.child(
                self.tile(
                    "skin-worn".into(),
                    self.skin_thumb(uuid, &skin, worn_slim),
                    tr("Your skin").into(),
                    *chosen == skin,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    pick(this, skin.clone(), Some(worn_slim));
                    cx.notify();
                })),
            );
        }
        let mut history = self.skins.saved.clone();
        history.reverse();
        history.sort_by_key(|saved| SkinPick::Saved(saved.id.clone()) != worn.skin);
        for saved in history {
            let skin = SkinPick::Saved(saved.id.clone());
            let is_worn = skin == worn.skin;
            // The worn skin with the arms the profile wears it with, not the ones saved for it.
            let slim = if is_worn { worn_slim } else { saved.slim };
            let id = saved.id.clone();
            let group = SharedString::from(format!("skin-{}", saved.id));
            // Shown on hover, and on keyboard focus, where it is a Tab stop of its own.
            let forget = div()
                .id(SharedString::from(format!("skin-forget-{}", saved.id)))
                .reach()
                .absolute()
                .top(px(4.))
                .right(px(4.))
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(crate::tokens::r(crate::tokens::R_SMALL))
                .bg(rgb(palette().field))
                .opacity(0.)
                .group_hover(group.clone(), |s| s.opacity(1.))
                .focus_visible(|s| s.opacity(1.))
                .hover(|s| s.bg(rgb(palette().hover)))
                .child(blocks::icon("close", 12., self.theme.muted))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.skin_forget(&id);
                    cx.notify();
                }));
            tiles = tiles.child(
                self.tile(
                    group,
                    self.skin_thumb(uuid, &skin, slim),
                    saved.name.clone().into(),
                    *chosen == skin,
                )
                // The worn skin can't be forgotten: it would be kept again at once.
                .when(!is_worn, |d| d.child(forget))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    pick(this, skin.clone(), Some(slim));
                    cx.notify();
                })),
            );
        }
        let p = palette();
        tiles = tiles.child(
            div()
                .id("skin-add")
                .reach_edge()
                .w(px(TILE_W))
                // As tall as a tile's picture, or its row's tallest tile.
                .min_h(px(PICTURE_H + 20.))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .rounded(crate::tokens::r(crate::tokens::R_CARD))
                .border_1()
                .border_dashed()
                .border_color(rgb(p.divider))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(p.hover)))
                .child(blocks::icon("plus", 20., self.theme.muted))
                .child(
                    div()
                        .text_size(px(tokens::SMALL))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(tr("Add skin")),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.skins_pick_file(cx);
                })),
        );
        let defaults = self
            .skin_resources_found()
            .map(|(_, defaults)| defaults)
            .unwrap_or_default();
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(Self::section(
                "Skin",
                "Add a skin from a PNG file, or drop one here. Saved skins stay on this computer.",
            ))
            .child(tiles)
            // Files that couldn't be added, beside the one that was (chosen, with its tile).
            .children(
                self.skins
                    .added
                    .iter()
                    .map(|why| crate::settings::warning(why.clone())),
            )
            .when_some(self.skins.library_error.clone(), |d, why| {
                d.child(crate::settings::warning(why))
            })
            .when_some(self.skins.keep_error.clone(), |d, why| {
                d.child(crate::settings::warning(why))
            });
        if !defaults.is_empty() {
            let mut grid = div().flex().flex_wrap().gap(px(10.));
            for name in defaults {
                let skin = SkinPick::Default(name);
                let title: String = name[..1].to_uppercase() + &name[1..];
                grid = grid.child(
                    self.tile(
                        SharedString::from(format!("skin-default-{name}")),
                        self.skin_thumb(uuid, &skin, chosen_slim),
                        title.into(),
                        *chosen == skin,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            pick(this, skin.clone(), None);
                            cx.notify();
                        },
                    )),
                );
            }
            section = section
                .child(Self::section("Default skins", ""))
                .child(grid);
        }
        section
    }

    fn cape_choices(&mut self, chosen: Option<&CapePick>, cx: &mut Context<Self>) -> Div {
        let section = div().flex().flex_col().gap(px(12.)).child(Self::section(
            "Cape",
            "The capes your account owns. Everyone sees the one you wear, in every launcher and on every server.",
        ));
        // No cape is a choice even while the account's capes can't be listed.
        let (list, problem) = match &self.skins.profile {
            None => (
                Vec::new(),
                Some(crate::settings::hint(tr("Looking up your capes…"))),
            ),
            Some(Err(_)) if self.skins.reading.is_some() => (
                Vec::new(),
                Some(crate::settings::hint(tr("Looking up your capes…"))),
            ),
            Some(Err(why)) => {
                let needs_sign_in = self.look_account().is_some_and(|a| {
                    self.auth_status
                        .get(&a.uuid)
                        .is_some_and(|s| s.state == crate::account::session::State::SignInRequired)
                });
                let action = if needs_sign_in {
                    crate::frame::button("capes-sign-in", tr("Sign in again")).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(account) = this.look_account() {
                                this.re_sign_in(&account.uuid);
                            }
                            cx.notify();
                        }),
                    )
                } else {
                    crate::frame::button("capes-retry", tr("Try again")).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            if let Some(account) = this.look_account() {
                                this.profile_read(&account);
                            }
                            cx.notify();
                        },
                    ))
                };
                (
                    Vec::new(),
                    Some(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(12.))
                            .child(crate::settings::warning(why.clone()))
                            .child(action),
                    ),
                )
            }
            Some(Ok(profile)) => (profile.capes.clone(), None),
        };
        let mut choices: Vec<(CapePick, String)> = vec![(CapePick::None, tr("No cape").into())];
        choices.extend(
            list.iter()
                .map(|c| (CapePick::Minecraft(c.id.clone()), c.alias.clone())),
        );
        let mut tiles = div().flex().flex_wrap().gap(px(10.));
        for (cape, label) in choices {
            let face = self.cape_texture(&cape).map(|(key, texture)| {
                let front = player_model::cape_front(&texture);
                let image = self
                    .skins
                    .baked
                    .borrow_mut()
                    .entry((format!("{key}:front"), self.scale.to_bits()))
                    .or_insert_with(|| {
                        art::bake(
                            &front.texels,
                            front.w,
                            front.h,
                            PICTURE_W / front.w as f32,
                            self.scale,
                        )
                    })
                    .clone();
                img(image).w(px(PICTURE_W)).h(px(PICTURE_H))
            });
            let picture = div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .rounded(crate::tokens::r(crate::tokens::R_SMALL))
                .bg(self.theme.well)
                .map(|d| match face {
                    Some(face) => d.child(face),
                    None if cape == CapePick::None => {
                        d.child(blocks::icon("close", 22., self.theme.muted))
                    }
                    None => d,
                })
                .into_any_element();
            let id = SharedString::from(match &cape {
                CapePick::None => "cape-none".to_string(),
                CapePick::Minecraft(id) => format!("cape-{id}"),
            });
            tiles = tiles.child(
                self.tile(id, picture, label.into(), chosen == Some(&cape))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let cape = cape.clone();
                        this.choose(|edit| edit.cape = Some(cape));
                        cx.notify();
                    })),
            );
        }
        section
            .child(tiles)
            .when_some(problem, |d, problem| d.child(problem))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_launcher::account::capes::Cape;

    const SKIN: &str = "http://textures.minecraft.net/texture/worn";
    const ALEX: &str = "http://textures.minecraft.net/texture/alex";
    const KEPT: &str = "0123456789abcdef";

    fn cape(id: &str, active: bool) -> Cape {
        Cape {
            id: id.into(),
            alias: id.into(),
            url: format!("http://textures.minecraft.net/texture/{id}"),
            active,
        }
    }

    fn profile(skin: &str, slim: bool, worn_cape: Option<&str>) -> Profile {
        Profile {
            id: "069a79f444e94726a5befca90e38aaf5".into(),
            skin: Some((skin.into(), if slim { "slim" } else { "classic" }.into())),
            capes: ["c1", "c2"]
                .into_iter()
                .map(|id| cape(id, worn_cape == Some(id)))
                .collect(),
        }
    }

    fn worn(skins: &Skins) -> Worn {
        skins.worn(None, false)
    }

    /// Chooses on the page the way a tile does: the edit, then forgetting what is worn already.
    fn choose(skins: &mut Skins, change: impl FnOnce(&mut Edit)) {
        let worn = worn(skins);
        skins.choose(change, &worn);
    }

    fn pick(skins: &mut Skins, skin: SkinPick) {
        choose(skins, |edit| edit.skin = Some(skin));
    }

    /// A Save answered and settled, as `Launcher::look_saved` does it.
    fn answer_save(skins: &mut Skins, id: u64, outcome: &look::Outcome) -> bool {
        let answered = skins.save_answered(id, outcome);
        let worn = worn(skins);
        skins.settle(&worn);
        answered
    }

    fn outcome(
        skin: Option<Change>,
        cape: Option<Change>,
        profile: Option<Profile>,
    ) -> look::Outcome {
        look::Outcome {
            kept: Kept::Nothing,
            skin,
            cape,
            profile,
            arms: None,
            recorded: None,
            uploaded: None,
        }
    }

    fn kept(url: &str) -> Kept {
        Kept::Kept {
            url: url.into(),
            saved: Saved {
                id: KEPT.into(),
                name: String::new(),
                slim: false,
            },
        }
    }

    fn read(skins: &mut Skins, profile: Profile) {
        let id = skins.read_asked();
        assert!(skins.read_answered("a", id, Ok(profile), false));
    }

    #[test]
    fn a_skin_picked_while_the_capes_load_sends_no_cape_change() {
        let mut skins = Skins::new_for("a");
        let id = skins.read_asked();
        pick(&mut skins, SkinPick::Default("alex"));
        assert_eq!(worn(&skins).cape, None, "the worn cape isn't known yet");
        assert!(skins.read_answered("a", id, Ok(profile(SKIN, false, Some("c2"))), false));
        let submitted = skins.submit(false).unwrap();
        assert_eq!(submitted.skin, Some(SkinPick::Default("alex")));
        assert_eq!(submitted.cape, None, "no DELETE nobody asked for");
    }

    #[test]
    fn a_save_that_ends_before_the_reading_keeps_its_answer() {
        let mut skins = Skins::new_for("a");
        let reading = skins.read_asked();
        pick(&mut skins, SkinPick::Default("alex"));
        let submitted = skins.submit(false).unwrap();
        let after = profile(ALEX, false, Some("c2"));
        assert!(answer_save(
            &mut skins,
            submitted.id,
            &outcome(Some(Change::Done), None, Some(after.clone()))
        ));
        assert!(skins.said.iter().all(Result::is_ok), "{:?}", skins.said);
        // The reading asked before the Save says what was worn before it: it changes nothing.
        assert!(!skins.read_answered("a", reading, Ok(profile(SKIN, false, None)), false));
        assert_eq!(skins.profile, Some(Ok(after)));
        assert!(skins.edit.is_empty());
    }

    #[test]
    fn a_reading_answered_during_a_save_leaves_it_alone() {
        let mut skins = Skins::new_for("a");
        let reading = skins.read_asked();
        skins.edit.cape = Some(CapePick::Minecraft("c1".into()));
        let submitted = skins.submit(false).unwrap();
        assert!(!skins.read_answered("a", reading, Ok(profile(SKIN, false, Some("c2"))), false));
        assert_eq!(skins.saving.as_ref().map(|s| s.id), Some(submitted.id));
        let after = profile(SKIN, false, Some("c1"));
        assert!(answer_save(
            &mut skins,
            submitted.id,
            &outcome(None, Some(Change::Done), Some(after.clone()))
        ));
        assert_eq!(skins.profile, Some(Ok(after)));
    }

    #[test]
    fn an_explicit_no_cape_is_sent_and_an_unknown_one_is_not_taken_for_it() {
        let mut skins = Skins::new_for("a");
        choose(&mut skins, |edit| edit.cape = Some(CapePick::None));
        assert_eq!(skins.edit.cape, Some(CapePick::None), "kept while unknown");
        assert_eq!(skins.submit(false).unwrap().cape, Some(CapePick::None));
    }

    #[test]
    fn a_partial_save_says_each_change_and_leaves_only_the_failed_one_to_do() {
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c2")));
        pick(&mut skins, SkinPick::Default("alex"));
        choose(&mut skins, |edit| {
            edit.cape = Some(CapePick::Minecraft("c1".into()))
        });
        let submitted = skins.submit(false).unwrap();
        let after = profile(ALEX, false, Some("c2"));
        answer_save(
            &mut skins,
            submitted.id,
            &outcome(
                Some(Change::Done),
                Some(Change::Failed("Too many cape changes.".into())),
                Some(after.clone()),
            ),
        );
        assert_eq!(skins.said.len(), 2, "{:?}", skins.said);
        assert!(skins.said[0].is_ok(), "the skin: {:?}", skins.said[0]);
        let cape = skins.said[1].clone().unwrap_err();
        assert!(cape.contains("Too many cape changes."), "{cape}");
        assert_eq!(
            skins.profile,
            Some(Ok(after)),
            "the new skin is the baseline"
        );
        assert_eq!(skins.edit.skin, None, "the skin is done: no second upload");
        assert_eq!(skins.edit.cape, Some(CapePick::Minecraft("c1".into())));
        let again = skins.submit(false).unwrap();
        assert_eq!((again.skin, again.slim), (None, None));
        assert_eq!(again.cape, Some(CapePick::Minecraft("c1".into())));
    }

    #[test]
    fn an_unconfirmed_half_of_a_save_is_not_called_saved() {
        let mut lines = said(&outcome(
            Some(Change::Done),
            Some(Change::Unconfirmed("busy".into())),
            None,
        ));
        assert!(lines.remove(0).is_ok(), "the skin went through");
        let cape = lines.remove(0).unwrap_err();
        assert!(
            cape.contains("isn't confirmed") && cape.contains("busy"),
            "{cape}"
        );
        assert!(lines.is_empty());
        let lines = said(&outcome(
            Some(Change::Unconfirmed("busy".into())),
            None,
            None,
        ));
        assert!(lines[0].is_err(), "{lines:?}");
    }

    #[test]
    fn a_failed_save_keeps_every_edit() {
        let mut skins = Skins::new_for("a");
        pick(&mut skins, SkinPick::Default("alex"));
        let submitted = skins.submit(false).unwrap();
        answer_save(
            &mut skins,
            submitted.id,
            &outcome(Some(Change::Failed("Sign in again.".into())), None, None),
        );
        let said = skins.said[0].clone().unwrap_err();
        assert!(said.contains("Sign in again."), "{said}");
        assert_eq!(skins.edit.skin, Some(SkinPick::Default("alex")));
        assert!(skins.saving.is_none(), "Save can be tried again");
    }

    #[test]
    fn a_skin_not_replaced_because_its_predecessor_couldnt_be_kept_says_so() {
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, None));
        pick(&mut skins, SkinPick::Default("alex"));
        let submitted = skins.submit(false).unwrap();
        let mut failed = outcome(
            Some(Change::Failed("no picture".into())),
            None,
            Some(profile(SKIN, false, None)),
        );
        failed.kept = Kept::Failed("no picture".into());
        answer_save(&mut skins, submitted.id, &failed);
        let said = skins.said[0].clone().unwrap_err();
        assert!(
            said.contains("couldn't be kept") && said.contains("no picture"),
            "{said}"
        );
        assert_eq!(
            skins.edit.skin,
            Some(SkinPick::Default("alex")),
            "still to do"
        );
    }

    #[test]
    fn a_choice_made_during_a_save_outlives_it() {
        let mut skins = Skins::new_for("a");
        pick(&mut skins, SkinPick::Default("alex"));
        let submitted = skins.submit(false).unwrap();
        pick(&mut skins, SkinPick::Default("steve"));
        answer_save(
            &mut skins,
            submitted.id,
            &outcome(Some(Change::Done), None, Some(profile(ALEX, false, None))),
        );
        assert_eq!(skins.edit.skin, Some(SkinPick::Default("steve")));
    }

    #[test]
    fn the_worn_cape_clicked_while_another_saves_is_kept() {
        // c1 is worn; c2 is being saved; the player clicks c1 again.
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c1")));
        choose(&mut skins, |edit| {
            edit.cape = Some(CapePick::Minecraft("c2".into()))
        });
        let submitted = skins.submit(false).unwrap();
        choose(&mut skins, |edit| {
            edit.cape = Some(CapePick::Minecraft("c1".into()))
        });
        assert_eq!(
            skins.edit.cape,
            Some(CapePick::Minecraft("c1".into())),
            "not dropped as a no-op against the profile from before the Save"
        );
        let (_, _, shown) = skins.chosen(&worn(&skins));
        assert_eq!(
            shown,
            Some(CapePick::Minecraft("c1".into())),
            "the preview shows it"
        );
        // c2 went on: c1 is still to do, and the next Save sends it.
        answer_save(
            &mut skins,
            submitted.id,
            &outcome(
                None,
                Some(Change::Done),
                Some(profile(SKIN, false, Some("c2"))),
            ),
        );
        assert_eq!(skins.edit.cape, Some(CapePick::Minecraft("c1".into())));
        assert_eq!(
            skins.submit(false).unwrap().cape,
            Some(CapePick::Minecraft("c1".into()))
        );

        // The same click when c2 was refused: c1 is what is worn, so nothing is left to do.
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c1")));
        choose(&mut skins, |edit| {
            edit.cape = Some(CapePick::Minecraft("c2".into()))
        });
        let submitted = skins.submit(false).unwrap();
        choose(&mut skins, |edit| {
            edit.cape = Some(CapePick::Minecraft("c1".into()))
        });
        answer_save(
            &mut skins,
            submitted.id,
            &outcome(
                None,
                Some(Change::Failed("busy".into())),
                Some(profile(SKIN, false, Some("c1"))),
            ),
        );
        assert!(skins.edit.is_empty());
    }

    #[test]
    fn the_worn_skin_clicked_during_a_save_stays_the_picture_clicked() {
        // The default skin is worn; Alex is being saved; the player clicks "Your skin". The
        // Save keeps nothing (a default skin isn't kept): the click is still the default skin,
        // not dropped, and the next Save sends it.
        let mut skins = Skins::new_for("a");
        skins.profile = Some(Ok(Profile {
            skin: None,
            ..profile(SKIN, false, None)
        }));
        assert_eq!(worn(&skins).skin, SkinPick::AccountDefault);
        pick(&mut skins, SkinPick::Default("alex"));
        let submitted = skins.submit(false).unwrap();
        pick(&mut skins, SkinPick::AccountDefault);
        answer_save(
            &mut skins,
            submitted.id,
            &outcome(Some(Change::Done), None, Some(profile(ALEX, false, None))),
        );
        assert_eq!(skins.edit.skin, Some(SkinPick::AccountDefault));
        assert_eq!(
            skins.submit(false).unwrap().skin,
            Some(SkinPick::AccountDefault)
        );

        // `worn` is worn and shown; the player clicks it while Alex saves. The Save keeps it:
        // the click is that saved skin.
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, None));
        pick(&mut skins, SkinPick::Default("alex"));
        let submitted = skins.submit(false).unwrap();
        pick(&mut skins, SkinPick::Texture(SKIN.into()));
        let mut done = outcome(Some(Change::Done), None, Some(profile(ALEX, false, None)));
        done.kept = kept(SKIN);
        answer_save(&mut skins, submitted.id, &done);
        assert_eq!(skins.edit.skin, Some(SkinPick::Saved(KEPT.into())));

        // A stale "Your skin" (the page showed `worn`, the Save found and kept another): the
        // click is still `worn`'s picture, not the one the Save kept.
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, None));
        pick(&mut skins, SkinPick::Default("alex"));
        let submitted = skins.submit(false).unwrap();
        pick(&mut skins, SkinPick::Texture(SKIN.into()));
        let mut done = outcome(Some(Change::Done), None, Some(profile(ALEX, false, None)));
        done.kept = kept("http://textures.minecraft.net/texture/other");
        answer_save(&mut skins, submitted.id, &done);
        assert_eq!(skins.edit.skin, Some(SkinPick::Texture(SKIN.into())));
    }

    #[test]
    fn new_arms_alone_are_sent_as_such_with_the_skin_the_page_shows() {
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, None));
        choose(&mut skins, |edit| edit.slim = Some(true));
        assert_eq!(
            skins.skin_sent(&worn(&skins)),
            Some(SkinSent::Arms(Some(SKIN.into()))),
            "the worker checks `worn` is still worn, and sends its own copy of it"
        );
        // Kept among the saved skins, it is still the skin worn.
        skins.kept.insert(SKIN.into(), KEPT.into());
        assert_eq!(
            skins.skin_sent(&worn(&skins)),
            Some(SkinSent::Arms(Some(SKIN.into())))
        );
        // Another picture is sent as a picture.
        pick(&mut skins, SkinPick::Default("alex"));
        assert_eq!(
            skins.skin_sent(&worn(&skins)),
            Some(SkinSent::Picture(SkinPick::Default("alex")))
        );
        // The account's default skin.
        let mut skins = Skins::new_for("a");
        skins.profile = Some(Ok(Profile {
            skin: None,
            ..profile(SKIN, false, None)
        }));
        choose(&mut skins, |edit| edit.slim = Some(true));
        assert_eq!(skins.skin_sent(&worn(&skins)), Some(SkinSent::Arms(None)));
        // While an accepted skin stands in for the profile's, it is sent as the picture it is.
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, None));
        skins.unconfirmed.skin = Some(SkinPick::Default("alex"));
        choose(&mut skins, |edit| edit.slim = Some(true));
        assert_eq!(
            skins.skin_sent(&worn(&skins)),
            Some(SkinSent::Picture(SkinPick::Default("alex")))
        );
        // No skin edit: no skin sent.
        skins.edit = Edit {
            cape: Some(CapePick::None),
            ..Edit::default()
        };
        assert_eq!(skins.skin_sent(&worn(&skins)), None);
    }

    #[test]
    fn a_save_during_a_switch_away_and_back_stays_with_its_account() {
        let mut pages = SkinsPages::default();
        let signed_in = |_: &str| true;
        pages.show("a", signed_in);
        let old_read = pages.read_asked();
        pick(&mut pages, SkinPick::Default("alex"));
        let save = pages.submit(false).unwrap();
        // A → B → A while the Save runs.
        pages.show("b", signed_in);
        assert_eq!(pages.uuid.as_deref(), Some("b"));
        assert!(pages.saving.is_none(), "B has a page of its own");
        pages.show("a", signed_in);
        assert_eq!(
            pages.saving.as_ref().map(|s| s.id),
            Some(save.id),
            "A's Save is still under way"
        );
        assert_eq!(pages.edit.skin, Some(SkinPick::Default("alex")));
        assert!(
            pages.submit(false).is_none(),
            "no second write while the first runs"
        );
        // A reading from before the switch is still no answer to a newer one.
        let read = pages.read_asked();
        assert_ne!(read, old_read);
        assert!(!pages.read_answered("a", old_read, Ok(profile(SKIN, false, None)), false));
        assert!(!pages.read_answered("b", read, Ok(profile(SKIN, false, None)), false));
        // The Save ends while B is shown: A's page is reconciled, B's left alone.
        pages.show("b", signed_in);
        assert!(pages.writing(), "A's Save, though B is shown");
        let after = profile(ALEX, false, Some("c1"));
        let done = outcome(Some(Change::Done), None, Some(after.clone()));
        assert!(!pages.save_answered(save.id, &done), "not B's Save");
        assert!(answer_save(pages.page_mut("a").unwrap(), save.id, &done));
        assert!(!pages.writing());
        assert!(
            pages.profile.is_none(),
            "B's page knows nothing of A's profile"
        );
        pages.show("a", signed_in);
        assert_eq!(pages.profile, Some(Ok(after)));
        assert!(pages.saving.is_none());
        assert!(pages.edit.is_empty(), "done, not sent again");
        assert!(pages.said.iter().all(Result::is_ok));
    }

    #[test]
    fn a_data_move_waits_for_skins_being_written() {
        let mut pages = SkinsPages::default();
        pages.show("a", |_| true);
        assert!(!pages.writing());
        pages.disk_writes += 1;
        assert!(pages.writing(), "an import or a kept skin");
        pages.disk_writes -= 1;
        pick(&mut pages, SkinPick::Default("alex"));
        pages.submit(false).unwrap();
        assert!(pages.writing(), "a Save");
    }

    #[test]
    fn a_page_is_forgotten_with_its_account_but_not_during_a_save() {
        let mut pages = SkinsPages::default();
        pages.show("a", |_| true);
        pick(&mut pages, SkinPick::Default("alex"));
        pages.submit(false).unwrap();
        pages.show("b", |_| true);
        pages.show("c", |uuid| uuid != "a");
        assert!(
            pages.page_mut("a").is_some(),
            "its Save still to be answered"
        );
        pages.page_mut("a").unwrap().saving = None;
        pages.show("b", |uuid| uuid != "a");
        assert!(pages.page_mut("a").is_none());
        assert!(pages.page_mut("c").is_some());
    }

    #[test]
    fn a_picture_reaches_the_page_that_asked_for_it_while_another_is_shown() {
        let mut pages = SkinsPages::default();
        pages.show("a", |_| true);
        assert!(pages.ask_picture(SKIN, false));
        pages.show("b", |_| true);
        pages.picture(SKIN, Some(vec![1, 2, 3]));
        assert!(!pages.pictures.contains_key(SKIN), "B didn't ask for it");
        pages.show("a", |_| true);
        assert_eq!(
            pages.pictures.get(SKIN),
            Some(&Picture::Ready(vec![1, 2, 3]))
        );
    }

    #[test]
    fn a_picture_that_couldnt_be_had_is_asked_for_again() {
        let mut skins = Skins::new_for("a");
        assert!(skins.ask_picture(SKIN, false));
        assert!(!skins.ask_picture(SKIN, false), "on its way already");
        assert!(skins.unasked([&SKIN.to_owned()]).is_empty());
        skins.picture(SKIN.into(), None);
        assert!(matches!(skins.picture_png(SKIN), Png::Missing));
        // The next reading of the profile asks for it again.
        assert_eq!(skins.unasked([&SKIN.to_owned()]), [SKIN]);
        assert!(skins.ask_picture(SKIN, false));
        skins.picture(SKIN.into(), Some(vec![1]));
        assert!(!skins.ask_picture(SKIN, false), "here now");
        assert!(skins.unasked([&SKIN.to_owned()]).is_empty());
    }

    #[test]
    fn a_file_is_read_once_and_only_the_latest_read_of_it_counts() {
        let mut pages = SkinsPages::default();
        pages.show("a", |_| true);
        let path = || Some(PathBuf::from("x.png"));
        let asked = |pages: &SkinsPages, key: &str| {
            pages
                .wanted
                .borrow()
                .iter()
                .rev()
                .find(|(k, _, _)| k == key)
                .map(|(_, _, id)| *id)
                .unwrap()
        };
        assert!(matches!(pages.file("saved:x", path), Png::Pending));
        assert!(matches!(pages.file("saved:x", path), Png::Pending));
        assert_eq!(pages.wanted.borrow().len(), 1, "read once");
        assert!(matches!(pages.file("saved:none", || None), Png::Missing));
        let x = asked(&pages, "saved:x");
        pages.file_read("saved:x", x, Some(vec![7]));
        assert!(matches!(pages.file("saved:x", path), Png::Ready(png) if *png == [7]));
        // One that couldn't be read is missing, and read again once forgotten.
        assert!(matches!(pages.file("saved:y", path), Png::Pending));
        let first = asked(&pages, "saved:y");
        pages.file_read("saved:y", first, None);
        assert!(matches!(pages.file("saved:y", path), Png::Missing));
        // Forgotten while a read is under way (its picture written anew): the read from before
        // is no answer to the one after.
        pages.forget_file("saved:y");
        assert!(matches!(pages.file("saved:y", path), Png::Pending));
        let stale = asked(&pages, "saved:y");
        pages.forget_file("saved:y");
        assert!(matches!(pages.file("saved:y", path), Png::Pending));
        let latest = asked(&pages, "saved:y");
        pages.file_read("saved:y", stale, Some(vec![1]));
        assert!(matches!(pages.file("saved:y", path), Png::Pending));
        pages.file_read("saved:y", latest, Some(vec![2]));
        assert!(matches!(pages.file("saved:y", path), Png::Ready(png) if *png == [2]));
        // Each account's saved skins are its own files.
        assert_ne!(saved_key(Some("a"), "x"), saved_key(Some("b"), "x"));
    }

    #[test]
    fn a_queue_holds_a_bounded_amount_of_work_on_one_thread() {
        let mut queue = Queue::default();
        let (release, held) = mpsc::channel::<()>();
        let (done, finished) = mpsc::channel::<usize>();
        // The first job holds the thread until released; the queue fills behind it.
        queue
            .push(
                "test-queue",
                Box::new(move || {
                    let _ = held.recv();
                }),
            )
            .unwrap();
        let mut taken = 0;
        let mut refused = 0;
        for n in 0..QUEUED + 8 {
            let done = done.clone();
            match queue.push("test-queue", Box::new(move || done.send(n).unwrap())) {
                Ok(()) => taken += 1,
                Err(Refused::Full) => refused += 1,
                Err(Refused::NoThread) => panic!("the thread is there"),
            }
        }
        assert!((QUEUED - 1..=QUEUED).contains(&taken), "{taken}");
        assert!(refused >= 8, "refused, not queued without end: {refused}");
        // A job that panics doesn't take the queue down with it.
        release.send(()).unwrap();
        for _ in 0..taken {
            finished.recv().unwrap();
        }
        queue
            .push("test-queue", Box::new(|| panic!("a bad job")))
            .unwrap();
        let done2 = done.clone();
        queue
            .push("test-queue", Box::new(move || done2.send(999).unwrap()))
            .unwrap();
        assert_eq!(finished.recv().unwrap(), 999);
    }

    #[test]
    fn a_failed_import_stays_beside_the_skin_added_with_it() {
        let mut skins = Skins::new_for("a");
        let good = Saved {
            id: KEPT.into(),
            name: "Knight".into(),
            slim: false,
        };
        let last = skins.files_added(vec![
            Err("big.png couldn't be added: too big".into()),
            Ok(good.clone()),
        ]);
        assert_eq!(last, Some(good.clone()));
        // The good one is chosen, as `Launcher::skins_added` does: the failure stays.
        choose(&mut skins, |edit| {
            edit.skin = Some(SkinPick::Saved(good.id))
        });
        assert_eq!(skins.added, ["big.png couldn't be added: too big"]);
        // The next import starts afresh.
        assert_eq!(skins.files_added(Vec::new()), None);
        assert!(skins.added.is_empty());
    }

    #[test]
    fn an_accepted_but_unconfirmed_save_is_not_sent_again() {
        // The upload got a 204 and the reading after it failed.
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c1")));
        pick(&mut skins, SkinPick::Default("alex"));
        choose(&mut skins, |edit| edit.cape = Some(CapePick::None));
        let submitted = skins.submit(false).unwrap();
        answer_save(
            &mut skins,
            submitted.id,
            &outcome(
                Some(Change::Unconfirmed("busy".into())),
                Some(Change::Unconfirmed("busy".into())),
                Some(profile(SKIN, false, Some("c1"))),
            ),
        );
        assert_eq!(skins.said.len(), 2);
        assert!(
            skins.said.iter().all(Result::is_err),
            "not called saved: {:?}",
            skins.said
        );
        assert!(skins.edit.is_empty(), "out of the resend path");
        assert!(skins.submit(false).is_none(), "Save has nothing to send");
        // Until a reading says otherwise, the account is taken to wear what was accepted.
        let shown = worn(&skins);
        assert_eq!(shown.skin, SkinPick::Default("alex"));
        assert_eq!(shown.cape, Some(CapePick::None));
        // Clicking what was worn before is a change again, and is sent.
        choose(&mut skins, |edit| {
            edit.cape = Some(CapePick::Minecraft("c1".into()))
        });
        assert_eq!(skins.edit.cape, Some(CapePick::Minecraft("c1".into())));
        skins.edit = Edit::default();
        // The next reading is the truth.
        read(&mut skins, profile(ALEX, false, None));
        assert!(skins.unconfirmed.is_empty());
        assert_eq!(worn(&skins).skin, SkinPick::Texture(ALEX.into()));
    }

    #[test]
    fn offline_now_the_page_takes_no_answer_asks_nothing_and_sends_nothing() {
        // Same account: a reading asked online, answered once the offline view is shown.
        let mut skins = Skins::new_for("a");
        let reading = skins.read_asked();
        assert!(!skins.read_answered("a", reading, Ok(profile(SKIN, false, Some("c1"))), true));
        assert_eq!(skins.profile, None);
        assert!(
            !skins.ask_picture(SKIN, true),
            "no texture requests offline"
        );
        skins.edit.skin = Some(SkinPick::Default("alex"));
        assert!(skins.submit(true).is_none(), "no edits are sent");
        // Online again, all of it works.
        assert!(skins.ask_picture(SKIN, false));
        assert!(skins.submit(false).is_some());
    }

    #[test]
    fn the_worn_look_comes_from_one_reading_of_the_profile() {
        let mut skins = Skins::new_for("a");
        let recorded = (SKIN.to_owned(), "slim".to_owned());
        assert_eq!(
            skins.worn(Some(&recorded), false),
            Worn {
                skin: SkinPick::Texture(SKIN.into()),
                slim: true,
                cape: None,
                texture: Some(SKIN.into()),
            }
        );
        read(
            &mut skins,
            profile(
                "http://textures.minecraft.net/texture/new",
                false,
                Some("c1"),
            ),
        );
        let worn = skins.worn(Some(&recorded), false);
        assert!(!worn.slim);
        assert_eq!(worn.cape, Some(CapePick::Minecraft("c1".into())));
        let again = skins.read_asked();
        skins.read_answered("a", again, Err("offline".into()), false);
        assert!(matches!(skins.profile, Some(Ok(_))));
    }

    #[test]
    fn the_worn_skins_tile_uses_the_profiles_arms() {
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, true, None));
        skins.kept.insert(SKIN.into(), KEPT.into());
        let worn = worn(&skins);
        assert_eq!(worn.skin, SkinPick::Saved(KEPT.into()));
        skins.edit.skin = Some(worn.skin.clone());
        skins.edit.slim = Some(worn.slim);
        skins.settle(&worn);
        assert!(skins.edit.is_empty());
        // Removed from the saved skins, it is the skin at its URL again.
        skins.removed(KEPT);
        assert_eq!(
            super::tests::worn(&skins).skin,
            SkinPick::Texture(SKIN.into())
        );
    }

    #[test]
    fn only_regular_files_are_read_as_skins() {
        let dir = tempfile::tempdir().unwrap();
        let added = add_files(
            "069a79f444e94726a5befca90e38aaf5",
            &[dir.path().to_path_buf()],
        );
        let why = added[0].clone().unwrap_err();
        let name = dir.path().file_name().unwrap().to_string_lossy();
        assert!(why.contains(name.as_ref()), "named: {why}");
        assert!(add_files("not-a-uuid", &[dir.path().join("x.png")])[0].is_err());
    }

    #[test]
    fn a_job_that_panics_still_answers() {
        let (tx, rx) = std::sync::mpsc::channel();
        answer(
            &tx,
            || panic!("a bad job"),
            || Msg::SkinFile("k".into(), 7, None),
        );
        assert!(matches!(rx.recv().unwrap(), Msg::SkinFile(key, 7, None) if key == "k"));
        answer(
            &tx,
            || Msg::SkinFile("k".into(), 8, Some(vec![1])),
            || panic!("not the fallback"),
        );
        assert!(matches!(rx.recv().unwrap(), Msg::SkinFile(_, 8, Some(_))));
    }

    #[test]
    fn a_save_that_stopped_unexpectedly_is_neither_sent_again_nor_taken_as_done() {
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c1")));
        pick(&mut skins, SkinPick::Default("alex"));
        choose(&mut skins, |edit| edit.cape = Some(CapePick::None));
        let submitted = skins.submit(false).unwrap();
        answer_save(&mut skins, submitted.id, &stopped_outcome((true, true)));
        assert!(skins.saving.is_none());
        assert!(skins.edit.is_empty(), "not sent again");
        assert_eq!(skins.unconfirmed.skin, Some(SkinPick::Default("alex")));
        assert_eq!(skins.said.len(), 2);
        assert!(skins.said.iter().all(Result::is_err));
        assert_eq!(stopped_outcome((false, true)).skin, None);
    }

    #[test]
    fn the_account_default_skin_comes_with_the_arms_shown() {
        let resources = tempfile::tempdir().unwrap();
        // An id the game gives a slim default skin, then the same skin with wide arms.
        let uuid = (0..64u32)
            .map(|n| format!("{n:032x}"))
            .find(|uuid| {
                sunrise_launcher::heads::default_skin_file(resources.path(), uuid)
                    .is_some_and(|(_, slim)| slim)
            })
            .unwrap();
        let (slim_file, _) =
            sunrise_launcher::heads::default_skin_file(resources.path(), &uuid).unwrap();
        let name = slim_file.file_stem().unwrap().to_str().unwrap().to_owned();
        assert_eq!(
            account_default_path(resources.path(), &uuid, true),
            Some(slim_file)
        );
        assert_eq!(
            account_default_path(resources.path(), &uuid, false),
            Some(default_skin_path(resources.path(), &name, false)),
            "the wide picture of the same skin"
        );
    }

    #[test]
    fn only_the_latest_listing_counts_and_a_failed_keep_stays_shown() {
        let mut skins = Skins::new_for("a");
        let one = Saved {
            id: KEPT.into(),
            name: String::new(),
            slim: false,
        };
        skins.listing((5, Ok(vec![one.clone()])));
        skins.listing((3, Ok(Vec::new())));
        assert_eq!(
            skins.saved,
            std::slice::from_ref(&one),
            "an older listing changes nothing"
        );
        skins.keep_error = Some("disk full".into());
        skins.listing((6, Err("unreadable".into())));
        assert_eq!(skins.library_error.as_deref(), Some("unreadable"));
        skins.listing((7, Ok(vec![one])));
        assert_eq!(skins.library_error, None);
        assert_eq!(
            skins.keep_error.as_deref(),
            Some("disk full"),
            "a listing that works doesn't hide a keep that didn't"
        );
    }

    /// The launcher itself, in a child of the test binary with scratch homes, no keychain and an
    /// unreachable proxy, so it reads and writes no player's data and fetches nothing.
    fn in_a_scratch_launcher(name: &str, test: impl FnOnce(&mut Launcher, &mut Context<Launcher>)) {
        const CHILD: &str = "SUNRISE_SKINS_LAUNCHER_TEST";
        if std::env::var_os(CHILD).is_none() {
            let scratch = tempfile::tempdir().unwrap();
            let root = scratch.path();
            let nowhere = "http://127.0.0.1:9";
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env(CHILD, root)
                .env("SUNRISE_LAUNCHER_HOME", root.join("launcher"))
                .env("SUNRISE_HOME", root.join("game"))
                .env("SUNRISE_LAUNCHER_LOCATION_FILE", root.join("location.json"))
                .env("SUNRISE_NO_KEYCHAIN", "1")
                .env("SUNRISE_TELEMETRY_URL", "off")
                .env("SUNRISE_LAUNCHER_NO_SELF_UPDATE", "1")
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("HTTPS_PROXY", nowhere)
                .env("HTTP_PROXY", nowhere)
                .env("ALL_PROXY", nowhere)
                .env_remove("NO_PROXY")
                .env_remove("SUNRISE_GITHUB_MIRROR")
                .env_remove("SUNRISE_LAUNCHER_BENCH")
                .env_remove("SUNRISE_LAUNCHER_DEVELOPER_OFFLINE")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let mut app = gpui::TestAppContext::single();
        let (shell, cx) = app.add_window_view(crate::window_root::Shell::new_for_test);
        let launcher = shell.read_with(cx, |shell, _| shell.launcher());
        launcher.update(cx, |l, cx| {
            l.accounts.accounts.push(account::Account {
                uuid: UUID.into(),
                name: "Player".into(),
                skin: None,
                cape: None,
            });
            test(l, cx)
        });
    }

    const UUID: &str = "069a79f444e94726a5befca90e38aaf5";

    /// The launcher's next message that `wanted` picks, the others (release lists, heads) let go.
    fn next(l: &mut Launcher, wanted: impl Fn(&Msg) -> bool) -> Msg {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let msg = l.rx.recv_timeout(left).expect("the message came");
            if wanted(&msg) {
                return msg;
            }
        }
    }

    #[test]
    fn a_save_on_any_page_holds_off_a_data_move_and_a_launcher_restart() {
        in_a_scratch_launcher(
            "skins_page::tests::a_save_on_any_page_holds_off_a_data_move_and_a_launcher_restart",
            |l, _| {
                let before = (l.location_busy(), l.launcher_update_wait());
                l.skins.show(UUID, |_| true);
                l.skins.edit.cape = Some(CapePick::None);
                l.skins.submit(false).unwrap();
                // Another account's page shown: the Save is still under way.
                l.skins.show("b09600dff4354d4fb56776eec1e55083", |_| true);
                assert!(l.location_busy());
                assert_eq!(
                    l.launcher_update_wait(),
                    Some(crate::launcher_update::Wait::Work)
                );
                l.skins.page_mut(UUID).unwrap().saving = None;
                assert_eq!((l.location_busy(), l.launcher_update_wait()), before);
                l.skins.disk_writes += 1;
                assert!(l.location_busy(), "a saved skin being written");
            },
        );
    }

    #[test]
    fn switching_the_profiles_build_while_skins_is_shown_works_the_offline_mode_out_again() {
        in_a_scratch_launcher(
            "skins_page::tests::switching_the_profiles_build_while_skins_is_shown_works_the_offline_mode_out_again",
            |l, _| {
                l.installed = vec!["developer-a".into(), "26.3-0.1.20261101".into()];
                let pin = |l: &mut Launcher, tag: &str| {
                    let id = l.selected_instance().unwrap().id.clone();
                    let instance = l.instances.list.iter_mut().find(|i| i.id == id).unwrap();
                    instance.version = Some(tag.into());
                };
                pin(l, "developer-a");
                assert_eq!(l.selected().map(String::as_str), Some("developer-a"));
                assert!(l.offline_cached(|_| true), "worked out");
                assert!(l.offline_cached(|_| false), "kept while nothing changed");
                // The profile now plays another build: the mode is worked out again.
                pin(l, "26.3-0.1.20261101");
                assert!(!l.offline_cached(|_| false));
                pin(l, "developer-a");
                assert!(l.offline_cached(|_| true));
                // The played build's install record changes (reinstalled, removed).
                let dir = backend::version_dir("developer-a");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(".installed"), "").unwrap();
                assert!(!l.offline_cached(|_| false));
                // An imported build: its base's records count, read through its own record.
                let imported = "imported-base-test";
                let own = backend::version_dir(imported);
                std::fs::create_dir_all(&own).unwrap();
                std::fs::write(
                    own.join("imported.json"),
                    r#"{"base": "developer-b", "file": "x.jar"}"#,
                )
                .unwrap();
                l.installed.push(imported.into());
                pin(l, imported);
                assert_eq!(l.selected().map(String::as_str), Some(imported));
                assert!(l.offline_cached(|_| true));
                assert!(l.offline_cached(|_| false), "kept");
                // The base reinstalled under the same tag.
                let base = backend::version_dir("developer-b");
                std::fs::create_dir_all(&base).unwrap();
                std::fs::write(base.join(".installed"), "").unwrap();
                assert!(!l.offline_cached(|_| false), "worked out again");
            },
        );
    }

    #[test]
    fn offline_now_a_save_that_ends_starts_nothing_and_choices_are_ignored() {
        in_a_scratch_launcher(
            "skins_page::tests::offline_now_a_save_that_ends_starts_nothing_and_choices_are_ignored",
            |l, _| {
                l.skins.offline_for_test = Some(false);
                l.skins.show(UUID, |_| true);
                l.skins.edit.skin = Some(SkinPick::Default("alex"));
                let save = l.skins.submit(false).unwrap();
                // The offline developer view comes on while the Save runs.
                l.skins.offline_for_test = Some(true);
                let mut done = outcome(
                    Some(Change::Done),
                    None,
                    Some(Profile {
                        id: UUID.into(),
                        ..profile(ALEX, false, Some("c1"))
                    }),
                );
                done.kept = kept(SKIN);
                l.look_saved(UUID.into(), save.id, done);
                assert!(l.skins.saving.is_none(), "reconciled");
                assert_eq!(
                    l.accounts.accounts[0].skin.as_ref().map(|(u, _)| u.as_str()),
                    Some(ALEX),
                    "recorded"
                );
                assert!(l.skins.heads_stale, "no profile head fetched offline");
                assert!(l.skins.reading.is_none(), "no profile read");
                assert!(l.skins.pictures.is_empty(), "no picture asked for");
                assert!(l.skins.keeping.is_empty(), "no skin kept");
                assert!(l.skins.writes.jobs.is_none(), "no saved skins listed");
                // A reading answered, a choice and a Save: none of them do anything offline.
                let reading = l.skins.read_asked();
                l.skins_profile(UUID.into(), reading, Ok(profile(SKIN, false, None)));
                assert!(l.skins.pictures.is_empty());
                l.choose(|edit| edit.cape = Some(CapePick::None));
                assert!(l.skins.edit.is_empty());
                l.skins.edit.cape = Some(CapePick::None);
                l.look_save();
                assert!(l.skins.saving.is_none(), "nothing sent");
                l.skins_add_for(UUID.into(), vec![PathBuf::from("x.png")]);
                assert_eq!(l.skins.disk_writes, 0, "nothing added");
            },
        );
    }

    /// A 64 by 64 skin, one texel of it `n`.
    fn skin_png(n: u8) -> Vec<u8> {
        let image = image::RgbaImage::from_fn(64, 64, |x, y| {
            image::Rgba(if (x, y) == (0, 0) {
                [n, 1, 1, 255]
            } else {
                [x as u8, y as u8, 7, 255]
            })
        });
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    const NEW: &str = "http://textures.minecraft.net/texture/new";

    /// A Save the services took, whose own profile was the one from before it.
    fn unconfirmed_save(skins: &mut Skins) {
        pick(skins, SkinPick::Default("alex"));
        choose(skins, |edit| {
            edit.cape = Some(CapePick::Minecraft("c1".into()))
        });
        let submitted = skins.submit(false).unwrap();
        let mut stale = outcome(
            Some(Change::Unconfirmed("old".into())),
            Some(Change::Unconfirmed("old".into())),
            Some(profile(SKIN, false, Some("c2"))),
        );
        stale.uploaded = Some((skin_library::id_of_png(&skin_png(5)).unwrap(), true));
        answer_save(skins, submitted.id, &stale);
        assert_eq!(
            skins.confirmed,
            look::Record::default(),
            "nothing shown yet"
        );
    }

    #[test]
    fn a_change_a_later_profile_shows_is_recorded_and_not_sent_again() {
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c2")));
        unconfirmed_save(&mut skins);
        // The next reading shows the cape and the skin, with the arms sent; the skin's picture
        // is still on its way.
        read(&mut skins, profile(NEW, true, Some("c1")));
        assert_eq!(
            skins.confirmed.cape,
            Some(Some("http://textures.minecraft.net/texture/c1".into()))
        );
        assert_eq!(
            skins.confirmed.skin, None,
            "not before its picture is checked"
        );
        skins.pictures.insert(NEW.into(), Picture::Pending);
        skins.picture(NEW.into(), Some(skin_png(5)));
        assert_eq!(
            skins.confirmed.skin,
            Some(Some((NEW.into(), "slim".into()))),
            "the picture served is the one uploaded"
        );
        assert!(skins.submit(false).is_none(), "nothing sent again");
    }

    #[test]
    fn a_later_profile_that_doesnt_show_the_change_records_nothing() {
        // Another skin at a new URL, or the arms not sent, and no cape: something else is worn,
        // so both are dropped and nothing is recorded.
        for (url, slim, png) in [(NEW, true, skin_png(6)), (NEW, false, skin_png(5))] {
            let mut skins = Skins::new_for("a");
            read(&mut skins, profile(SKIN, false, Some("c2")));
            unconfirmed_save(&mut skins);
            read(&mut skins, profile(url, slim, None));
            skins.picture(url.into(), Some(png));
            assert_eq!(skins.confirmed, look::Record::default());
            assert_eq!(
                skins.awaited,
                Awaited::default(),
                "the profile is the truth"
            );
        }
        // A Save that changes the cape again: the earlier one's cape is not recorded after it.
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c2")));
        unconfirmed_save(&mut skins);
        choose(&mut skins, |edit| edit.cape = Some(CapePick::None));
        skins.submit(false).unwrap();
        assert_eq!(skins.awaited.cape, None);
    }

    #[test]
    fn a_profile_still_behind_keeps_the_change_awaited_for_a_few_readings() {
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c2")));
        unconfirmed_save(&mut skins);
        // Readings that still show what was worn before: behind, kept.
        for _ in 1..AWAITED_READINGS {
            read(&mut skins, profile(SKIN, false, Some("c2")));
            assert!(skins.awaited.skin.is_some() && skins.awaited.cape.is_some());
        }
        // The next shows both: recorded.
        read(&mut skins, profile(NEW, true, Some("c1")));
        skins.picture(NEW.into(), Some(skin_png(5)));
        assert_eq!(
            skins.confirmed.cape,
            Some(Some("http://textures.minecraft.net/texture/c1".into()))
        );
        assert_eq!(
            skins.confirmed.skin,
            Some(Some((NEW.into(), "slim".into())))
        );
        // Behind for every reading allowed: dropped after the last.
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c2")));
        unconfirmed_save(&mut skins);
        for _ in 0..AWAITED_READINGS {
            read(&mut skins, profile(SKIN, false, Some("c2")));
        }
        assert_eq!(skins.awaited, Awaited::default());
        assert_eq!(skins.confirmed, look::Record::default());
    }

    #[test]
    fn a_picture_that_couldnt_be_had_leaves_the_skin_awaited() {
        let mut skins = Skins::new_for("a");
        read(&mut skins, profile(SKIN, false, Some("c2")));
        unconfirmed_save(&mut skins);
        read(&mut skins, profile(NEW, true, Some("c2")));
        skins.pictures.insert(NEW.into(), Picture::Pending);
        skins.picture(NEW.into(), None);
        assert!(
            skins.awaited.skin.is_some(),
            "kept: the picture is asked again"
        );
        assert_eq!(skins.confirmed.skin, None);
        // The next reading names it again, and this time its picture is had.
        read(&mut skins, profile(NEW, true, Some("c2")));
        skins.picture(NEW.into(), Some(skin_png(5)));
        assert_eq!(
            skins.confirmed.skin,
            Some(Some((NEW.into(), "slim".into())))
        );
    }

    #[test]
    fn a_save_waits_for_what_an_earlier_one_confirmed_to_be_recorded() {
        let mut skins = Skins::new_for("a");
        skins.recording = 1;
        choose(&mut skins, |edit| edit.cape = Some(CapePick::None));
        assert!(skins.submit(false).is_none());
        skins.recording = 0;
        assert!(skins.submit(false).is_some());
    }

    #[test]
    fn a_queued_texture_job_fetches_nothing_once_offline() {
        static FETCHED: AtomicU64 = AtomicU64::new(0);
        fn fetch(_: &str) -> Option<Vec<u8>> {
            FETCHED.fetch_add(1, Ordering::Relaxed);
            Some(vec![1])
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let offline = Arc::new(AtomicBool::new(false));
        let job = texture_job(tx.clone(), SKIN.into(), offline.clone(), fetch);
        // The view comes on after the job was queued, before it starts.
        offline.store(true, Ordering::Relaxed);
        job();
        assert!(matches!(rx.recv().unwrap(), Msg::Picture(url, None) if url == SKIN));
        assert_eq!(FETCHED.load(Ordering::Relaxed), 0, "no request");
        offline.store(false, Ordering::Relaxed);
        texture_job(tx, SKIN.into(), offline, fetch)();
        assert!(matches!(rx.recv().unwrap(), Msg::Picture(_, Some(_))));
        assert_eq!(FETCHED.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_texture_jobs_go_by_the_pages_own_answer() {
        in_a_scratch_launcher(
            "skins_page::tests::the_texture_jobs_go_by_the_pages_own_answer",
            |l, _| {
                l.skins.offline_for_test = Some(true);
                assert!(l.skins_offline());
                assert!(l.skins.offline_flag.load(Ordering::Relaxed));
                l.skins.offline_for_test = Some(false);
                assert!(!l.skins_offline());
                assert!(!l.skins.offline_flag.load(Ordering::Relaxed));
            },
        );
    }

    #[test]
    fn a_confirmed_change_is_written_to_accounts_json_and_shown() {
        in_a_scratch_launcher(
            "skins_page::tests::a_confirmed_change_is_written_to_accounts_json_and_shown",
            |l, _| {
                l.skins.offline_for_test = Some(false);
                l.accounts.save().unwrap();
                l.skins.show(UUID, |_| true);
                l.skins.profile = Some(Ok(profile(SKIN, false, Some("c2"))));
                pick(&mut l.skins, SkinPick::Default("alex"));
                l.skins.edit.cape = Some(CapePick::Minecraft("c1".into()));
                let save = l.skins.submit(false).unwrap();
                let mut stale = outcome(
                    Some(Change::Unconfirmed("old".into())),
                    Some(Change::Unconfirmed("old".into())),
                    Some(profile(SKIN, false, Some("c2"))),
                );
                stale.uploaded = Some((skin_library::id_of_png(&skin_png(5)).unwrap(), true));
                l.look_saved(UUID.into(), save.id, stale);
                assert_eq!(l.accounts.accounts[0].skin, None, "not confirmed yet");
                // The picture at the new URL is here; the next reading shows both changes.
                l.skins.reading = None;
                l.skins
                    .pictures
                    .insert(NEW.into(), Picture::Ready(skin_png(5)));
                let reading = l.skins.read_asked();
                l.skins_profile(UUID.into(), reading, Ok(profile(NEW, true, Some("c1"))));
                assert_eq!(l.skins.recording, 1, "being recorded");
                assert!(l.skins.writing());
                assert_eq!(
                    l.accounts.accounts[0].skin, None,
                    "the chip waits until it is written"
                );
                let Msg::LookRecorded(uuid, record, recorded) =
                    next(l, |msg| matches!(msg, Msg::LookRecorded(..)))
                else {
                    unreachable!()
                };
                l.look_recorded(uuid, record, recorded);
                assert_eq!(l.skins.recording, 0);
                assert_eq!(
                    l.accounts.accounts[0].skin,
                    Some((NEW.into(), "slim".into())),
                    "the chip shows it"
                );
                let on_disk = account::Accounts::load();
                assert_eq!(on_disk.accounts[0].skin, Some((NEW.into(), "slim".into())));
                assert_eq!(
                    on_disk.accounts[0].cape.as_deref(),
                    Some("http://textures.minecraft.net/texture/c1")
                );
                assert!(l.skins.edit.is_empty() && l.skins.submit(false).is_none());
            },
        );
    }

    #[test]
    fn a_confirmed_change_the_queue_wont_take_stays_to_record() {
        in_a_scratch_launcher(
            "skins_page::tests::a_confirmed_change_the_queue_wont_take_stays_to_record",
            |l, _| {
                l.skins.show(UUID, |_| true);
                // The writes queue is full: a job holds its thread, the rest fill it.
                let (started, running) = std::sync::mpsc::channel::<()>();
                let (release, held) = std::sync::mpsc::channel::<()>();
                l.skins
                    .writes
                    .push(
                        "test",
                        Box::new(move || {
                            let _ = started.send(());
                            let _ = held.recv();
                        }),
                    )
                    .unwrap();
                running.recv().unwrap();
                while l.skins.writes.push("test", Box::new(|| {})).is_ok() {}
                l.skins.confirmed.cape = Some(None);
                l.record_confirmed(UUID);
                assert_eq!(l.skins.confirmed.cape, Some(None), "kept, to try again");
                assert_eq!(l.skins.recording, 0);
                release.send(()).unwrap();
            },
        );
    }

    #[test]
    fn a_skin_kept_after_a_save_gave_it_other_arms_keeps_them() {
        in_a_scratch_launcher(
            "skins_page::tests::a_skin_kept_after_a_save_gave_it_other_arms_keeps_them",
            |l, _| {
                l.skins.offline_for_test = Some(false);
                let dir = skin_library::folder(UUID).unwrap();
                // An arm-only Save gave the worn skin slim arms ...
                let saved = skin_library::add(&dir, &skin_png(3), "", Some(true)).unwrap();
                // ... and a keep queued before it ends after it, with the classic arms the page
                // saw then.
                l.skins.show(UUID, |_| true);
                l.skins.profile = Some(Ok(profile(SKIN, false, None)));
                l.skins
                    .pictures
                    .insert(SKIN.into(), Picture::Ready(skin_png(3)));
                l.remember_worn();
                let Msg::SkinKept(uuid, url, kept, listed) =
                    next(l, |msg| matches!(msg, Msg::SkinKept(..)))
                else {
                    unreachable!()
                };
                l.skins_kept(uuid, url, kept, listed);
                let listed = skin_library::list(&dir).unwrap();
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0].id, saved.id);
                assert!(listed[0].slim, "the arms the Save gave it");
            },
        );
    }

    #[test]
    fn a_reading_for_an_account_not_shown_settles_on_its_own_page() {
        in_a_scratch_launcher(
            "skins_page::tests::a_reading_for_an_account_not_shown_settles_on_its_own_page",
            |l, _| {
                l.skins.offline_for_test = Some(false);
                l.skins.show(UUID, |_| true);
                let reading = l.skins.read_asked();
                let other = "b09600dff4354d4fb56776eec1e55083";
                l.skins.show(other, |_| true);
                // The page shown has pictures of its own not asked for yet: an answer for another
                // account starts nothing for it either.
                l.skins.profile = Some(Ok(profile(ALEX, false, None)));
                l.skins_profile(UUID.into(), reading, Ok(profile(SKIN, false, Some("c1"))));
                let page = l.skins.page_mut(UUID).unwrap();
                assert_eq!(page.reading, None, "answered: the next opening reads again");
                assert!(matches!(page.profile, Some(Ok(_))));
                assert!(
                    page.pictures.is_empty(),
                    "nothing asked for a page not shown"
                );
                assert_eq!(
                    l.skins.profile,
                    Some(Ok(profile(ALEX, false, None))),
                    "the page shown is left alone"
                );
                assert!(l.skins.pictures.is_empty(), "and asks for nothing");
                // In the offline view an answer is not taken in, but the reading is over.
                l.skins.offline_for_test = Some(true);
                l.skins.show(UUID, |_| true);
                let reading = l.skins.read_asked();
                l.skins_profile(UUID.into(), reading, Ok(profile(ALEX, false, None)));
                assert_eq!(l.skins.reading, None);
            },
        );
    }

    #[test]
    fn a_restart_waits_for_a_skin_being_written() {
        in_a_scratch_launcher(
            "skins_page::tests::a_restart_waits_for_a_skin_being_written",
            |l, cx| {
                assert!(l.restart_blocked_by_skins().is_none());
                // A data folder that can't be retried or given up, so neither path can restart
                // this test even if it didn't wait: the pointer is damaged, and its backup's
                // place is taken by a folder that can't be removed as a file.
                let pointer =
                    PathBuf::from(std::env::var_os("SUNRISE_LAUNCHER_LOCATION_FILE").unwrap());
                std::fs::write(&pointer, "{ damaged").unwrap();
                let backup = pointer.with_file_name("location.json.old");
                std::fs::create_dir_all(backup.join("kept")).unwrap();
                l.skins.disk_writes += 1;
                let wait = l.restart_blocked_by_skins().unwrap();
                l.retry_location(cx);
                assert_eq!(
                    l.location_error.as_deref(),
                    Some(wait.as_str()),
                    "Retry waits"
                );
                l.location_error = None;
                l.use_default_location(cx);
                assert_eq!(
                    l.location_error.as_deref(),
                    Some(wait.as_str()),
                    "Use the default folder waits"
                );
                assert!(pointer.is_file(), "the pointer left as it was");
            },
        );
    }

    #[test]
    fn a_failed_texture_fetch_then_a_successful_one_records_the_new_skin() {
        in_a_scratch_launcher(
            "skins_page::tests::a_failed_texture_fetch_then_a_successful_one_records_the_new_skin",
            |l, _| {
                l.skins.offline_for_test = Some(false);
                l.accounts.save().unwrap();
                l.skins.show(UUID, |_| true);
                l.skins.profile = Some(Ok(profile(SKIN, false, None)));
                pick(&mut l.skins, SkinPick::Default("alex"));
                let save = l.skins.submit(false).unwrap();
                let uploaded_png = skin_png(7);
                let mut unconfirmed = outcome(
                    Some(Change::Unconfirmed("couldn't check".into())),
                    None,
                    // Mojang already returned the new skin URL, only fetching its PNG failed.
                    Some(profile(NEW, true, None)),
                );
                unconfirmed.uploaded =
                    Some((skin_library::id_of_png(&uploaded_png).unwrap(), true));
                l.look_saved(UUID.into(), save.id, unconfirmed);
                assert_eq!(
                    l.skins.awaited.skin_before,
                    Some((SKIN.into(), "classic".into())),
                    "keeps the pre-upload skin, not the post-upload URL"
                );

                // First texture fetch at NEW fails.
                l.skins_picture(NEW.into(), None);
                assert_eq!(l.skins.confirmed.skin, None);

                // Later profile read returns the same NEW profile; then the PNG fetch succeeds.
                l.skins.reading = None;
                let reading = l.skins.read_asked();
                l.skins_profile(UUID.into(), reading, Ok(profile(NEW, true, None)));
                assert_eq!(
                    l.skins.awaited.skin_at,
                    Some((NEW.into(), "slim".into())),
                    "NEW is not mistaken for the pre-upload skin"
                );
                l.skins_picture(NEW.into(), Some(uploaded_png));
                let Msg::LookRecorded(uuid, record, recorded) =
                    next(l, |msg| matches!(msg, Msg::LookRecorded(..)))
                else {
                    unreachable!()
                };
                l.look_recorded(uuid, record, recorded);
                assert_eq!(
                    account::Accounts::load().accounts[0].skin,
                    Some((NEW.into(), "slim".into())),
                    "new skin recorded without uploading again"
                );
            },
        );
    }

    #[test]
    fn a_newer_accepted_save_supersedes_a_pending_confirmed_record() {
        in_a_scratch_launcher(
            "skins_page::tests::a_newer_accepted_save_supersedes_a_pending_confirmed_record",
            |l, _| {
                l.skins.offline_for_test = Some(false);
                l.accounts.save().unwrap();
                l.skins.show(UUID, |_| true);
                l.skins.profile = Some(Ok(profile(SKIN, false, Some("c1"))));

                // Fill the write queue so an older confirmed cape record stays pending.
                let (started, running) = std::sync::mpsc::channel::<()>();
                let (release, held) = std::sync::mpsc::channel::<()>();
                l.skins
                    .writes
                    .push(
                        "test",
                        Box::new(move || {
                            let _ = started.send(());
                            let _ = held.recv();
                        }),
                    )
                    .unwrap();
                running.recv().unwrap();
                while l.skins.writes.push("test", Box::new(|| {})).is_ok() {}
                l.skins.confirmed.cape =
                    Some(Some("http://textures.minecraft.net/texture/c1".into()));
                l.record_confirmed(UUID);
                assert!(l.skins.confirmed.cape.is_some());
                assert_eq!(l.skins.recording, 0);

                // Release the queue before the newer Save lands, and simulate the newer Save
                // writing cape c2 to accounts.json directly (as look::save does on Change::Done).
                release.send(()).unwrap();
                let newer_cape = Some("http://textures.minecraft.net/texture/c2".to_string());
                account::Accounts::save_look(UUID, None, Some(newer_cape.clone())).unwrap();

                l.skins.edit.cape = Some(CapePick::Minecraft("c2".into()));
                let save = l.skins.submit(false).unwrap();
                let done = outcome(
                    None,
                    Some(Change::Done),
                    Some(profile(SKIN, false, Some("c2"))),
                );
                l.look_saved(UUID.into(), save.id, done);
                assert_eq!(
                    l.skins.confirmed.cape, None,
                    "superseded by the newer accepted Save"
                );
                assert_eq!(l.skins.recording, 0);

                // Drain the queue by waiting on a sentinel job on the same worker. The worker
                // was just released above, so wait briefly for a slot if the queue is still full.
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let tx = done_tx.clone();
                    match l.skins.writes.push(
                        "drain",
                        Box::new(move || {
                            let _ = tx.send(());
                        }),
                    ) {
                        Ok(()) => break,
                        Err(_) if std::time::Instant::now() < deadline => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(err) => panic!("write queue never drained: {err:?}"),
                    }
                }
                done_rx.recv().unwrap();

                assert_eq!(
                    account::Accounts::load().accounts[0].cape,
                    newer_cape,
                    "newer cape stays in accounts.json after the queue drains"
                );
            },
        );
    }

    #[test]
    fn switching_profiles_from_the_rail_refreshes_the_texture_offline_flag() {
        in_a_scratch_launcher(
            "skins_page::tests::switching_profiles_from_the_rail_refreshes_the_texture_offline_flag",
            |l, cx| {
                static FETCHED: AtomicU64 = AtomicU64::new(0);
                fn fetch(_: &str) -> Option<Vec<u8>> {
                    FETCHED.fetch_add(1, Ordering::Relaxed);
                    Some(vec![1])
                }
                l.skins.offline_for_test = Some(false);
                assert!(!l.skins_offline());
                assert!(!l.skins.offline_flag.load(Ordering::Relaxed));

                // Queue a texture job while online.
                let (tx, rx) = std::sync::mpsc::channel();
                let job = texture_job(tx, SKIN.into(), l.skins.offline_flag.clone(), fetch);

                // Switch from the rail to another profile whose build is in offline mode.
                // Switching from the rail opens Home, not Skins, yet must refresh the flag.
                let other = instances::create("Offline", None).unwrap();
                l.reload_instances();
                l.skins.offline_for_test = Some(true);
                l.switch_profile(&other, cx);
                assert!(matches!(l.ui.page, crate::frame::Page::Home));
                assert!(
                    l.skins.offline_flag.load(Ordering::Relaxed),
                    "offline flag refreshed on profile switch outside Skins"
                );

                // Running the queued job now makes no online request.
                job();
                assert!(matches!(rx.recv().unwrap(), Msg::Picture(url, None) if url == SKIN));
                assert_eq!(FETCHED.load(Ordering::Relaxed), 0);
            },
        );
    }

    #[test]
    fn a_test_window_does_not_start_startup_update_checks() {
        in_a_scratch_launcher(
            "skins_page::tests::a_test_window_does_not_start_startup_update_checks",
            |l, _| {
                assert!(
                    !l.checking_updates && l.checked.is_none(),
                    "Shell::new_for_test must not start startup GitHub update checks"
                );
            },
        );
    }
}
