//! Settings › Appearance: the theme (the built-in ones, then the player's own files), the
//! accent under it, what is behind Home, and the themes folder.
//!
//! Appearance changes use the same settings as the command line. The opacity slider only
//! previews during a drag; a failed save on release uses the ordinary Retry notice. The themes
//! folder is read at startup and on Reload, and its pictures decode off the window's thread.
use super::*;
use crate::paint::Palette;
use gpui::RenderImage;
use std::path::PathBuf;
use std::sync::Arc;
use sunrise_launcher::appearance::{self, Behind};
use sunrise_launcher::backend::{PANEL_OPACITY_MAX, PANEL_OPACITY_MIN};
use sunrise_launcher::theme_files::{self, Entry};
use sunrise_launcher::themes::{self, Accent};

/// An accent swatch's size, its ring included.
const SWATCH: f32 = 30.;

/// What [`Accent::ALL`]'s swatches are called, in their order.
const ACCENT_NAMES: [&str; 7] = ["Violet", "Blue", "Teal", "Green", "Amber", "Red", "Pink"];

/// A theme as Appearance lists it.
struct Choice {
    /// What `select_theme` takes.
    id: String,
    name: SharedString,
    about: SharedString,
    /// Its colours with the accent it would start with, for the preview.
    palette: Palette,
}

/// What stands behind Home, as the frame's backdrop draws it: the player's own picture or
/// their theme's, decoded to one still picture off the window's thread. The window is never
/// handed a file to decode itself.
#[derive(Clone)]
pub(crate) struct HomePhoto {
    pub image: Arc<RenderImage>,
    /// Representative colour of the visible pixels, computed once with the decode.
    pub tone: Option<u32>,
}

/// Sample the centres of a grid bounded to 32 × 32, independent of image size. Weight sRGB
/// channels by coverage, so transparent pixels (which may carry arbitrary RGB) do not tint us.
fn photo_tone(image: &image::RgbaImage) -> Option<u32> {
    let (w, h) = image.dimensions();
    let (cols, rows) = (w.min(32), h.min(32));
    let mut sum = [0_u32; 3];
    let mut weight = 0;
    let mut strongest = (0, 0);
    for y in 0..rows {
        for x in 0..cols {
            let [r, g, b, a] =
                image[((2 * x + 1) * w / (2 * cols), (2 * y + 1) * h / (2 * rows))].0;
            let a = u32::from(a);
            weight += a;
            let chroma = a * u32::from(r.max(g).max(b) - r.min(g).min(b));
            if chroma > strongest.0 {
                strongest = (chroma, u32::from_be_bytes([0, r, g, b]));
            }
            for (total, channel) in sum.iter_mut().zip([r, g, b]) {
                *total += u32::from(channel) * a;
            }
        }
    }
    (weight > 0).then(|| {
        let [r, g, b] = sum.map(|c| (c + weight / 2) / weight);
        // A colourful picture can average to gray if its hues cancel. In that case use
        // the most visibly chromatic sample instead of losing its colour altogether.
        if r == g && g == b && strongest.0 > 0 {
            strongest.1
        } else {
            r << 16 | g << 8 | b
        }
    })
}

/// The settings what is behind Home depends on: the theme, the player's picture, scenery.
type BehindKey = (Option<String>, Option<String>, bool);

pub(crate) struct State {
    /// The themes to choose from, made again when the folder is read or a choice changes.
    choices: Option<Vec<Choice>>,
    accent: line_edit::LineEdit,
    accent_focus: Option<FocusHandle>,
    /// Whether the accent field had the keyboard when last drawn: leaving it takes what is typed.
    accent_editing: bool,
    panel_opacity: crate::slider::Slider,
    /// The keyboard's place on "Worlds on Home", and the search's request to be taken to it:
    /// whether that was done yet.
    worlds_focus: Option<FocusHandle>,
    worlds_asked: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    /// What the last action could not do.
    error: Option<String>,
    /// What Reload found.
    note: Option<String>,
    /// What is behind Home for the settings it was worked out from (theme, picture, scenery).
    behind: Option<(BehindKey, Behind)>,
    /// The picture behind Home: its file, and the picture once it is decoded (None: it could
    /// not be).
    photo: Option<(PathBuf, Option<HomePhoto>)>,
    decoding: bool,
    /// The kept copy of the player's own picture that could not be decoded: Home shows what
    /// comes after it, and Appearance says so.
    unshown: Option<PathBuf>,
    /// Pictures no longer shown, for the window to let go of.
    retired: Vec<Arc<RenderImage>>,
}

