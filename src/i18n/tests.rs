use super::*;
use std::collections::BTreeSet;
use std::path::Path;

/// Every Rust file of the launcher's sources, with its path.
fn sources(dir: &Path, out: &mut Vec<(std::path::PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            out.push((path, text));
        }
    }
}

/// Read Rust tokens rather than quote positions: whitespace, comments, escaped and raw strings
/// must not hide a call or turn a quoted example into one. Recurse into macro bodies too.
fn strings_in(
    tokens: proc_macro2::TokenStream,
    asked: &mut BTreeSet<String>,
    all: &mut BTreeSet<String>,
) {
    use proc_macro2::{Delimiter, TokenTree};
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            TokenTree::Ident(name) if name == "tr" || name == "trf" || name == "tr_key" => {
                if let Some(TokenTree::Group(args)) = tokens.peek() {
                    if args.delimiter() == Delimiter::Parenthesis {
                        let first: Vec<TokenTree> = args
                            .stream()
                            .into_iter()
                            .take_while(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == ','))
                            .collect();
                        match first.first() {
                            Some(TokenTree::Literal(value)) => {
                                if let Some(value) = text_of(value) {
                                    asked.insert(value);
                                }
                            }
                            // `tr(if … { "a" } else { "b" })` and `tr(match … { … => "a" })`.
                            Some(TokenTree::Ident(word)) if word == "if" || word == "match" => {
                                branch_texts(first.into_iter().collect(), asked);
                            }
                            _ => {}
                        }
                    }
                }
            }
            TokenTree::Group(group) => strings_in(group.stream(), asked, all),
            TokenTree::Literal(value) => {
                if let Ok(value) = syn::parse_str::<syn::LitStr>(&value.to_string()) {
                    all.insert(value.value());
                }
            }
            _ => {}
        }
    }
}

fn text_of(literal: &proc_macro2::Literal) -> Option<String> {
    syn::parse_str::<syn::LitStr>(&literal.to_string())
        .ok()
        .map(|value| value.value())
}

/// The texts an `if` or `match` chooses between: a literal that is a block's whole body
/// (`{ "a" }`) or a match arm's whole value (`=> "a",`), at any depth.
fn branch_texts(tokens: proc_macro2::TokenStream, asked: &mut BTreeSet<String>) {
    use proc_macro2::{Delimiter, TokenTree};
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    if let [TokenTree::Literal(value)] = tokens.as_slice() {
        asked.extend(text_of(value));
        return;
    }
    for (i, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                branch_texts(group.stream(), asked);
            }
            TokenTree::Literal(value) => {
                let after_arrow = i >= 2
                    && matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == '>')
                    && matches!(&tokens[i - 2], TokenTree::Punct(p) if p.as_char() == '=');
                let ends_arm = match tokens.get(i + 1) {
                    None => true,
                    Some(TokenTree::Punct(p)) => p.as_char() == ',',
                    _ => false,
                };
                if after_arrow && ends_arm {
                    asked.extend(text_of(value));
                }
            }
            _ => {}
        }
    }
}

/// The English strings requested directly, plus every literal (some reach `tr` via a widget or
/// constant). The tables' shared key set below covers those indirect translations.
fn strings() -> (BTreeSet<String>, BTreeSet<String>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "crates/smod/src", "crates/sdk/src"] {
        sources(&root.join(dir), &mut files);
    }
    let parsed: Vec<(&Path, proc_macro2::TokenStream)> = files
        .iter()
        .map(|(path, text)| (path.as_path(), text.parse().expect("valid Rust tokens")))
        .collect();
    let (mut asked, mut all) = (BTreeSet::new(), BTreeSet::new());
    let mut helpers = Vec::new();
    for (path, tokens) in &parsed {
        strings_in(tokens.clone(), &mut asked, &mut all);
        helpers_in(tokens.clone(), path, &mut helpers);
    }
    for (path, tokens) in &parsed {
        helper_calls(tokens.clone(), path, &helpers, &mut asked);
    }
    // An empty label or a lone "…" passed to a helper is no text to translate.
    asked.retain(|key| key.chars().any(char::is_alphabetic));
    (asked, all)
}

