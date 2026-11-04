//! Turns untrusted Drive file names into safe local path components.
//!
//! Drive accepts almost any string as a file name: `/`, `..`, control characters, names that
//! Windows reserves for devices, and names longer than local filesystems allow. Every Drive name
//! that becomes part of a local path goes through [`local_name`] and is joined with
//! [`safe_join`], so the result is always a single component directly under its parent. The
//! children of one Drive folder go through [`unique_local_names`], which also keeps two Drive
//! files with the same name from overwriting each other.

use std::{
    borrow::Cow,
    collections::{hash_map, HashMap, HashSet},
    error,
    fmt::{self, Display, Formatter},
    path::{Path, PathBuf},
};

/// Longest path component, in bytes, that common local filesystems accept.
pub(crate) const MAX_COMPONENT_BYTES: usize = 255;

/// An extension longer than this (dot included) is not preserved when a name is shortened.
const MAX_EXTENSION_BYTES: usize = 32;

/// Number of hex digits of the file id hash used in name suffixes.
const ID_TAG_HEX_DIGITS: usize = 8;

const REPLACEMENT: char = '_';

/// Why a Drive name cannot be turned into a local name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidName {
    Empty,
    CurrentDir,
    ParentDir,
}

impl Display for InvalidName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InvalidName::Empty => "a file name cannot be empty",
            InvalidName::CurrentDir => "'.' refers to the current directory",
            InvalidName::ParentDir => "'..' refers to the parent directory",
        })
    }
}

impl error::Error for InvalidName {}

/// A Drive file whose name cannot be used locally, with the hint to rename it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnusableName {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) source: InvalidName,
}

impl UnusableName {
    #[must_use]
    pub(crate) fn new(name: &str, id: &str, source: InvalidName) -> Self {
        Self {
            name: name.to_owned(),
            id: id.to_owned(),
            source,
        }
    }
}

impl Display for UnusableName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the name '{}' of file {} cannot be used as a local file name, rename it with \
            `gdrive files rename {} <NEW_NAME>`",
            printable(&self.name),
            self.id,
            self.id,
        )
    }
}

impl error::Error for UnusableName {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.source)
    }
}

/// A Drive file that is saved under a different local name, reported as a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NameChange {
    /// The Drive name is not a valid local name and was changed by [`local_name`].
    Sanitized {
        id: String,
        drive_name: String,
        local_name: String,
    },
    /// Another file in the same folder (`kept_id`) keeps the local name; this one gets a suffix.
    Duplicate {
        id: String,
        kept_id: String,
        drive_name: String,
        local_name: String,
    },
}

impl Display for NameChange {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            NameChange::Sanitized {
                id,
                drive_name,
                local_name,
            } => write!(
                f,
                "'{}' (id {id}) is saved as '{local_name}' because the name is not a valid local \
                file name",
                printable(drive_name),
            ),
            NameChange::Duplicate {
                id,
                kept_id,
                drive_name,
                local_name,
            } => write!(
                f,
                "'{}' (id {id}) is saved as '{local_name}' because id {kept_id} has the same local \
                name in this folder",
                printable(drive_name),
            ),
        }
    }
}

/// One child of a Drive folder, input of [`unique_local_names`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct NameEntry<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
}

/// Result of [`unique_local_names`]: one name per entry, in input order, and the changes to warn
/// about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UniqueNames {
    pub(crate) names: Vec<String>,
    pub(crate) changes: Vec<NameChange>,
}

/// Platform behavior that changes which names are safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rules {
    /// Replace the characters that Windows forbids in file names: `< > : " | ? *`.
    windows_chars: bool,
    /// Names that differ only in letter case refer to the same file (Windows and macOS defaults).
    case_insensitive: bool,
}

impl Rules {
    const HOST: Self = Self {
        windows_chars: cfg!(windows),
        case_insensitive: cfg!(any(windows, target_os = "macos")),
    };
}