impl Default for State {
    fn default() -> Self {
        State {
            choices: None,
            accent: line_edit::LineEdit::new(String::new(), 32, 18),
            accent_focus: None,
            accent_editing: false,
            panel_opacity: crate::slider::Slider::default(),
            worlds_focus: None,
            worlds_asked: None,
            error: None,
            note: None,
            behind: None,
            photo: None,
            decoding: false,
            unshown: None,
            retired: Vec::new(),
        }
    }
}

impl State {
    /// The search opened "Worlds on Home": go to its control when the section is next drawn.
    pub(crate) fn ask_for_worlds(&mut self) {
        self.worlds_asked = Some(Default::default());
    }
}

/// The look's part of the settings, to put back when a change can't be saved.
struct Look {
    theme: Option<String>,
    accent: Accent,
    accents: std::collections::BTreeMap<String, Accent>,
    background: Option<String>,
    scenery: Option<bool>,
    reduced_graphics: Option<bool>,
}

/// A theme's preview: its page, a panel on it with both inks, and Play in its own fill.
/// The least width of a theme's card, and the room between cards.
const CARD_MIN: f32 = 168.;
const GAP: f32 = 12.;

thread_local! {
    /// Where the grid of themes was last drawn, for its number of columns.
    static THEME_GRID: gpui::ScrollHandle = gpui::ScrollHandle::new();
}

fn preview(p: &Palette) -> Div {
    let bar = |w: f32, color: u32| {
        div()
            .h(px(4.))
            .w(px(w))
            .rounded(crate::tokens::r(crate::tokens::R_BAR))
            .bg(rgb(color))
    };
    div()
        .h(px(92.))
        .p(px(10.))
        .bg(rgb(p.background))
        .flex()
        .gap(px(6.))
        .child(
            div()
                .w(px(14.))
                .h_full()
                .rounded(crate::tokens::r(crate::tokens::R_SMALL))
                .bg(rgb(p.chrome))
                .border_1()
                .border_color(rgb(p.chrome_line))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(4.))
                .pt(px(6.))
                .child(bar(6., p.chrome_text))
                .child(bar(6., p.chrome_muted)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .rounded(crate::tokens::r(crate::tokens::R_SMALL))
                .bg(rgb(p.card_color))
                .border_1()
                .border_color(rgb(p.divider))
                .p(px(8.))
                .flex()
                .flex_col()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(tokens::LABEL))
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(p.text))
                                .truncate()
                                .child(tr("Selected profile")),
                        )
                        .child(
                            div()
                                .text_size(px(tokens::CAPTION))
                                .text_color(rgb(p.muted))
                                .truncate()
                                .child(tr("Not played yet")),
                        ),
                )
                .child(
                    div().flex().child(
                        div()
                            .px(px(10.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .rounded(crate::tokens::r(crate::tokens::R_SMALL))
                            .border_1()
                            .border_color(p.input_edge())
                            .bg(p.play_gradient())
                            .text_size(px(tokens::CAPTION))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(p.accent_on))
                            .child(tr("Play")),
                    ),
                ),
        )
}

const OPACITY_SPAN: f32 = (PANEL_OPACITY_MAX - PANEL_OPACITY_MIN) as f32;

fn opacity_percent(value: f32) -> u8 {
    PANEL_OPACITY_MIN + (value * OPACITY_SPAN).round() as u8
}

impl Launcher {
    /// The frame previews a held slider, but navigation away discards it without saving.
    pub(crate) fn panel_opacity_preview(&self) -> Option<u8> {
        let slider = &self.ui.settings.appearance.panel_opacity;
        if self.ui.page == crate::frame::Page::Settings
            && self.ui.settings.section == Section::Appearance
        {
            slider.preview().map(opacity_percent)
        } else {
            slider.cancel();
            None
        }
    }

    fn look(&self) -> Look {
        Look {
            theme: self.settings.theme.clone(),
            accent: self.settings.accent,
            accents: self.settings.theme_accents.clone(),
            background: self.settings.background.clone(),
            scenery: self.settings.scenery_on,
            reduced_graphics: self.settings.reduced_graphics,
        }
    }

