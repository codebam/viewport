// SPDX-License-Identifier: GPL-3.0-or-later
//
// The .desktop scan the launcher runs.
//
// The shell is a web page, and a web page cannot read `XDG_DATA_DIRS` any more
// than it can read `/proc` — which is why the list is built here and handed
// over as a message, rather than the page walking the directories itself.
// What this file owns is the scan and the parse: which entries are allowed to
// be shown, what each one runs, and what it calls itself. Drawing the list is
// the shell's; starting what is chosen is the state's, with an activation
// token minted for the process it spawns.
//
// The parse is the freedesktop Desktop Entry specification, as much of it as
// a launcher needs: the `[Desktop Entry]` section, the fields a row can show
// and a launch can use, and the field codes in `Exec` dropped the way the
// specification says a launcher with no files and no URLs must drop them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One application the launcher can start.
#[derive(Debug, Clone)]
pub struct App {
    /// What the row draws.
    pub name: String,
    /// The command line, ready to hand to `/bin/sh`.
    pub exec: String,
    /// The freedesktop icon name, empty where the entry has none.
    pub icon: String,
    /// The row's second line: what the entry says it is for, when it says
    /// anything.
    pub detail: String,
    /// The app_id an activation token is minted under.
    pub app_id: String,
    /// Whether the entry wants a terminal around it.
    pub terminal: bool,
}

/// The size an icon is looked up at.
///
/// A row is a few pixels tall; the size only decides which of the sizes a
/// theme offers is the one worth sending. The tray's is 22 for the bar; the
/// launcher's rows are larger, and a 48 pixel icon scaled down reads better
/// than a 22 pixel one scaled up.
const ICON_SIZE: u32 = 48;

/// The largest icon a list message will carry.
///
/// A list is a hundred rows, and a megabyte of base64 per row is a frame
/// dropped per application. The row falls back to a letter for an icon that
/// large, which is what the tray does for an item with no icon at all.
const MAX_ICON_URL: usize = 32 * 1024;

/// The applications directories, most specific first.
///
/// The order is the specification's: a file the user wrote overrides one a
/// package installed, and one in `/usr/local` overrides one in `/usr`. A file
/// that exists in more than one of them is read from the first only.
pub fn directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(config) => dirs.push(PathBuf::from(config).join("applications")),
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".config/applications"));
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let data =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
    for dir in data.split(':').filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir).join("applications"));
    }
    dirs
}

/// What `NotShowIn` and `OnlyShowIn` are compared against.
///
/// `XDG_CURRENT_DESKTOP` is a colon-separated list, and an entry names one of
/// the parts: `NotShowIn=GNOME` on a `viewport:wlroots` session shows.
pub fn current_desktop() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The applications in `dirs`, in the order a menu draws them.
///
/// `desktop` is the list `NotShowIn` and `OnlyShowIn` are compared against.
/// A file that exists in more than one directory is read from the first one
/// only: the user's override is the whole point of the order.
pub fn scan(dirs: &[PathBuf], desktop: &[&str]) -> Vec<App> {
    let mut winners: HashMap<String, PathBuf> = HashMap::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            winners.entry(name.to_owned()).or_insert(path);
        }
    }

    let mut apps = Vec::new();
    for path in winners.values() {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Some(app) = entry(&text, desktop) {
            apps.push(app);
        }
    }
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    apps
}