/// Maps the Drive name of the file `file_id` to a safe single local path component.
///
/// The rules, applied in this order:
/// 1. An empty name, `.` and `..` are rejected.
/// 2. `/`, `\`, NUL and every other control character (Unicode category Cc) become `_`. On
///    Windows, `< > : " | ? *` become `_` too.
/// 3. A name whose part before the first `.` (ignoring trailing spaces) is a Windows device name
///    (`CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`, `LPT0`-`LPT9`, `COM¹²³`, `LPT¹²³`, in any case)
///    gets `_` appended to that part: `con.txt` becomes `con_.txt`. This applies on every
///    platform so that a download is portable.
/// 4. A name ending in `.` or a space gets `_` appended.
/// 5. A name longer than 255 bytes is cut at a UTF-8 character boundary and gets `~` plus the
///    first 8 hex digits of the MD5 of the file id before its extension, so that two long names
///    sharing a prefix stay distinct. The extension (from the last `.`, at most 32 bytes) is kept.
pub(crate) fn local_name(drive_name: &str, file_id: &str) -> Result<String, InvalidName> {
    sanitize(drive_name, file_id, Rules::HOST)
}

/// Maps the children of one Drive folder to local names that are safe and unique in the folder.
///
/// Every name goes through [`local_name`]. Entries whose local names collide (on Windows and
/// macOS also when they differ only in letter case) are ordered by file id: the first keeps the
/// name, every other one gets `~` plus 8 hex digits of the MD5 of its file id before the
/// extension (`report~1a2b3c4d.pdf`), and `-2`, `-3`, ... after that in the unlikely case that
/// this name is taken too. The result does not depend on the order of `entries`.
pub(crate) fn unique_local_names(entries: &[NameEntry<'_>]) -> Result<UniqueNames, UnusableName> {
    unique_local_names_with(entries, Rules::HOST)
}

fn unique_local_names_with(
    entries: &[NameEntry<'_>],
    rules: Rules,
) -> Result<UniqueNames, UnusableName> {
    let mut names = entries
        .iter()
        .map(|entry| {
            sanitize(entry.name, entry.id, rules)
                .map_err(|source| UnusableName::new(entry.name, entry.id, source))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut by_id = (0..entries.len()).collect::<Vec<_>>();
    by_id.sort_by(|&a, &b| entries[a].id.cmp(entries[b].id).then(a.cmp(&b)));

    // The first entry (by id) with a given local name keeps it.
    let mut owners = HashMap::new();
    let mut duplicates = Vec::new();
    for &index in &by_id {
        match owners.entry(collision_key(&names[index], rules)) {
            hash_map::Entry::Vacant(vacant) => {
                vacant.insert(index);
            }
            hash_map::Entry::Occupied(occupied) => duplicates.push((index, *occupied.get())),
        }
    }

    let mut taken = owners.into_keys().collect::<HashSet<_>>();
    let mut kept_by = vec![None; entries.len()];
    for (index, kept) in duplicates {
        let id = entries[index].id;
        // Every attempt yields a different candidate and `taken` is finite, so this ends.
        let mut attempt = 0;
        let unique = loop {
            let candidate = with_tag(&names[index], &duplicate_tag(id, attempt));
            if taken.insert(collision_key(&candidate, rules)) {
                break candidate;
            }
            attempt += 1;
        };
        names[index] = unique;
        kept_by[index] = Some(kept);
    }

    let changes = entries
        .iter()
        .zip(&names)
        .zip(&kept_by)
        .filter(|((entry, name), _)| entry.name != name.as_str())
        .map(|((entry, name), kept)| match kept {
            Some(kept) => NameChange::Duplicate {
                id: entry.id.to_owned(),
                kept_id: entries[*kept].id.to_owned(),
                drive_name: entry.name.to_owned(),
                local_name: name.clone(),
            },
            None => NameChange::Sanitized {
                id: entry.id.to_owned(),
                drive_name: entry.name.to_owned(),
                local_name: name.clone(),
            },
        })
        .collect();

    Ok(UniqueNames { names, changes })
}

/// Joins `component` to `base` so that the result is always directly under `base`.
///
/// `component` should come from [`local_name`]. As a second line of defense, separators and
/// control characters are replaced with `_` and an empty name, `.` and `..` get a `_` prefix, so
/// no input can produce an absolute path, a drive prefix or a traversal.
#[must_use]
pub(crate) fn safe_join(base: &Path, component: &str) -> PathBuf {
    safe_join_with(base, component, Rules::HOST)
}

fn safe_join_with(base: &Path, component: &str, rules: Rules) -> PathBuf {
    let component = replace_chars(component, rules);
    let component = match component.as_ref() {
        "" | "." | ".." => Cow::Owned(format!("{REPLACEMENT}{component}")),
        _ => component,
    };

    base.join(component.as_ref())
}

fn sanitize(name: &str, file_id: &str, rules: Rules) -> Result<String, InvalidName> {
    match name {
        "" => return Err(InvalidName::Empty),
        "." => return Err(InvalidName::CurrentDir),
        ".." => return Err(InvalidName::ParentDir),
        _ => {}
    }

    let mut name = replace_chars(name, rules).into_owned();

    if let Some(stem_end) = reserved_device_stem_end(&name) {
        name.insert(stem_end, REPLACEMENT);
    }

    if name.ends_with(['.', ' ']) {
        name.push(REPLACEMENT);
    }

    if name.len() > MAX_COMPONENT_BYTES {
        name = with_tag(&name, &id_tag(file_id));
    }

    Ok(name)
}

fn is_replaced(c: char, rules: Rules) -> bool {
    c == '/'
        || c == '\\'
        || c.is_control()
        || (rules.windows_chars && matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
}

fn replace_chars(name: &str, rules: Rules) -> Cow<'_, str> {
    if name.chars().any(|c| is_replaced(c, rules)) {
        let replaced = name
            .chars()
            .map(|c| {
                if is_replaced(c, rules) {
                    REPLACEMENT
                } else {
                    c
                }
            })
            .collect();
        Cow::Owned(replaced)
    } else {
        Cow::Borrowed(name)
    }
}

/// If the part of `name` before the first dot is a Windows device name, returns where it ends.
fn reserved_device_stem_end(name: &str) -> Option<usize> {
    let stem_end = name.find('.').unwrap_or(name.len());
    let stem = name[..stem_end].trim_end_matches(' ');
    is_reserved_device_name(stem).then_some(stem_end)
}

fn is_reserved_device_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }

    ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(prefix).is_some_and(|number| {
            matches!(number.as_bytes(), [b'0'..=b'9']) || matches!(number, "¹" | "²" | "³")
        })
    })
}