    /// Makes one change to the look and saves it. A change that is refused or can't be saved
    /// leaves the settings as they were and is said under the row.
    fn appearance_change(
        &mut self,
        change: impl FnOnce(&mut backend::Settings) -> Result<(), String>,
        window: &Window,
    ) {
        let before = self.look();
        let done = change(&mut self.settings).and_then(|()| self.settings.save());
        let state = &mut self.ui.settings.appearance;
        state.note = None;
        state.error = done.err();
        if state.error.is_some() {
            let Look {
                theme,
                accent,
                accents,
                background,
                scenery,
                reduced_graphics,
            } = before;
            self.settings.theme = theme;
            self.settings.accent = accent;
            self.settings.theme_accents = accents;
            self.settings.background = background;
            self.settings.scenery_on = scenery;
            self.settings.reduced_graphics = reduced_graphics;
            return;
        }
        self.appearance_changed(window);
    }

    /// The look changed, by a choice or by Reload: what was worked out from the old one is
    /// worked out again.
    fn appearance_changed(&mut self, window: &Window) {
        self.theme = self.settings_theme(window);
        let state = &mut self.ui.settings.appearance;
        state.choices = None;
        state.behind = None;
        state.accent.set(appearance::accent(&self.settings).name());
    }

    /// Reads the themes folder again: a repaired theme is back, an edited one is as edited.
    fn appearance_reload(&mut self, window: &Window) {
        let entries = theme_files::reload();
        let state = &mut self.ui.settings.appearance;
        // The same file may hold another picture now.
        if let Some((_, Some(old))) = state.photo.take() {
            state.retired.push(old.image);
        }
        let bad = entries
            .iter()
            .filter(|e| matches!(e, Entry::Bad { .. }))
            .count();
        state.error = None;
        state.note = Some(match (entries.len() - bad, bad) {
            (0, 0) => tr("No themes in the folder yet.").to_string(),
            (good, 0) => trf(
                "Read {count} of your themes.",
                &[("count", &good.to_string())],
            ),
            (good, bad) => trf(
                "Read {count} of your themes; {bad} can't be used.",
                &[("count", &good.to_string()), ("bad", &bad.to_string())],
            ),
        });
        self.appearance_changed(window);
    }