/// One `.desktop` file as an application, or nothing where it is not one to
/// show.
///
/// `desktop` is the list `NotShowIn` and `OnlyShowIn` are compared against.
pub fn entry(text: &str, desktop: &[&str]) -> Option<App> {
    let mut e = Entry::default();
    let mut in_desktop_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_desktop_entry = section == "Desktop Entry";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // A locale variant is the same key in another language. There is no
        // locale negotiation here; the base key is the one shown.
        let key = key.split('[').next().unwrap();
        let value = value.trim();
        match key {
            "Type" => e.r#type = value.to_owned(),
            "Name" => e.name = value.to_owned(),
            "Exec" => e.exec = value.to_owned(),
            "Icon" => e.icon = value.to_owned(),
            "Categories" => e.categories = split_list(value),
            "Keywords" => e.keywords = split_list(value),
            "Terminal" => e.terminal = value.eq_ignore_ascii_case("true"),
            "Hidden" => e.hidden = value.eq_ignore_ascii_case("true"),
            "NoDisplay" => e.no_display = value.eq_ignore_ascii_case("true"),
            "NotShowIn" => e.not_show_in = split_list(value),
            "OnlyShowIn" => e.only_show_in = split_list(value),
            "TryExec" => e.try_exec = value.to_owned(),
            "StartupWMClass" => e.startup_wm_class = value.to_owned(),
            "URL" => e.url = Some(value.to_owned()),
            _ => {}
        }
    }

    // Absent `Type` is `Application` where there is an `Exec`, and `Link`
    // where there is a `URL` instead — and a link is not something a launcher
    // starts.
    let r#type = if e.r#type.is_empty() {
        if e.exec.trim().is_empty() && e.url.is_some() {
            "Link"
        } else {
            "Application"
        }
    } else {
        e.r#type.as_str()
    };
    if !r#type.eq_ignore_ascii_case("application") {
        return None;
    }
    if e.hidden || e.no_display {
        return None;
    }
    if e.name.trim().is_empty() || e.exec.trim().is_empty() {
        return None;
    }
    // The entry says which desktops it is not for, and the session is one of
    // them.
    if e.not_show_in
        .iter()
        .any(|d| desktop.iter().any(|c| c.eq_ignore_ascii_case(d)))
    {
        return None;
    }
    // The entry says which desktops it is for, and the session is not one of
    // them. An empty list says every one, which is why the check needs the
    // list to be non-empty first.
    if !e.only_show_in.is_empty()
        && !e
            .only_show_in
            .iter()
            .any(|d| desktop.iter().any(|c| c.eq_ignore_ascii_case(d)))
    {
        return None;
    }
    // The entry names a binary it will not show without. The first token is
    // the binary; the rest are arguments it checks for the same reason.
    if let Some(binary) = e.try_exec.split_whitespace().next() {
        if !which(binary) {
            return None;
        }
    }

    let name = e.name.trim().to_owned();
    let mut tokens = exec_tokens(&e.exec)?;
    drop_field_codes(&mut tokens, &name);
    let exec = sh_line(&tokens);

    // The class the window will announce, or the name of the binary it runs:
    // what an activation token is minted under, and what a session file
    // matches the window back against.
    let app_id = if !e.startup_wm_class.trim().is_empty() {
        e.startup_wm_class.trim().to_owned()
    } else {
        tokens
            .first()
            .map(|t| {
                Path::new(t)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(t)
                    .to_owned()
            })
            .unwrap_or_default()
    };

    // The row's second line. Keywords are what the author wrote for exactly
    // this; the main category is what the file system says, with the
    // `X-` prefix a vendor namespace puts on it.
    let detail = if !e.keywords.is_empty() {
        e.keywords
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    } else if let Some(main) = e.categories.first() {
        main.strip_prefix("X-").unwrap_or(main).to_owned()
    } else {
        String::new()
    };

    Some(App {
        name,
        exec,
        icon: e.icon.trim().to_owned(),
        detail,
        app_id,
        terminal: e.terminal,
    })
}

/// One `[Desktop Entry]`, as far as the launcher reads it.
#[derive(Default)]
struct Entry {
    r#type: String,
    name: String,
    exec: String,
    icon: String,
    categories: Vec<String>,
    keywords: Vec<String>,
    terminal: bool,
    hidden: bool,
    no_display: bool,
    not_show_in: Vec<String>,
    only_show_in: Vec<String>,
    try_exec: String,
    startup_wm_class: String,
    url: Option<String>,
}

/// A `;`-separated list, trimmed and with the empties out.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The tokens of an `Exec` value, quoted the way the specification quotes
/// them.
///
/// The only escapes inside a quote are `\"` and `\\`; a backslash anywhere
/// else, and a quote outside the quotes, are what they say.
fn exec_tokens(exec: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut quoted = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                has_token = true;
            }
            '\\' if quoted => match chars.next() {
                Some('"') => {
                    current.push('"');
                    has_token = true;
                }
                Some('\\') => {
                    current.push('\\');
                    has_token = true;
                }
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                    has_token = true;
                }
                None => {}
            },
            c if c.is_whitespace() && !quoted => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            _ => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(current);
    }
    (!tokens.is_empty()).then_some(tokens)
}

/// The field codes, dropped the way the specification says a launcher with no
/// files and no URLs must drop them.
///
/// `%f`, `%F`, `%u`, `%U`, `%d` and `%D` take an argument, and the argument
/// goes with the code; `%n`, `%N` and `%c` are the name; the rest — `%k`,
/// `%v`, anything else — have nothing to stand in for here and go. A code in
/// the middle of a token is removed from it rather than splitting the token,
/// and a trailing one on an argument-taking code still takes the next token
/// with it.
fn drop_field_codes(tokens: &mut Vec<String>, name: &str) {
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].clone();
        let mut cleaned = String::new();
        let mut skip_next = false;
        let mut chars = token.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                cleaned.push(c);
                continue;
            }
            let Some(code) = chars.next() else {
                break;
            };
            match code {
                'n' | 'N' | 'c' => cleaned.push_str(name),
                'f' | 'F' | 'u' | 'U' | 'd' | 'D' if chars.peek().is_none() => {
                    skip_next = true;
                }
                _ => {}
            }
        }
        if !cleaned.is_empty() {
            out.push(cleaned);
        }
        i += 1;
        if skip_next {
            i += 1;
        }
    }
    *tokens = out;
}

