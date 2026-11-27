//! Collections: the index's curated lists and the cards they hold.
use super::*;

/// A publisher's collection, its members resolved to their Discover cards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::mods) struct V2Collection {
    pub(in crate::mods) id: String,
    pub(in crate::mods) name: String,
    pub(in crate::mods) summary: String,
    pub(in crate::mods) description: String,
    pub(in crate::mods) publisher: String,
    /// Verified content-addressed relative path (`pages/<id>/<sha256>.<png|jpg>`), if valid.
    pub(in crate::mods) icon: Option<String>,
    /// Each member's position in the cards, and whether it is picked when the collection opens.
    pub(in crate::mods) members: Vec<(usize, bool)>,
}

impl V2Collection {
    /// How many members have a release for the selected build.
    pub(in crate::mods) fn fitting(&self, cards: &[V2Card]) -> usize {
        self.members
            .iter()
            .filter(|&&(i, _)| cards[i].fit.fits())
            .count()
    }

    /// What opening the collection picks: the members on by default that fit the build.
    pub(in crate::mods) fn initial_picks(&self, cards: &[V2Card]) -> BTreeSet<String> {
        self.members
            .iter()
            .filter(|&&(i, default)| default && cards[i].fit.fits())
            .map(|&(i, _)| cards[i].id.clone())
            .collect()
    }

    /// Install's label for `count` members, and when it takes only some, a tooltip saying why
    /// the rest wait, rather than a bare Install beside "1 of 4 mods fit build".
    pub(in crate::mods) fn install_label(
        &self,
        cards: &[V2Card],
        count: usize,
        installed: &dyn Fn(&str) -> bool,
    ) -> (String, Option<String>) {
        let all = self.members.len();
        let unfit = all - self.fitting(cards);
        let optional = self
            .members
            .iter()
            .filter(|&&(i, default)| !default && cards[i].fit.fits() && !installed(&cards[i].id))
            .count();
        let count = count.to_string();
        let of = [("count", count.as_str()), ("all", &all.to_string())];
        let why = match (unfit > 0, optional > 0) {
            (true, false) => Some(trf(
                "Installs {count} of its {all} mods: the rest don't fit this build yet.",
                &of,
            )),
            (false, true) => Some(trf(
                "Installs {count} of its {all} mods: the rest are optional. Open it to add them.",
                &of,
            )),
            (true, true) => Some(trf(
                "Installs {count} of its {all} mods: some don't fit this build yet and some are optional. Open it to choose.",
                &of,
            )),
            (false, false) => None,
        };
        let label = match why {
            Some(_) => trf("Install {count}", &[("count", &count)]),
            None => tr("Install").to_owned(),
        };
        (label, why)
    }

    /// What Install installs: the picked members that fit and aren't installed, in the
    /// collection's order. A member that doesn't fit the build is left out.
    pub(in crate::mods) fn to_install(
        &self,
        cards: &[V2Card],
        picked: &BTreeSet<String>,
        installed: &dyn Fn(&str) -> bool,
    ) -> Vec<String> {
        self.members
            .iter()
            .map(|&(i, _)| &cards[i])
            .filter(|card| card.fit.fits() && picked.contains(&card.id) && !installed(&card.id))
            .map(|card| card.id.clone())
            .collect()
    }
}

/// The index's collections, each member found among `cards`.
pub(in crate::mods) fn collections(index: &Index, cards: &[V2Card]) -> Vec<V2Collection> {
    let at: HashMap<&str, usize> = cards
        .iter()
        .enumerate()
        .map(|(i, card)| (card.id.as_str(), i))
        .collect();
    index
        .collections
        .iter()
        .map(|c| V2Collection {
            id: c.id.clone(),
            name: c.name.clone(),
            summary: c.summary.clone(),
            description: c.description.clone(),
            publisher: c.publisher.clone(),
            icon: content_addressed_path(&c.id, &c.icon),
            members: c
                .members
                .iter()
                .filter_map(|m| Some((*at.get(m.project.as_str())?, m.default)))
                .collect(),
        })
        .collect()
}

/// Browsing shows a collection's members only inside its card, Show all included; a search
/// still finds a member by name.
pub(in crate::mods) fn fold_members(
    shown: &mut Vec<usize>,
    collections: &[V2Collection],
    filters: &V2Filters,
) {
    if !filters.text.trim().is_empty() {
        return;
    }
    let folded: HashSet<usize> = collections
        .iter()
        .flat_map(|c| c.members.iter().map(|&(i, _)| i))
        .collect();
    shown.retain(|i| !folded.contains(i));
}

/// The collections Discover lists for `filters`: unless Show all is on, only those with a member
/// that fits, and under a category only those with such a member in it, since browsing folds
/// members into their collection there too. Search matches the collection's own text or the
/// text of a member listed under the same filters.
pub(in crate::mods) fn discover_collections(
    collections: &[V2Collection],
    cards: &[V2Card],
    filters: &V2Filters,
) -> Vec<usize> {
    let words = query_words(&filters.text);
    // The members that would be listed under these filters if they weren't folded.
    let listed = |i: usize| {
        let card = &cards[i];
        (filters.show_all || card.fit.fits())
            && filters
                .category
                .as_ref()
                .is_none_or(|k| card.categories.contains(k))
    };
    collections
        .iter()
        .enumerate()
        .filter(|(_, c)| c.members.iter().any(|&(i, _)| listed(i)))
        .filter(|(_, c)| {
            let own = [&c.name, &c.summary, &c.publisher].map(|s| s.to_lowercase());
            words.iter().all(|w| own.iter().any(|s| s.contains(w)))
                || (!words.is_empty()
                    && c.members
                        .iter()
                        .any(|&(i, _)| listed(i) && card_matches_query(&cards[i], &words)))
        })
        .map(|(i, _)| i)
        .collect()
}