    /// What stands behind Home, for the frame's backdrop: the player's picture, else their
    /// theme's once it is decoded. None is the theme's scene (or nothing, with scenery off).
    /// Worked out when the settings it depends on change, not every frame.
    pub(crate) fn home_photo(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<HomePhoto> {
        let key = (
            self.settings.theme.clone(),
            self.settings.background.clone(),
            self.settings.scenery(),
        );
        let state = &mut self.ui.settings.appearance;
        for old in state.retired.drain(..) {
            let _ = window.drop_image(old);
        }
        if state.behind.as_ref().is_none_or(|(old, _)| *old != key) {
            let behind = match appearance::behind_home(&self.settings, &backend::data_dir()) {
                Behind::Personal(path) if state.unshown.as_ref() == Some(&path) => {
                    appearance::behind_without_own(&self.settings)
                }
                behind => behind,
            };
            state.behind = Some((key, behind));
        }
        let (path, own) = match state.behind.as_ref().map(|(_, behind)| behind.clone())? {
            Behind::Plain | Behind::Scene => return None,
            Behind::Personal(path) => (path, true),
            Behind::Theme(path) => (path, false),
        };
        if let Some((_, picture)) = state.photo.as_ref().filter(|(of, _)| *of == path) {
            return picture.clone();
        }
        if state.decoding {
            return None;
        }
        state.decoding = true;
        let read = path.clone();
        let task = cx.background_executor().spawn(async move {
            if own {
                let name = read
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                themes::background_picture(&backend::data_dir(), name)
            } else {
                theme_files::picture(&read).map_err(|reason| reason.text())
            }
            .map(|still| {
                let rgba = theme_files::fitted(still, sunrise_launcher::pictures::gpu_limit());
                let tone = photo_tone(&rgba);
                (rgba, tone)
            })
        });
        cx.spawn(async move |this, cx| {
            let decoded = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.ui.settings.appearance;
                state.decoding = false;
                let picture = match decoded {
                    Ok((mut rgba, tone)) => {
                        // The window draws BGRA, its alpha premultiplied.
                        for px in rgba.pixels_mut() {
                            let a = u16::from(px.0[3]);
                            let [r, g, b] = [0, 1, 2].map(|i| (u16::from(px.0[i]) * a / 255) as u8);
                            px.0 = [b, g, r, a as u8];
                        }
                        Some(HomePhoto {
                            image: Arc::new(RenderImage::new([image::Frame::new(rgba)])),
                            tone,
                        })
                    }
                    Err(said) => {
                        state.error = Some(said);
                        if own {
                            // A kept copy that can't be decoded is never claimed as shown:
                            // Home goes on to the theme's picture, or the scene.
                            state.unshown = Some(path.clone());
                            if let Some((_, behind)) = &mut state.behind {
                                *behind = appearance::behind_without_own(&this.settings);
                            }
                        }
                        None
                    }
                };
                if let Some((_, Some(old))) = state.photo.replace((path, picture)) {
                    state.retired.push(old.image);
                }
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// The themes to choose from: Dawn first, Sunrise's own violet, the other built-in ones,
    /// then the player's.
    fn appearance_choices(&mut self) {
        if self.ui.settings.appearance.choices.is_some() {
            return;
        }
        let tint = self.settings.glass_tint();
        let in_use = appearance::effective_id(&self.settings);
        let choice = |id: String, name: SharedString, about: SharedString, night, tone| {
            // Each preview wears its theme's own accent; the one in use, the accent in use.
            let accent = if id == in_use {
                appearance::accent(&self.settings).rgb()
            } else {
                themes::own_accent(Some(&id)).rgb()
            };
            Choice {
                palette: Palette::plain(night, accent, tint, tone),
                id,
                name,
                about,
            }
        };
        let builtin = |t: &themes::Theme| {
            choice(
                t.id.into(),
                tr(t.name).into(),
                tr(t.description).into(),
                t.palette.copied().unwrap_or(theme::night::NIGHT),
                Some(theme::sky_tone(t.sky)),
            )
        };
        let mut all = vec![
            builtin(&themes::DAWN_THEME),
            choice(
                themes::TWILIGHT.into(),
                tr(themes::TWILIGHT_NAME).into(),
                tr(themes::TWILIGHT_DESCRIPTION).into(),
                theme::night::NIGHT,
                Some(theme::twilight_tone()),
            ),
        ];
        all.extend(
            themes::THEMES
                .iter()
                .filter(|t| t.id != themes::DAWN_THEME.id)
                .map(builtin),
        );
        for entry in theme_files::loaded().iter() {
            if let Entry::Good(local) = entry {
                all.push(choice(
                    local.id(),
                    local.name.clone().into(),
                    trf("Your theme · {folder}", &[("folder", &local.slug)]).into(),
                    local.roles.night(),
                    None,
                ));
            }
        }
        self.ui.settings.appearance.choices = Some(all);
    }

    pub(super) fn settings_appearance(&mut self, window: &Window, cx: &mut Context<Self>) -> Div {
        self.appearance_choices();
        let missing = appearance::missing(&self.settings);
        let selected = match &self.settings.theme {
            // A missing theme is still the choice: nothing else is ticked in its place.
            Some(id) if missing => id.clone(),
            _ => appearance::effective_id(&self.settings),
        };
        // As many columns as fit at a card's least width, and the cards share the row between
        // them: no strip of empty space on the right at any width. The row's width is the one it
        // was last drawn at.
        let grid = THEME_GRID.with(|g| g.clone());
        let width = f32::from(grid.bounds().size.width);
        let columns = (((width + GAP) / (CARD_MIN + GAP)).floor() as u16).max(1);
        let mut cards = div()
            .id("alt-theme-grid")
            .track_scroll(&grid)
            .grid()
            .grid_cols(columns)
            .gap(px(GAP));
        let choices = self
            .ui
            .settings
            .appearance
            .choices
            .as_deref()
            .unwrap_or_default();
        for (n, choice) in choices.iter().enumerate() {
            let on = choice.id == selected;
            let id = choice.id.clone();
            cards = cards.child(
                div()
                    .id(ElementId::named_usize("alt-theme", n))
                    .tab_index(0)
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .rounded(crate::tokens::r(crate::tokens::R_CARD))
                    .overflow_hidden()
                    .border_1()
                    .border_color(rgb(if on {
                        palette().text
                    } else {
                        palette().divider
                    }))
                    .bg(rgb(palette().card_color))
                    .cursor_pointer()
                    .focus_visible(|s| s.shadow(crate::frame::focus_ring(palette().focus)))
                    .seen()
                    .child(preview(&choice.palette))
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_size(px(tokens::LABEL))
                                            .truncate()
                                            .child(choice.name.clone()),
                                    )
                                    // Chosen is said in words and by the tick, not by colour
                                    // alone.
                                    .child(if on {
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(4.))
                                            .text_size(px(tokens::SMALL))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(blocks::icon(
                                                "check",
                                                13.,
                                                rgb(palette().text).into(),
                                            ))
                                            .child(tr("Selected"))
                                    } else {
                                        hint(choice.about.clone()).truncate()
                                    }),
                            ),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.appearance_change(|s| appearance::select_theme(s, &id), window);
                        cx.notify();
                    })),
            );
        }
        // A folder that isn't a theme is listed with why, and can't be chosen.
        let bad: Vec<Div> = theme_files::loaded()
            .iter()
            .filter_map(|entry| match entry {
                Entry::Bad { folder, reason } => Some(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(10.))
                        .opacity(0.9)
                        .child(
                            blocks::icon("info-box", 15., rgb(palette().warning).into()).mt(px(2.)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_size(px(tokens::LABEL))
                                        .child(trf(
                                            "{folder} can't be used",
                                            &[("folder", folder)],
                                        )),
                                )
                                .child(hint(reason.text())),
                        ),
                ),
                Entry::Good(_) => None,
            })
            .collect();
        let state = &self.ui.settings.appearance;
        let said = state
            .error
            .clone()
            .map(warning)
            .or_else(|| state.note.clone().map(hint));
        let gone = missing.then(|| {
            warning(trf(
                "The chosen theme, {id}, can't be read. Dawn is shown until it is repaired: fix its file, then press Reload.",
                &[("id", self.settings.theme.as_deref().unwrap_or_default())],
            ))
        });
        let theme = stacked(
            tr("Theme"),
            tr("Colours and scenery for the whole launcher. Play uses white text in every theme."),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .children(gone)
                .child(cards),
        );
        let mut rows = vec![self.panel_opacity_row(cx), theme];
        if !bad.is_empty() {
            rows.push(div().flex().flex_col().gap(px(10.)).children(bad));
        }
        rows.push(self.accent_row(missing, window, cx));
        rows.push(self.background_row(cx));
        rows.push(self.scenery_row(cx));
        let reduced = self.settings.reduced_graphics();
        rows.push(row(
            tr("Reduced graphics"),
            tr("Turns off glass blur and keeps the scenery and cat still. Also reduces motion in the launcher."),
            div().child(
                switch("alt-reduced-graphics", reduced)
                    .seen()
                    .tooltip(crate::glass_ui::tip(tr("Reduced graphics")))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.appearance_change(
                            |s| {
                                s.reduced_graphics = Some(!reduced);
                                Ok(())
                            },
                            window,
                        );
                        cx.notify();
                    })),
            ),
        ));
        let layout = self.settings.world_layout;
        let layouts = [
            (backend::WorldLayout::Rows, "Rows"),
            (backend::WorldLayout::Tiles, "Tiles"),
        ]
        .into_iter()
        .map(|(way, label)| {
            let pick: crate::mods::Choose = std::rc::Rc::new(move |this, _| {
                this.settings.world_layout = way;
                this.save_settings();
            });
            (tr(label).into(), way == layout, pick)
        })
        .collect();
        rows.push(row(
            tr("Worlds on Home"),
            tr(match layout {
                backend::WorldLayout::Rows => "Continue playing lists one world a row.",
                backend::WorldLayout::Tiles => {
                    "Continue playing shows the worlds' pictures side by side, as many as fit across; the rest wrap under them."
                }
            }),
            {
                let state = &mut self.ui.settings.appearance;
                let focus = state
                    .worlds_focus
                    .get_or_insert_with(|| cx.focus_handle().tab_index(0).tab_stop(true))
                    .clone();
                // Opened from the search: the page scrolls here and the keyboard is on it.
                let asked = state.worlds_asked.clone().map(|done| {
                    crate::mods::reveal(
                        crate::frame::scrolls("settings"),
                        done,
                        Some(focus.clone()),
                    )
                });
                div().child(
                    crate::mods::choices("alt-world-layout", layouts, cx)
                        .track_focus(&focus)
                        .gap(px(6.))
                        .children(asked),
                )
            },
        ));
        rows.push(
            self.wide_row(
                tr("Your themes"),
                tr("A theme is a folder with a theme.toml: up to six colours and one picture. Reload reads the folder again."),
                div()
                    .child(
                        button("alt-themes-folder", tr("Open themes folder")).on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                let dir = theme_files::dir();
                                // A link there is not the launcher's folder: never opened
                                // or made through.
                                if theme_files::root_linked(&dir) {
                                    this.ui.settings.appearance.error = Some(format!(
                                        "{}: {}",
                                        dir.display(),
                                        theme_files::Reason::Linked("folder").text()
                                    ));
                                    cx.notify();
                                    return;
                                }
                                match std::fs::create_dir_all(&dir) {
                                    Ok(()) => crate::glass_toast::open(cx, &dir),
                                    Err(e) => {
                                        this.ui.settings.appearance.error =
                                            Some(format!("{}: {e}", dir.display()))
                                    }
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(button("alt-themes-reload", tr("Reload")).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| {
                            this.appearance_reload(window);
                            cx.notify();
                        },
                    ))),
            ),
        );
        rows.push(self.corners_row(cx));
        let cat = self.settings.show_cat();
        rows.push(row(
            tr("Show the cat"),
            tr("It visits Home now and then. It stays away while motion is reduced."),
            div().child(switch("alt-cat-toggle", cat).seen().on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| {
                    this.settings.cat = Some(!cat);
                    this.save_settings();
                    cx.notify();
                },
            ))),
        ));
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(group(rows))
            .children(said)
    }

    fn panel_opacity_row(&self, cx: &mut Context<Self>) -> Div {
        let saved = self.settings.panel_opacity();
        let control = &self.ui.settings.appearance.panel_opacity;
        let actual = (palette().alpha * 100.).round() as u8;
        let chosen = control.preview().map(opacity_percent).or(saved);
        let percent = chosen.unwrap_or(actual);
        let thumb = (f32::from(percent) - f32::from(PANEL_OPACITY_MIN)) / OPACITY_SPAN;
        let slider = control.render(
            "panel-opacity",
            thumb.clamp(0., 1.),
            1. / OPACITY_SPAN,
            |this, value, cx| {
                this.settings
                    .set_panel_opacity(Some(opacity_percent(value)));
                this.save_settings();
                cx.notify();
            },
            cx,
        );
        let reset = button("panel-opacity-reset", tr("Use default"));
        let reset = if saved.is_none() {
            off(reset)
        } else {
            reset.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.settings.set_panel_opacity(None);
                this.save_settings();
                cx.notify();
            }))
        };
        let about = if !self.settings.scenery() {
            tr("Turn on scenery to see how the backdrop shows through panels.")
        } else if chosen.is_some() && actual > percent {
            tr("This theme stays more opaque so its text remains readable.")
        } else {
            tr("How much of the backdrop shows through cards and panels.")
        };
        self.wide_row(
            tr("Panel opacity"),
            about,
            div()
                .child(slider)
                .child(
                    div()
                        .min_w(px(44.))
                        .text_size(px(tokens::LABEL))
                        .text_center()
                        .child(format!("{percent}%")),
                )
                .child(reset),
        )
    }

    /// The accent: the theme's own colour and the named ones to pick from, and one field that
    /// takes any name, hue or `#rrggbb`. What is typed is taken on Enter or when the field is
    /// left; Escape leaves the accent as it was.
    fn accent_row(&mut self, missing: bool, window: &Window, cx: &mut Context<Self>) -> Div {
        let state = &mut self.ui.settings.appearance;
        let focus = state
            .accent_focus
            .get_or_insert_with(|| cx.focus_handle().tab_index(0).tab_stop(true))
            .clone();
        let focused = focus.is_focused(window);
        let left = std::mem::replace(&mut state.accent_editing, focused) && !focused;
        let shown = appearance::accent(&self.settings);
        let typed = state.accent.text().trim().to_string();
        if left && !missing && !typed.is_empty() && typed != shown.name() {
            self.appearance_change(|s| appearance::type_accent(s, &typed), window);
            // This frame has started in the old colours.
            cx.notify();
        }
        let shown = appearance::accent(&self.settings);
        let state = &mut self.ui.settings.appearance;
        if !focused {
            state.accent.set(shown.name());
        }
        let own = themes::own_accent(self.settings.theme.as_deref());
        let edit: fn(&mut Self) -> &mut line_edit::LineEdit =
            |this| &mut this.ui.settings.appearance.accent;
        let field_theme = crate::mods::field_theme(self.theme);
        let to_focus = focus.clone();
        let field = crate::frame::input(focused)
            .id("alt-accent")
            .track_focus(&focus)
            .seen()
            .w(px(150.))
            .h(px(tokens::FIELD))
            .px(px(12.))
            .font_family(theme::MONO)
            .text_size(px(tokens::LABEL))
            .text_color(rgb(palette().text))
            .cursor_text()
            .child(line_edit::content(
                &self.ui.settings.appearance.accent,
                &field_theme,
                focused,
                false,
                false,
                "teal, 176 or #2563eb",
            ))
            .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                window.focus(&to_focus, cx);
                cx.notify();
            }))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                match line_edit::handle(edit(this), ev, cx) {
                    line_edit::Key::Pass => return,
                    // Enter takes what is typed; Escape leaves the accent as it was.
                    line_edit::Key::Enter => {
                        let typed = edit(this).text().to_string();
                        this.appearance_change(|s| appearance::type_accent(s, &typed), window);
                    }
                    line_edit::Key::Escape => {
                        let shown = appearance::accent(&this.settings).name();
                        edit(this).set(shown);
                        window.blur(cx);
                    }
                    _ => {}
                }
                cx.stop_propagation();
                cx.notify();
            }));
        let field = line_edit::mouse(field, cx, edit);
        // The theme's own colour first, then each named accent the theme doesn't already wear.
        let picks = std::iter::once((own, true)).chain(
            Accent::ALL
                .into_iter()
                .filter(|a| *a != own)
                .map(|a| (a, false)),
        );
        let mut swatches = div().flex().flex_wrap().items_center().gap(px(8.));
        for (i, (accent, theirs)) in picks.enumerate() {
            let on = accent == shown;
            let tip = if theirs {
                tr("Theme accent")
            } else {
                Accent::ALL
                    .iter()
                    .position(|a| *a == accent)
                    .map_or("", |i| tr(ACCENT_NAMES[i]))
            };
            let swatch = div()
                .id(("alt-accent-swatch", i))
                .flex_none()
                .size(px(SWATCH))
                .rounded_full()
                .p(px(3.))
                .border_2()
                .border_color(if on {
                    rgb(palette().text).into()
                } else {
                    gpui::transparent_black()
                })
                .reach_edge()
                .tooltip(crate::glass_ui::tip(tip))
                .child(div().size_full().rounded_full().bg(rgb(accent.rgb())));
            swatches = swatches.child(if missing || on {
                swatch.cursor_default()
            } else {
                swatch
                    .cursor_pointer()
                    .hover(|s| s.border_color(rgb(palette().muted)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        if theirs {
                            this.appearance_change(appearance::reset_accent, window);
                        } else {
                            this.appearance_change(|s| appearance::set_accent(s, accent), window);
                        }
                        cx.notify();
                    }))
            });
        }
        stacked(
            tr("Accent"),
            tr("Pick a colour, or type a colour name, a hue in degrees or #rrggbb. Play darkens a pale accent just enough for its white label."),
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(16.))
                .child(swatches.when(missing, |d| d.opacity(0.4)))
                .child(if missing {
                    div().opacity(0.4).child(field)
                } else {
                    div().child(field)
                }),
        )
    }

    /// What is behind Home: where it comes from, a picture of the player's own, the way back
    /// to the theme's, and the switch for all of it.
    fn background_row(&mut self, cx: &mut Context<Self>) -> Div {
        let data = backend::data_dir();
        // A kept copy Home could not decode is not what is behind Home.
        let unshown = self
            .ui
            .settings
            .appearance
            .unshown
            .as_ref()
            .is_some_and(|path| {
                appearance::behind_home(&self.settings, &data) == Behind::Personal(path.clone())
            });
        let behind = if unshown {
            appearance::behind_without_own(&self.settings)
        } else {
            appearance::behind_home(&self.settings, &data)
        };
        let lost = appearance::background_lost(&self.settings, &data);
        let source = match &behind {
            Behind::Plain => tr("Off: the theme's plain background."),
            Behind::Personal(_) => tr("Your picture, on Home."),
            Behind::Theme(_) => tr("The theme's picture, on Home."),
            Behind::Scene => tr("The theme's scene."),
        };
        let about = if unshown {
            format!(
                "{} {}",
                source,
                tr("The picture you chose can't be shown; choose it again or use the theme's.")
            )
        } else if lost {
            format!(
                "{} {}",
                source,
                tr("The picture you chose is no longer there; choose it again or use the theme's.")
            )
        } else {
            source.to_string()
        };
        let choose = button("alt-background-choose", tr("Choose picture…")).on_click(cx.listener(
            |_, _: &ClickEvent, window, cx| {
                let paths = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some(tr("Choose").into()),
                });
                let window = window.window_handle();
                cx.spawn(async move |this, cx| {
                    // Closing the dialog keeps the picture there was.
                    let Ok(Ok(Some(mut paths))) = paths.await else {
                        return;
                    };
                    let Some(file) = paths.pop() else {
                        return;
                    };
                    // Read, checked and decoded off the window's thread, then kept.
                    let kept = cx
                        .background_executor()
                        .spawn(async move { themes::keep_background(&backend::data_dir(), &file) })
                        .await;
                    let _ = window.update(cx, |_, window, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.appearance_change(
                                |s| {
                                    s.background = Some(kept?);
                                    Ok(())
                                },
                                window,
                            );
                            cx.notify();
                        });
                    });
                })
                .detach();
            },
        ));
        let theirs = button("alt-background-theme", tr("Use theme background"));
        let theirs = if self.settings.background.is_some() {
            theirs.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.appearance_change(
                    |s| {
                        s.background = None;
                        Ok(())
                    },
                    window,
                );
                cx.notify();
            }))
        } else {
            off(theirs)
        };
        self.wide_row(
            tr("Home background"),
            about,
            div().child(choose).child(theirs),
        )
    }

    /// Whether Home shows its background at all: a row of its own, its switch on the right like
    /// every other switch on the page.
    fn scenery_row(&mut self, cx: &mut Context<Self>) -> Div {
        let on = self.settings.scenery();
        let show = switch("alt-scenery", on).seen().on_click(cx.listener(
            move |this, _: &ClickEvent, window, cx| {
                this.appearance_change(
                    |s| {
                        s.scenery_on = Some(!on);
                        Ok(())
                    },
                    window,
                );
                cx.notify();
            },
        ));
        row(tr("Show background"), "", div().child(show))
    }
}