/// The tokens back into one line, quoted for `/bin/sh`.
///
/// A token that could not be read as anything but itself goes bare; the rest
/// are single-quoted, which is the one quoting a shell cannot misread. A
/// leading `~` is expanded first, because a quote is what stops the shell
/// from doing it.
fn sh_line(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|t| expand_tilde(t))
        .map(|t| sh_quote(&t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A leading `~`, which the shell would expand if it were bare and will not
/// expand inside the quotes `sh_line` puts on it.
fn expand_tilde(token: &str) -> String {
    if let Some(rest) = token.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    token.to_owned()
}

/// One word, quoted for `/bin/sh`.
///
/// The same rule `sh_line` applies to its tokens: bare where it could not be
/// read as anything but itself, single-quoted otherwise. For a command the
/// session names — the terminal a `Terminal=true` entry is run in — which may
/// be more than one word.
pub fn sh_quote(word: &str) -> String {
    if word.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '/' | '.' | ',' | '=' | ':' | '@' | '+' | '%')
    }) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Whether a binary a `TryExec` names is anywhere on the `PATH`.
fn which(binary: &str) -> bool {
    let path = Path::new(binary);
    if path.is_absolute() {
        return path.is_file();
    }
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .any(|dir| Path::new(dir).join(binary).is_file())
}