/// A function or closure that hands one of its parameters straight to `tr` or `trf`
/// (`fn label(text: &str) -> Div { caption(tr(text)) }`): a literal its callers pass there is a
/// key too. Its calls count only in its own folder, so another function of the same name
/// elsewhere adds nothing.
struct Helper {
    name: String,
    /// Where the calls that count are: the definer's folder, or its file for a closure.
    scope: std::path::PathBuf,
    /// The translated parameters' positions, as a call writes its arguments.
    at: Vec<usize>,
    /// Called as a method, `self` is not among the arguments.
    method: bool,
}

/// `tokens` split at its top-level commas; a comma inside `<…>` belongs to a type.
fn split_commas(tokens: proc_macro2::TokenStream) -> Vec<Vec<proc_macro2::TokenTree>> {
    use proc_macro2::TokenTree;
    let (mut parts, mut part, mut angle) = (Vec::new(), Vec::new(), 0usize);
    let mut last_dash = false;
    for token in tokens {
        if let TokenTree::Punct(p) = &token {
            match p.as_char() {
                '<' => angle += 1,
                '>' if !last_dash => angle = angle.saturating_sub(1),
                ',' if angle == 0 => {
                    parts.push(std::mem::take(&mut part));
                    last_dash = false;
                    continue;
                }
                _ => {}
            }
            last_dash = p.as_char() == '-';
        } else {
            last_dash = false;
        }
        part.push(token);
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

/// The parameter a pattern names: `text: &str` and `mut text: String` name `text`.
fn param_name(part: &[proc_macro2::TokenTree]) -> Option<String> {
    use proc_macro2::TokenTree;
    part.iter().find_map(|token| match token {
        TokenTree::Ident(name) if name != "mut" => Some(name.to_string()),
        _ => None,
    })
}

/// Whether `body` passes `name`, alone or borrowed, as the first argument of `tr` or `trf`.
fn translates(body: proc_macro2::TokenStream, name: &str) -> bool {
    use proc_macro2::{Delimiter, TokenTree};
    let tokens: Vec<TokenTree> = body.into_iter().collect();
    tokens.iter().enumerate().any(|(i, token)| match token {
        TokenTree::Ident(word) if word == "tr" || word == "trf" => match tokens.get(i + 1) {
            Some(TokenTree::Group(args)) if args.delimiter() == Delimiter::Parenthesis => {
                let first = split_commas(args.stream())
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let first: Vec<_> = first
                    .iter()
                    .filter(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == '&'))
                    .collect();
                matches!(first.as_slice(), [TokenTree::Ident(arg)] if arg == name)
                    || translates(args.stream(), name)
            }
            _ => false,
        },
        TokenTree::Group(group) => translates(group.stream(), name),
        _ => false,
    })
}

fn helper(
    name: String,
    scope: &Path,
    params: proc_macro2::TokenStream,
    body: proc_macro2::TokenStream,
) -> Option<Helper> {
    let params = split_commas(params);
    let method = params
        .first()
        .and_then(|first| param_name(first))
        .is_some_and(|first| first == "self");
    let at: Vec<usize> = params
        .iter()
        .enumerate()
        .filter(|(_, part)| {
            param_name(part).is_some_and(|p| p != "self" && translates(body.clone(), &p))
        })
        .map(|(i, _)| i - usize::from(method))
        .collect();
    (!at.is_empty()).then(|| Helper {
        name,
        scope: scope.to_path_buf(),
        at,
        method,
    })
}

/// The helpers `tokens` defines: `fn name(…) … { … }` and `let name = |…| …;`.
fn helpers_in(tokens: proc_macro2::TokenStream, path: &Path, out: &mut Vec<Helper>) {
    use proc_macro2::{Delimiter, TokenTree};
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let folder = path.parent().unwrap_or(path);
    for (i, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Ident(word) if word == "fn" => {
                let Some(TokenTree::Ident(name)) = tokens.get(i + 1) else {
                    continue;
                };
                let rest = &tokens[i + 2..];
                let Some(params) = rest.iter().find_map(|t| match t {
                    TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g),
                    _ => None,
                }) else {
                    continue;
                };
                // A declaration without a body (`fn name(&self);`) ends at its semicolon.
                let body = rest
                    .iter()
                    .take_while(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == ';'))
                    .find_map(|t| match t {
                        TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => Some(g),
                        _ => None,
                    });
                if let Some(body) = body {
                    out.extend(helper(
                        name.to_string(),
                        folder,
                        params.stream(),
                        body.stream(),
                    ));
                }
            }
            TokenTree::Ident(word) if word == "let" => {
                let (
                    Some(TokenTree::Ident(name)),
                    Some(TokenTree::Punct(eq)),
                    Some(TokenTree::Punct(bar)),
                ) = (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3))
                else {
                    continue;
                };
                if eq.as_char() != '=' || bar.as_char() != '|' {
                    continue;
                }
                let Some(close) = tokens[i + 4..]
                    .iter()
                    .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == '|'))
                else {
                    continue;
                };
                let params: proc_macro2::TokenStream =
                    tokens[i + 4..i + 4 + close].iter().cloned().collect();
                let body: proc_macro2::TokenStream = tokens[i + 5 + close..]
                    .iter()
                    .take_while(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == ';'))
                    .cloned()
                    .collect();
                out.extend(helper(name.to_string(), path, params, body));
            }
            TokenTree::Group(group) => helpers_in(group.stream(), path, out),
            _ => {}
        }
    }
}