#[cfg(test)]
mod tests {
    use super::photo_tone;
    use image::{Rgba, RgbaImage};

    /// Runs `test` again in a child process with its own scratch homes; Some(root) inside it.
    fn scratch_child(test: &str) -> Option<std::path::PathBuf> {
        const CHILD: &str = "SUNRISE_APPEARANCE_TEST";
        if let Some(root) = std::env::var_os(CHILD) {
            return Some(root.into());
        }
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        let nowhere = "http://127.0.0.1:9";
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("settings::appearance::tests::{test}")])
            .env(CHILD, root)
            .env("SUNRISE_HOME", root.join("game"))
            .env("SUNRISE_LAUNCHER_HOME", root.join("launcher"))
            .env("SUNRISE_LAUNCHER_LOCATION_FILE", root.join("location.json"))
            .env("SUNRISE_NO_KEYCHAIN", "1")
            .env("SUNRISE_LAUNCHER_NO_SELF_UPDATE", "1")
            .env("SUNRISE_TELEMETRY_URL", "off")
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("HTTP_PROXY", nowhere)
            .env("HTTPS_PROXY", nowhere)
            .env("ALL_PROXY", nowhere)
            .env_remove("NO_PROXY")
            .env_remove("SUNRISE_GITHUB_MIRROR")
            .env_remove("SUNRISE_LAUNCHER_BENCH")
            .status()
            .unwrap();
        assert!(status.success(), "{test} failed in its child process");
        None
    }

    #[test]
    fn a_typed_accent_is_taken_when_the_field_is_left_without_enter() {
        use sunrise_launcher::appearance;
        use sunrise_launcher::backend::Settings;
        use sunrise_launcher::themes::Accent;
        if scratch_child("a_typed_accent_is_taken_when_the_field_is_left_without_enter").is_none() {
            return;
        }
        let mut app = gpui::TestAppContext::single();
        let (shell, cx) = app.add_window_view(crate::window_root::Shell::new_for_test);
        let launcher = shell.read_with(cx, |shell, _| shell.launcher());
        launcher.update(cx, |l, cx| {
            l.navigate(crate::frame::Page::Settings);
            l.settings_open(super::Section::Appearance);
            cx.notify();
        });
        cx.run_until_parked();
        // The field is below the fold of the test window: give it the keyboard directly.
        let focus = launcher.read_with(cx, |l, _| {
            let focus = l.ui.settings.appearance.accent_focus.clone();
            focus.expect("the accent field is drawn")
        });
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("ctrl-a t e a l");
        cx.run_until_parked();
        let accent = || appearance::accent(&Settings::load());
        assert_ne!(accent(), Accent::TEAL, "not taken before the field is left");
        cx.update(|window, cx| window.blur(cx));
        cx.run_until_parked();
        assert_eq!(accent(), Accent::TEAL);
    }

    #[test]
    fn picture_tone_is_weighted_by_visible_coverage() {
        let mut image = RgbaImage::from_pixel(3, 1, Rgba([255, 0, 0, 0]));
        assert_eq!(photo_tone(&image), None);
        image.put_pixel(1, 0, Rgba([90, 150, 210, 60]));
        image.put_pixel(2, 0, Rgba([30, 90, 150, 120]));
        assert_eq!(photo_tone(&image), Some(0x326eaa));
    }

    #[test]
    fn complementary_colours_do_not_cancel_a_pictures_tint() {
        let mut image = RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255]));
        image.put_pixel(1, 0, Rgba([0, 255, 255, 255]));
        assert_eq!(photo_tone(&image), Some(0xff0000));
    }
}