/// The icon an entry names, as a `data:` URL the page can draw.
///
/// Nothing where the name is empty, the themes have no such icon, or the file
/// is too large to send: the row falls back to a letter, which is what the
/// tray does for an item with no icon at all.
pub fn icon_url(name: &str, theme: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let path = crate::icon::lookup(name, None, theme, ICON_SIZE)?;
    let url = crate::icon::data_url(&path)?;
    (url.len() <= MAX_ICON_URL).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop() -> &'static [&'static str] {
        &["viewport", "wlroots"]
    }

    #[test]
    fn a_plain_entry_is_an_application() {
        let app = entry(
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=firefox\nIcon=firefox\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.name, "Firefox");
        assert_eq!(app.exec, "firefox");
        assert_eq!(app.icon, "firefox");
        assert_eq!(app.app_id, "firefox");
        assert!(!app.terminal);
    }

    #[test]
    fn field_codes_are_dropped_with_their_arguments() {
        let app = entry(
            "[Desktop Entry]\nName=Firefox\nExec=firefox --new-window %u\n",
            desktop(),
        )
        .expect("parses");
        // The code and the (absent) URL it would have named are both gone.
        assert_eq!(app.exec, "firefox --new-window");
    }

    #[test]
    fn a_field_code_with_an_argument_drops_the_argument() {
        let app = entry(
            "[Desktop Entry]\nName=Link\nExec=x-www-browser %U https://example.com\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.exec, "x-www-browser");
    }

    #[test]
    fn the_name_codes_stand_in_for_the_name() {
        let app = entry(
            "[Desktop Entry]\nName=Firefox\nExec=termite -t %n\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.exec, "termite -t Firefox");
    }

    #[test]
    fn quotes_and_escapes_are_the_specifications_own() {
        let app = entry(
            "[Desktop Entry]\nName=Thing\nExec=run \"a \\\"b\\\" c\"\n",
            desktop(),
        )
        .expect("parses");
        // What the specification's quoting meant, re-quoted for the shell.
        assert_eq!(app.exec, "run 'a \"b\" c'");
    }

    #[test]
    fn an_argument_with_a_space_is_quoted_for_the_shell() {
        let app = entry(
            "[Desktop Entry]\nName=Thing\nExec=run --title \"two words\"\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.exec, "run --title 'two words'");
    }

    #[test]
    fn a_terminal_entry_says_so() {
        let app = entry(
            "[Desktop Entry]\nName=htop\nExec=htop\nTerminal=true\n",
            desktop(),
        )
        .expect("parses");
        assert!(app.terminal);
    }

    #[test]
    fn hidden_and_no_display_are_not_shown() {
        for key in ["Hidden=true", "NoDisplay=true"] {
            assert!(
                entry(
                    &format!("[Desktop Entry]\nName=X\nExec=x\n{key}\n"),
                    desktop()
                )
                .is_none(),
                "{key} is shown"
            );
        }
    }

    #[test]
    fn not_show_in_hides_on_the_named_desktop() {
        let text = "[Desktop Entry]\nName=X\nExec=x\nNotShowIn=GNOME;viewport\n";
        assert!(entry(text, desktop()).is_none());
        // A desktop that is not named shows.
        let text = "[Desktop Entry]\nName=X\nExec=x\nNotShowIn=GNOME\n";
        assert!(entry(text, desktop()).is_some());
    }

    #[test]
    fn only_show_in_shows_only_on_the_named_desktops() {
        let text = "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=KDE\n";
        assert!(entry(text, desktop()).is_none());
        let text = "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=KDE;Viewport\n";
        assert!(entry(text, desktop()).is_some());
        // No list at all is every desktop.
        let text = "[Desktop Entry]\nName=X\nExec=x\n";
        assert!(entry(text, desktop()).is_some());
    }

    #[test]
    fn a_link_is_not_an_application() {
        assert!(entry(
            "[Desktop Entry]\nName=Example\nURL=https://example.com\n",
            desktop()
        )
        .is_none());
        // An explicit Type wins over what the fields imply.
        assert!(entry(
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\nURL=https://example.com\n",
            desktop(),
        )
        .is_some());
    }

    #[test]
    fn an_entry_without_a_name_or_a_command_is_nothing() {
        assert!(entry("[Desktop Entry]\nExec=x\n", desktop()).is_none());
        assert!(entry("[Desktop Entry]\nName=X\n", desktop()).is_none());
    }

    #[test]
    fn try_exec_checks_the_binary() {
        // /bin/sh exists on every machine this runs on.
        let shown = entry(
            "[Desktop Entry]\nName=X\nExec=x\nTryExec=/bin/sh\n",
            desktop(),
        );
        assert!(shown.is_some());
        let hidden = entry(
            "[Desktop Entry]\nName=X\nExec=x\nTryExec=/no/such/binary-viewport-test\n",
            desktop(),
        );
        assert!(hidden.is_none());
    }

    #[test]
    fn only_the_desktop_entry_section_is_read() {
        let app = entry(
            "[Desktop Action copy]\nName=Copy\n[Desktop Entry]\nName=X\nExec=x\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.name, "X");
    }

    #[test]
    fn locale_variants_are_the_base_keys() {
        let app = entry(
            "[Desktop Entry]\nName=Firefox\nName[de]=Firefox\nExec=firefox\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.name, "Firefox");
    }

    #[test]
    fn startup_wm_class_is_the_app_id() {
        let app = entry(
            "[Desktop Entry]\nName=Firefox\nExec=/usr/lib/firefox/firefox\nStartupWMClass=Navigator\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.app_id, "Navigator");
    }

    #[test]
    fn the_detail_is_keywords_or_the_main_category() {
        let app = entry(
            "[Desktop Entry]\nName=X\nExec=x\nKeywords=web;browser;www\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.detail, "web, browser, www");

        let app = entry(
            "[Desktop Entry]\nName=X\nExec=x\nCategories=Network;X-WebBrowser;\n",
            desktop(),
        )
        .expect("parses");
        assert_eq!(app.detail, "Network");
    }

    #[test]
    fn a_scan_reads_the_first_directory_that_has_the_file() {
        let dir = test_dir("scan");
        let user = dir.join("user");
        let system = dir.join("system");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(
            user.join("thing.desktop"),
            "[Desktop Entry]\nName=User Thing\nExec=user-thing\n",
        )
        .unwrap();
        std::fs::write(
            system.join("thing.desktop"),
            "[Desktop Entry]\nName=System Thing\nExec=system-thing\n",
        )
        .unwrap();
        std::fs::write(
            system.join("other.desktop"),
            "[Desktop Entry]\nName=Other\nExec=other\n",
        )
        .unwrap();
        // A file that is not a .desktop is not an entry.
        std::fs::write(user.join("readme.txt"), "not an entry").unwrap();

        let apps = scan(&[user, system], desktop());
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "Other");
        assert_eq!(apps[1].name, "User Thing");
        assert_eq!(apps[1].exec, "user-thing");
    }

    #[test]
    fn a_scan_sorts_by_name() {
        let dir = test_dir("sort");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("b.desktop"),
            "[Desktop Entry]\nName=Bravo\nExec=b\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("a.desktop"),
            "[Desktop Entry]\nName=alpha\nExec=a\n",
        )
        .unwrap();
        let apps = scan(&[dir], desktop());
        assert_eq!(
            apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "Bravo"]
        );
    }

    /// A scratch directory that cannot collide with another test's: the tests
    /// run in threads of one process, and a shared name is a race.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "viewport-launcher-test-{}-{}-{name}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(' ', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