/// The literals `tokens` passes where a helper in scope translates them.
fn helper_calls(
    tokens: proc_macro2::TokenStream,
    path: &Path,
    helpers: &[Helper],
    asked: &mut BTreeSet<String>,
) {
    use proc_macro2::{Delimiter, TokenTree};
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    for (i, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Ident(name) => {
                let Some(TokenTree::Group(args)) = tokens.get(i + 1) else {
                    continue;
                };
                if args.delimiter() != Delimiter::Parenthesis
                    || matches!(i.checked_sub(1).map(|j| &tokens[j]), Some(TokenTree::Ident(w)) if w == "fn")
                {
                    continue;
                }
                let dotted = matches!(i.checked_sub(1).map(|j| &tokens[j]), Some(TokenTree::Punct(p)) if p.as_char() == '.');
                let args = split_commas(args.stream());
                for helper in helpers.iter().filter(|h| {
                    *name == h.name && path.starts_with(&h.scope) && (!h.method || dotted)
                }) {
                    for &at in &helper.at {
                        if let Some([TokenTree::Literal(value)]) = args.get(at).map(Vec::as_slice) {
                            asked.extend(text_of(value));
                        }
                    }
                }
            }
            TokenTree::Group(group) => helper_calls(group.stream(), path, helpers, asked),
            _ => {}
        }
    }
}