/// `~` followed by the first hex digits of the MD5 of the file id.
fn id_tag(file_id: &str) -> String {
    let hash = format!("{:x}", md5::compute(file_id.as_bytes()));
    format!("~{}", &hash[..ID_TAG_HEX_DIGITS])
}

/// [`id_tag`], then `-2`, `-3`, ... for the following attempts.
fn duplicate_tag(file_id: &str, attempt: u32) -> String {
    let tag = id_tag(file_id);
    if attempt == 0 {
        tag
    } else {
        format!("{tag}-{}", attempt + 1)
    }
}

fn collision_key(name: &str, rules: Rules) -> String {
    if rules.case_insensitive {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

/// `name` with control characters escaped, safe to print to a terminal.
fn printable(name: &str) -> Cow<'_, str> {
    if name.chars().any(char::is_control) {
        Cow::Owned(
            name.chars()
                .map(|c| {
                    if c.is_control() {
                        c.escape_default().to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(name)
    }
}

/// Inserts `tag` before the extension of `name`, shortening the stem so that the result fits in
/// [`MAX_COMPONENT_BYTES`].
fn with_tag(name: &str, tag: &str) -> String {
    let (stem, extension) = split_extension(name);
    let budget = MAX_COMPONENT_BYTES.saturating_sub(tag.len() + extension.len());
    let stem = truncate_at_char_boundary(stem, budget);
    format!("{stem}{tag}{extension}")
}

/// Splits `name` into stem and extension (from the last dot). A leading dot does not start an
/// extension, and an extension longer than [`MAX_EXTENSION_BYTES`] is treated as part of the stem.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= MAX_EXTENSION_BYTES => name.split_at(dot),
        _ => (name, ""),
    }
}

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }

    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use std::path::{Component, Path};

    use super::{
        id_tag, local_name, safe_join, safe_join_with, sanitize, unique_local_names_with,
        InvalidName, NameChange, NameEntry, Rules, UniqueNames, UnusableName, MAX_COMPONENT_BYTES,
    };

    const UNIX: Rules = Rules {
        windows_chars: false,
        case_insensitive: false,
    };
    const WINDOWS: Rules = Rules {
        windows_chars: true,
        case_insensitive: true,
    };

    fn unix(name: &str) -> String {
        sanitize(name, "id", UNIX).unwrap()
    }

    #[test]
    fn plain_names_are_unchanged() {
        for name in [
            "report.pdf",
            "a b c",
            ".bashrc",
            "archive.tar.gz",
            "ünïcødé ✓.txt",
            "日本語のファイル",
            "CONSOLE",
            "COM10",
            "LPT",
            "xNUL",
            "con_",
            "~",
            "a.b.c",
        ] {
            assert_eq!(unix(name), name);
            assert_eq!(sanitize(name, "id", WINDOWS).unwrap(), name);
        }
    }

    #[test]
    fn rejects_empty_and_dot_names() {
        assert_eq!(local_name("", "id"), Err(InvalidName::Empty));
        assert_eq!(local_name(".", "id"), Err(InvalidName::CurrentDir));
        assert_eq!(local_name("..", "id"), Err(InvalidName::ParentDir));
    }

    #[test]
    fn replaces_separators() {
        assert_eq!(unix("a/b"), "a_b");
        assert_eq!(unix("a\\b"), "a_b");
        assert_eq!(unix("../x"), ".._x");
        assert_eq!(unix("..\\..\\x"), ".._.._x");
        assert_eq!(unix("/etc/passwd"), "_etc_passwd");
        assert_eq!(unix("/"), "_");
        assert_eq!(unix("C:\\Windows"), "C:_Windows");
    }

    #[test]
    fn replaces_control_characters() {
        assert_eq!(unix("a\0b"), "a_b");
        assert_eq!(unix("line\nbreak\r"), "line_break_");
        assert_eq!(unix("tab\there"), "tab_here");
        assert_eq!(unix("\u{1b}[31mred"), "_[31mred");
        assert_eq!(unix("del\u{7f}"), "del_");
        assert_eq!(unix("c1\u{85}"), "c1_");
    }

    #[test]
    fn windows_characters_depend_on_platform() {
        let name = "a<b>c:d\"e|f?g*h";
        assert_eq!(unix(name), name);
        assert_eq!(sanitize(name, "id", WINDOWS).unwrap(), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize("C:\\x", "id", WINDOWS).unwrap(), "C__x");
    }

    #[test]
    fn reserved_device_names_get_underscore() {
        assert_eq!(unix("CON"), "CON_");
        assert_eq!(unix("con"), "con_");
        assert_eq!(unix("Prn"), "Prn_");
        assert_eq!(unix("AUX"), "AUX_");
        assert_eq!(unix("nul"), "nul_");
        assert_eq!(unix("COM1"), "COM1_");
        assert_eq!(unix("com9"), "com9_");
        assert_eq!(unix("COM0"), "COM0_");
        assert_eq!(unix("LPT1"), "LPT1_");
        assert_eq!(unix("lpt9"), "lpt9_");
        assert_eq!(unix("COM¹"), "COM¹_");
        assert_eq!(unix("LPT³"), "LPT³_");
        assert_eq!(unix("con.txt"), "con_.txt");
        assert_eq!(unix("NUL.tar.gz"), "NUL_.tar.gz");
        assert_eq!(unix("AUX .txt"), "AUX _.txt");
        assert_eq!(unix("AUX "), "AUX _");
        assert_eq!(unix("CON."), "CON_._");
        assert_eq!(sanitize("CON", "id", WINDOWS).unwrap(), "CON_");
    }

    #[test]
    fn trailing_dots_and_spaces_get_underscore() {
        assert_eq!(unix("name."), "name._");
        assert_eq!(unix("name "), "name _");
        assert_eq!(unix("..."), "..._");
        assert_eq!(unix(" "), " _");
        assert_eq!(unix(". "), ". _");
    }

    #[test]
    fn id_tag_is_stable() {
        // md5("abc") = 900150983cd24fb0d6963f7d28e17f72
        assert_eq!(id_tag("abc"), "~90015098");
    }

    #[test]
    fn long_names_are_truncated_with_id_tag() {
        let name = "a".repeat(300);
        let local = sanitize(&name, "abc", UNIX).unwrap();
        assert_eq!(local.len(), MAX_COMPONENT_BYTES);
        assert_eq!(local, format!("{}~90015098", "a".repeat(246)));
    }

    #[test]
    fn truncation_keeps_extension() {
        let name = format!("{}.pdf", "a".repeat(300));
        let local = sanitize(&name, "abc", UNIX).unwrap();
        assert_eq!(local.len(), MAX_COMPONENT_BYTES);
        assert!(local.ends_with("~90015098.pdf"), "{local}");
        assert!(local.starts_with("aaaa"));
    }

    #[test]
    fn truncation_drops_overlong_extension() {
        let name = format!("{}.{}", "a".repeat(300), "b".repeat(40));
        let local = sanitize(&name, "abc", UNIX).unwrap();
        assert_eq!(local.len(), MAX_COMPONENT_BYTES);
        assert!(local.ends_with("~90015098"), "{local}");
    }

    #[test]
    fn length_limit_is_exact() {
        let exact = "b".repeat(MAX_COMPONENT_BYTES);
        assert_eq!(unix(&exact), exact);

        let over = "b".repeat(MAX_COMPONENT_BYTES + 1);
        let local = unix(&over);
        assert_eq!(local.len(), MAX_COMPONENT_BYTES);
        assert_ne!(local, over[..MAX_COMPONENT_BYTES]);
    }

    #[test]
    fn truncation_respects_multibyte_boundaries() {
        for unit in ["é", "日", "😀", "e\u{301}"] {
            let name = format!("{}.txt", unit.repeat(200));
            let local = sanitize(&name, "abc", UNIX).unwrap();
            assert!(
                local.len() <= MAX_COMPONENT_BYTES,
                "{unit}: {}",
                local.len()
            );
            assert!(local.ends_with("~90015098.txt"), "{local}");

            let stem = local.strip_suffix("~90015098.txt").unwrap();
            assert!(name.starts_with(stem));
            // The stem is cut at a character boundary, so it is made of whole characters.
            assert!(stem.chars().all(|c| unit.contains(c)));
        }
    }

    #[test]
    fn names_differing_after_truncation_stay_distinct() {
        let prefix = "x".repeat(300);
        let first = sanitize(&format!("{prefix}1.txt"), "id-one", UNIX).unwrap();
        let second = sanitize(&format!("{prefix}2.txt"), "id-two", UNIX).unwrap();

        assert_ne!(first, second);
        assert_eq!(first.len(), MAX_COMPONENT_BYTES);
        assert_eq!(second.len(), MAX_COMPONENT_BYTES);
        assert_eq!(&first[first.len() - 4..], ".txt");
        assert_eq!(&second[second.len() - 4..], ".txt");

        // The same file always maps to the same name.
        assert_eq!(
            first,
            sanitize(&format!("{prefix}1.txt"), "id-one", UNIX).unwrap()
        );
    }

    #[test]
    fn sanitize_is_idempotent() {
        let long = format!("{}.pdf", "z".repeat(400));
        for name in [
            "a/b",
            "con.txt",
            "name.",
            "AUX ",
            "\u{1b}x",
            long.as_str(),
            "plain",
        ] {
            let once = sanitize(name, "id", UNIX).unwrap();
            let twice = sanitize(&once, "id", UNIX).unwrap();
            assert_eq!(once, twice, "{name}");
        }
    }

    #[test]
    fn safe_join_keeps_result_directly_under_base() {
        let base = Path::new("/srv/downloads");
        // A Windows host always uses the Windows rules; `C:` would be a drive prefix there.
        let rule_sets: &[Rules] = if cfg!(windows) {
            &[WINDOWS]
        } else {
            &[UNIX, WINDOWS]
        };
        for &rules in rule_sets {
            for component in [
                "file.txt",
                "../x",
                "..",
                ".",
                "",
                "/etc/passwd",
                "a/../../b",
                "C:\\Windows\\x",
                "C:",
                "\\\\server\\share",
                "a\0b",
                "dir/",
            ] {
                let joined = safe_join_with(base, component, rules);
                assert_eq!(joined.parent(), Some(base), "{component:?}");
                let last = joined.components().next_back();
                assert!(
                    matches!(last, Some(Component::Normal(_))),
                    "{component:?} -> {joined:?}"
                );
                assert_eq!(
                    joined.components().count(),
                    base.components().count() + 1,
                    "{component:?} -> {joined:?}"
                );
            }
        }
    }

    #[test]
    fn safe_join_values() {
        let base = Path::new("base");
        assert_eq!(safe_join(base, "a.txt"), base.join("a.txt"));
        assert_eq!(safe_join(base, "../x"), base.join(".._x"));
        assert_eq!(safe_join(base, ".."), base.join("_.."));
        assert_eq!(safe_join(base, "."), base.join("_."));
        assert_eq!(safe_join(base, ""), base.join("_"));
        assert_eq!(safe_join(base, "/abs"), base.join("_abs"));
    }

    #[test]
    fn local_names_join_safely() {
        let base = Path::new("/dst");
        for name in [
            "../../etc/cron.d/x",
            "/root/.ssh/authorized_keys",
            "a\\..\\b",
        ] {
            let local = local_name(name, "id").unwrap();
            assert_eq!(safe_join(base, &local).parent(), Some(base));
            assert_eq!(safe_join(base, &local), base.join(&local));
        }
    }

    fn entries<'a>(pairs: &[(&'a str, &'a str)]) -> Vec<NameEntry<'a>> {
        pairs
            .iter()
            .map(|&(id, name)| NameEntry { id, name })
            .collect()
    }

    fn unique(pairs: &[(&str, &str)], rules: Rules) -> UniqueNames {
        unique_local_names_with(&entries(pairs), rules).unwrap()
    }

    #[test]
    fn unique_names_without_collisions_are_unchanged() {
        let result = unique(&[("1", "a.pdf"), ("2", "b.pdf"), ("3", "dir")], UNIX);
        assert_eq!(result.names, ["a.pdf", "b.pdf", "dir"]);
        assert_eq!(result.changes, []);
    }

    #[test]
    fn duplicates_keep_first_by_id_and_suffix_the_rest() {
        // md5("abc") = 900150983cd24fb0..., md5("abd") = 4911e516e5aa21d3...
        let result = unique(&[("abd", "a.pdf"), ("abc", "a.pdf")], UNIX);
        assert_eq!(result.names, ["a~4911e516.pdf", "a.pdf"]);
        assert_eq!(
            result.changes,
            [NameChange::Duplicate {
                id: "abd".to_owned(),
                kept_id: "abc".to_owned(),
                drive_name: "a.pdf".to_owned(),
                local_name: "a~4911e516.pdf".to_owned(),
            }]
        );
        let warning = result.changes[0].to_string();
        assert!(
            warning.contains("abd") && warning.contains("abc"),
            "{warning}"
        );
    }

    #[test]
    fn duplicate_mapping_does_not_depend_on_listing_order() {
        let forward = [("id1", "x"), ("id2", "x"), ("id3", "x"), ("id4", "y")];
        let mut backward = forward;
        backward.reverse();

        let forward_result = unique(&forward, UNIX);
        let backward_result = unique(&backward, UNIX);

        let mut forward_pairs = forward
            .iter()
            .map(|(id, _)| *id)
            .zip(forward_result.names)
            .collect::<Vec<_>>();
        let mut backward_pairs = backward
            .iter()
            .map(|(id, _)| *id)
            .zip(backward_result.names)
            .collect::<Vec<_>>();
        forward_pairs.sort();
        backward_pairs.sort();
        assert_eq!(forward_pairs, backward_pairs);

        assert_eq!(forward_pairs[0], ("id1", "x".to_owned()));
        assert_eq!(forward_pairs[3], ("id4", "y".to_owned()));
        assert_ne!(forward_pairs[1].1, forward_pairs[2].1);
    }

    #[test]
    fn names_that_collide_after_sanitizing_are_deduplicated() {
        let result = unique(&[("2", "a/b"), ("1", "a_b")], UNIX);
        assert_eq!(result.names[1], "a_b");
        assert_ne!(result.names[0], "a_b");
        assert!(result.names[0].starts_with("a_b~"));
        assert!(matches!(
            result.changes.as_slice(),
            [NameChange::Duplicate { id, kept_id, .. }] if id == "2" && kept_id == "1"
        ));
    }

    #[test]
    fn files_and_folders_share_one_namespace() {
        // A folder `data` and a file `data` cannot both exist in one local directory.
        let result = unique(&[("folder", "data"), ("file", "data")], UNIX);
        assert_eq!(result.names[0], format!("data{}", id_tag("folder")));
        assert_eq!(result.names[1], "data");
    }

    #[test]
    fn case_collisions_depend_on_platform() {
        let pairs = [("1", "Report.pdf"), ("2", "report.pdf")];
        assert_eq!(unique(&pairs, UNIX).names, ["Report.pdf", "report.pdf"]);

        let insensitive = unique(&pairs, WINDOWS);
        assert_eq!(insensitive.names[0], "Report.pdf");
        assert_eq!(insensitive.names[1], format!("report{}.pdf", id_tag("2")));
    }

    #[test]
    fn suffixed_name_that_is_taken_gets_counter() {
        let tagged = format!("a{}.pdf", id_tag("2"));
        let result = unique(&[("1", "a.pdf"), ("2", "a.pdf"), ("0", &tagged)], UNIX);
        assert_eq!(result.names[0], "a.pdf");
        assert_eq!(result.names[2], tagged);
        assert_eq!(result.names[1], format!("a{}-2.pdf", id_tag("2")));
    }

    #[test]
    fn long_duplicates_stay_within_limit() {
        let name = format!("{}.txt", "n".repeat(250));
        let result = unique(&[("1", &name), ("2", &name)], UNIX);
        assert_ne!(result.names[0], result.names[1]);
        for local in &result.names {
            assert!(local.len() <= MAX_COMPONENT_BYTES);
            assert_eq!(&local[local.len() - 4..], ".txt");
        }
    }

    #[test]
    fn sanitized_names_are_reported() {
        let result = unique(&[("1", "a/b"), ("2", "ok")], UNIX);
        assert_eq!(
            result.changes,
            [NameChange::Sanitized {
                id: "1".to_owned(),
                drive_name: "a/b".to_owned(),
                local_name: "a_b".to_owned(),
            }]
        );
    }

    #[test]
    fn unusable_names_fail_the_folder() {
        let err = unique_local_names_with(&entries(&[("1", "ok"), ("2", "..")]), UNIX).unwrap_err();
        assert_eq!(err, UnusableName::new("..", "2", InvalidName::ParentDir));
        let message = err.to_string();
        assert!(message.contains("gdrive files rename 2"), "{message}");
    }

    #[test]
    fn warnings_escape_control_characters() {
        let result = unique(&[("1", "evil\u{1b}[2Jname")], UNIX);
        let warning = result.changes[0].to_string();
        assert!(!warning.contains('\u{1b}'), "{warning}");
        assert!(warning.contains("\\u{1b}"), "{warning}");
    }
}