/// A literal handed to a function or closure that translates that parameter is a key; one
/// handed elsewhere, or to a same-named function in another folder, is not.
#[test]
fn the_scan_follows_literals_into_helpers_that_translate_them() {
    let code = r#"
        fn label(text: &'static str) -> Div { caption(tr(text)) }
        fn part(&self, title: &str, n: u8) -> Div { row(tr(&title), n) }
        fn other(text: &str) -> String { text.to_string() }
        trait Shown { fn shown(&self, text: &str); }
        fn render() {
            let small = |id: &str, says: &str| tip(tr(says));
            label("Online now");
            self.part("Difficulty", 1);
            small("alt-play", "Play this world");
            other("not a key");
        }
    "#;
    let (here, elsewhere) = (
        Path::new("/src/hosting/page.rs"),
        Path::new("/src/other/page.rs"),
    );
    let mut helpers = Vec::new();
    helpers_in(code.parse().unwrap(), here, &mut helpers);
    let mut asked = BTreeSet::new();
    helper_calls(code.parse().unwrap(), here, &helpers, &mut asked);
    assert_eq!(
        asked,
        ["Difficulty", "Online now", "Play this world"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    let mut asked = BTreeSet::new();
    helper_calls(
        r#"label("Somewhere else");"#.parse().unwrap(),
        elsewhere,
        &helpers,
        &mut asked,
    );
    assert!(asked.is_empty(), "{asked:?}");
}

#[test]
fn the_scan_finds_multiline_calls_without_reading_comments_or_quoted_code() {
    let code = r###"
        tr
        (
            "multiline"
        );
        trf(/* before the string */ "hello {name}", &[("name", "player")]);
        some_macro!(crate::i18n::tr(r#"raw "quotes""#));
        tr("escaped \"quotes\" and \u{4e2d}\nnext");
        tr("continued \
            line");
        // tr("line comment");
        /* tr("block comment"); */
        let example = r#"tr("quoted code")"#;
        other_tr("not a translation");
        trf_other("not a translation either");
        let indirect = "widget label";
        tr(indirect);
        tr(if on { "chosen on" } else { "chosen off" });
        trf(match n { 1 => "one {n}", _ => { "many {n}" } }, &[("n", "2")]);
        tr(match way { Way::A => compute("not a branch"), Way::B => "branch b" });
        const LATER: &str = tr_key("marked key");
        trf(en, &[("name", "not a key")]);
    "###;
    let (mut asked, mut all) = (BTreeSet::new(), BTreeSet::new());
    strings_in(code.parse().unwrap(), &mut asked, &mut all);
    assert_eq!(
        asked,
        [
            "multiline",
            "hello {name}",
            "raw \"quotes\"",
            "escaped \"quotes\" and 中\nnext",
            "continued line",
            "chosen on",
            "chosen off",
            "one {n}",
            "many {n}",
            "branch b",
            "marked key",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
    assert!(all.contains("widget label"));
    assert!(asked.is_subset(&all));
    assert!(!all.contains("line comment"));
    assert!(!all.contains("block comment"));
}

fn holes(s: &str) -> BTreeSet<&str> {
    s.split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect()
}

/// A profile has one name wherever a player reads it. "Setup" is only the Windows installer,
/// whose window is not translated through `tr`; "instance" is only the command's name, which
/// a sentence may quote (`sunrise-launcher instance select`, `--instance`).
#[test]
fn a_profile_is_never_called_a_setup_or_an_instance() {
    let (asked, _) = strings();
    let others: Vec<_> = asked
        .iter()
        .filter(|said| {
            let said = said
                .to_lowercase()
                .replace("sunrise-launcher instance", "")
                .replace("--instance", "")
                .replace("{instance}", "");
            said.contains("setup") || said.contains("instance")
        })
        .collect();
    assert!(others.is_empty(), "{others:#?}");
}

#[test]
fn translations_are_valid_and_coverage_is_reported() {
    let (mut asked, all) = strings();
    assert!(asked.contains("Language"), "the scan finds tr(\"…\")");
    let tables: Vec<_> = Language::ALL
        .into_iter()
        .filter(|language| *language != Language::English)
        .map(|language| {
            let table: HashMap<String, String> = serde_json::from_str(language.source())
                .unwrap_or_else(|e| panic!("lang/{}.json: {e}", language.code()));
            (language, table)
        })
        .collect();
    // Diverges from upstream: tables are sparse, using the existing English-source fallback.
    // Missing and stale entries are coverage information, not failures that force every new
    // label to edit every language. Keep existing translations; never add dummy English entries.
    // A live label passed via a widget/constant is also known once any table translates it.
    for (_, table) in &tables {
        asked.extend(table.keys().filter(|key| all.contains(*key)).cloned());
    }
    // Show the detailed lists with SUNRISE_LAUNCHER_I18N_DETAILS=1 cargo test
    // --lib i18n::tests::translations_are_valid_and_coverage_is_reported
    use std::fmt::Write as _;
    use std::io::Write as _;
    let show_details = std::env::var("SUNRISE_LAUNCHER_I18N_DETAILS").as_deref() == Ok("1");
    let (mut missing_count, mut stale_count) = (0, 0);
    let mut details = String::new();
    for (language, table) in &tables {
        let missing: Vec<_> = asked.iter().filter(|s| !table.contains_key(*s)).collect();
        let stale: BTreeSet<_> = table.keys().filter(|s| !all.contains(*s)).collect();
        missing_count += missing.len();
        stale_count += stale.len();
        if show_details && (!missing.is_empty() || !stale.is_empty()) {
            writeln!(
                details,
                "lang/{}.json missing: {missing:#?}; stale: {stale:#?}",
                language.code()
            )
            .unwrap();
        }
        for (en, said) in table {
            assert_eq!(holes(en), holes(said), "{}: {en:?}", language.code());
            assert!(
                !said.trim().is_empty(),
                "{}: {en:?} is empty",
                language.code()
            );
        }
    }
    // Write directly so the single summary is visible even when libtest captures passing tests.
    let mut out = std::io::stdout().lock();
    writeln!(out, "Launcher locale coverage: {} languages, {missing_count} missing entries, {stale_count} stale entries (SUNRISE_LAUNCHER_I18N_DETAILS=1 for lists).", tables.len()).unwrap();
    if show_details {
        write!(out, "{details}").unwrap();
    }
}

/// A value is inserted as it is: a data folder whose path contains `{file}` must not have the
/// next argument filled into it, or a recovery message names the wrong file to remove.
#[test]
fn a_filled_value_is_never_filled_in_turn() {
    let said = fill(
        "{key} is damaged. Remove it and {file} and sign in again.",
        &[
            ("key", "/home/a/{file}/accounts.key"),
            ("file", "/home/a/{file}/accounts.json"),
        ],
    );
    assert_eq!(
        said,
        "/home/a/{file}/accounts.key is damaged. Remove it and /home/a/{file}/accounts.json and sign in again."
    );
    // A hole named twice is filled both times; braces that name no argument stay as written.
    assert_eq!(fill("{a}-{a} {b} {{a}} {", &[("a", "1")]), "1-1 {b} {1} {");
}

/// `lang/en.json` is the source a translation service reads: every key asked for in the code is
/// in it, mapped to itself, or translators never see that string.
#[test]
fn the_english_catalog_lists_every_key() {
    let (asked, _) = strings();
    let english: HashMap<String, String> =
        serde_json::from_str(Language::English.source()).expect("lang/en.json is valid");
    let missing: Vec<_> = asked.iter().filter(|s| !english.contains_key(*s)).collect();
    assert!(
        missing.is_empty(),
        "add these with tools/launcher_add_lang_keys.py: {missing:#?}"
    );
    for (key, said) in &english {
        assert_eq!(key, said, "lang/en.json maps a key to other text");
    }
}

#[test]
fn the_system_language_is_the_first_one_the_launcher_has() {
    assert_eq!(Language::pick(["pl-PL", "en-US"]), Language::Polish);
    assert_eq!(Language::pick(["ja-JP", "en-US"]), Language::Japanese);
    assert_eq!(Language::parse("ja_JP.UTF-8"), Some(Language::Japanese));
    assert_eq!(Language::pick(["tr-TR", "en-US"]), Language::Turkish);
    assert_eq!(Language::parse("tr_TR.UTF-8"), Some(Language::Turkish));
    assert_eq!(Language::parse("pl_PL.UTF-8"), Some(Language::Polish));
    assert_eq!(Language::pick(["fr-CH", "en-US"]), Language::French);
    assert_eq!(Language::pick(["it-IT", "ja"]), Language::Italian);
    assert_eq!(Language::pick(["tlh", "ru_RU.UTF-8"]), Language::Russian);
    assert_eq!(Language::pick(["tlh", "zh-Hans-CN"]), Language::Chinese);
    assert_eq!(Language::pick(["tlh", "ja"]), Language::Japanese);
    assert_eq!(Language::pick(["de-CH", "ru_RU.UTF-8"]), Language::German);
    assert_eq!(Language::pick(["de-DE", "ja"]), Language::German);
    assert_eq!(Language::pick(["de-DE", "zh-Hans-CN"]), Language::German);
    assert_eq!(Language::pick(["es-ES", "en-US"]), Language::Spanish);
    assert_eq!(
        Language::pick(["pt-BR", "en-US"]),
        Language::PortugueseBrazil
    );
    assert_eq!(Language::pick(["ko", "it-IT"]), Language::Korean);
    assert_eq!(Language::pick(["uk-UA", "ru-RU"]), Language::Ukrainian);
    assert_eq!(Language::parse("uk_UA.UTF-8"), Some(Language::Ukrainian));
    assert_eq!(
        Language::parse("pt_BR.UTF-8"),
        Some(Language::PortugueseBrazil)
    );
    assert_eq!(Language::parse("pt-BR"), Some(Language::PortugueseBrazil));
    assert_eq!(Language::parse("pt-PT"), Some(Language::Portuguese)); // European Portuguese now has its own table
    assert_eq!(Language::parse("it_IT.UTF-8"), Some(Language::Italian));
    assert_eq!(Language::pick(["nb-NO", "en-US"]), Language::Norwegian);
    assert_eq!(Language::parse("nn_NO.UTF-8"), Some(Language::Norwegian));
    assert_eq!(Language::pick([]), Language::English);
    assert_eq!(Language::parse("FR"), Some(Language::French));
    assert_eq!(Language::parse("french"), None);
    assert_eq!(Language::parse("DE"), Some(Language::German));
    assert_eq!(Language::parse("ES"), Some(Language::Spanish));
    assert_eq!(Language::parse("german"), None);
    assert_eq!(Language::parse("ZH"), Some(Language::Chinese));
    assert_eq!(serde_json::to_string(&Language::German).unwrap(), "\"de\"");
    assert_eq!(serde_json::to_string(&Language::Chinese).unwrap(), "\"zh\"");
    assert_eq!(serde_json::to_string(&Language::Russian).unwrap(), "\"ru\"");
    assert_eq!(serde_json::to_string(&Language::Spanish).unwrap(), "\"es\"");
    assert_eq!(
        serde_json::to_string(&Language::PortugueseBrazil).unwrap(),
        "\"pt-br\""
    );
}

#[test]
fn a_missing_string_shows_in_english() {
    assert_eq!(tr_in(Language::Polish, "Settings"), "Ustawienia");
    assert_eq!(tr_in(Language::Japanese, "Settings"), "設定");
    assert_eq!(tr_in(Language::Turkish, "Settings"), "Ayarlar");
    assert_eq!(tr_in(Language::French, "Settings"), "Paramètres");
    assert_eq!(tr_in(Language::Russian, "Settings"), "Настройки");
    assert_eq!(tr_in(Language::German, "Settings"), "Einstellungen");
    assert_eq!(tr_in(Language::Chinese, "Settings"), "设置");
    assert_eq!(tr_in(Language::Spanish, "Settings"), "Ajustes");
    assert_eq!(tr_in(Language::Ukrainian, "Settings"), "Налаштування");
    assert_eq!(tr_in(Language::English, "Settings"), "Settings");
    assert_eq!(tr_in(Language::French, "No such string"), "No such string");
    assert_eq!(
        tr_in(Language::PortugueseBrazil, "Settings"),
        "Configurações"
    );
}

#[test]
fn polish_entries_are_valid_and_keep_placeholders() {
    let table: HashMap<String, String> = serde_json::from_str(Language::Polish.source()).unwrap();
    for (en, said) in &table {
        assert!(!said.trim().is_empty(), "empty Polish string: {en:?}");
        assert_eq!(holes(en), holes(said), "Polish: {en:?}");
    }
}

/// The duplicate-name error and the reminder to check worlds and profiles before deleting the
/// old folders are said in every language the launcher has, not in English: renaming a key
/// carries its translation along, with the name and the paths still filled in.
#[test]
fn the_renamed_profile_sentences_kept_their_translations() {
    let taken = "There is already a profile called {name}. Choose another name.";
    let moved = "Data moved. Old folders still use disk space: {paths}. Check that your worlds and profiles work in the new location before deleting the old folders.";
    let moves = "Moves all launcher, game and build data, including settings, worlds, profiles and installed versions. Choose an empty folder with enough free space.";
    for language in Language::ALL
        .into_iter()
        .filter(|language| *language != Language::English)
    {
        for key in [taken, moved, moves] {
            let said = tr_in(language, key);
            assert_ne!(said, key, "{} says it in English", language.code());
            assert_eq!(holes(said), holes(key), "{}: {said}", language.code());
        }
    }
    assert_eq!(
        tr_in(Language::French, taken),
        "Un profil nommé {name} existe déjà. Choisissez un autre nom."
    );
    assert_eq!(
        tr_in(Language::German, taken),
        "Es gibt bereits ein Profil namens {name}. Wähle einen anderen Namen."
    );
    assert_eq!(
        tr_in(Language::Japanese, taken),
        "{name}という名前のプロファイルは既にあります。別の名前を選んでください。"
    );
    assert!(tr_in(Language::French, moved).contains("vos mondes et vos profils"));
    assert!(tr_in(Language::Polish, moved).contains("światy i profile"));
    // Deletion applies to the old folders, never to the worlds and profiles just checked.
    for (language, object) in [
        (Language::English, "deleting the old folders"),
        (Language::Italian, "eliminare le vecchie cartelle"),
        (Language::Catalan, "eliminar les carpetes antigues"),
        (Language::Venetian, "cavar łe cartełe vece"),
    ] {
        assert!(
            tr_in(language, moved).contains(object),
            "{}",
            language.code()
        );
    }
}
