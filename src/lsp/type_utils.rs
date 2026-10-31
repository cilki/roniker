//! Shared helpers for working with Rust type names extracted by the analyzer.
use crate::rust_analyzer::FieldInfo;
use std::borrow::Cow;

/// Get the last path segment of a fully-qualified type name.
/// For example: `crate::models::User` -> `User`
pub fn short_name(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// Normalize a type name for comparison by removing all spaces, so that
/// `Vec < Post >` and `Vec<Post>` are treated the same, and by dropping the
/// module path in front of a standard-library name, so that
/// `std::collections::HashMap<String, u32>` is treated the same as
/// `HashMap<String, u32>`. Depending on the source formatting the analyzer
/// extracts types with inconsistent internal spacing, and it records whatever
/// path the source spelled, so this is applied before any prefix/suffix or
/// set-membership checks.
///
/// This is the same normalization [`crate::rust_analyzer::RustAnalyzer::get_type_info`]
/// applies to a lookup key, so the two cannot disagree about what a type name
/// means.
pub fn normalize_type(type_name: &str) -> Cow<'_, str> {
    crate::rust_analyzer::normalize_type_name(type_name)
}

/// Extract the inner type from a generic type when it starts with the given
/// wrapper prefix. For example: `extract_inner_type("Vec<User>", "Vec<")` -> `Some("User")`
pub fn extract_inner_type<'a>(type_str: &'a str, wrapper: &str) -> Option<&'a str> {
    if type_str.starts_with(wrapper) && type_str.ends_with('>') {
        Some(&type_str[wrapper.len()..type_str.len() - 1])
    } else {
        None
    }
}

/// Get the content of the outermost generic (e.g., `Option<Vec<T>>` -> `Vec<T>`),
/// or the type itself if it isn't generic. Whitespace is removed.
pub fn strip_outer_generic(type_name: &str) -> String {
    let clean = normalize_type(type_name);
    match (clean.find('<'), clean.rfind('>')) {
        (Some(start), Some(end)) if start < end => clean[start + 1..end].to_string(),
        _ => clean.into_owned(),
    }
}

/// Check if a type is a primitive type (not a custom enum/struct). A thin
/// wrapper over [`crate::rust_analyzer::is_primitive`] that normalizes the name
/// first, so callers holding a raw type name don't each have to.
pub fn is_primitive_type(type_name: &str) -> bool {
    crate::rust_analyzer::is_primitive(&normalize_type(type_name))
}

/// Standard-library wrappers a value is written straight through: the document
/// spells out the wrapped value and nothing else, so each of these asks of a
/// value exactly what the type it wraps would.
///
/// `Option<T>` is not one of them: `Some(..)` and `None` are spellings of its
/// own. Neither is `NonZero<T>`, which is written as the integer but is not
/// *any* integer — [`crate::rust_analyzer::std_type`] answers for both its
/// spellings, so that the value offered for one is the value offered for the
/// other.
pub const TRANSPARENT_WRAPPERS: [&str; 8] = [
    "Box<", "Rc<", "Arc<", "Cow<", "Cell<", "RefCell<", "Mutex<", "RwLock<",
];

/// Standard-library sequences, which RON writes as an array of their elements
/// and which therefore ask of a value exactly what a `Vec<T>` would.
///
/// The sets (`HashSet`, `BTreeSet`) are written the same way but are checked as
/// a whole rather than element by element, so they are not in this list.
pub const SEQUENCE_WRAPPERS: [&str; 4] = ["Vec<", "VecDeque<", "LinkedList<", "BinaryHeap<"];

/// Check if a type is a user-defined struct or enum — i.e. something the
/// analyzer should resolve and validate against. A thin wrapper over
/// [`crate::rust_analyzer::is_custom_type`], which spells out what that
/// amounts to, that normalizes the name first — so `type_name` may be passed
/// raw or pre-normalized.
pub fn is_custom_type(type_name: &str) -> bool {
    crate::rust_analyzer::is_custom_type(&normalize_type(type_name))
}

/// The standard-library type `type_name` names, if it is one of those that take
/// no generic arguments. A thin wrapper over
/// [`crate::rust_analyzer::std_type`] that normalizes the name first, so
/// callers holding a raw type name don't each have to.
pub fn std_type(type_name: &str) -> Option<crate::rust_analyzer::StdType> {
    crate::rust_analyzer::std_type(&normalize_type(type_name))
}

/// Largest number of positions [`tuple_element_types`] will enumerate. An
/// array longer than this is not a value anybody writes out by hand, and
/// enumerating `[u8; 1048576]` position by position would cost more than the
/// checking it enables is worth.
const MAX_TUPLE_ARITY: usize = 256;

/// The declared type of each element of the RON tuple `clean` is written as,
/// when `clean` — an already-normalized type name — names something RON spells
/// that way.
///
/// RON writes a Rust tuple and a fixed-size array alike as a parenthesized
/// tuple: a `(String, u16)` field is written `("a", 2)` and a `[u32; 3]` field
/// `(1, 2, 3)`. Brackets belong to the sequence types — `Vec`, slices, sets —
/// whose length the value decides rather than the type, and `ron` refuses them
/// for either of these.
///
/// `None` for anything else, which includes the composites with no fixed list
/// of positions to return: a slice `[T]`, an array whose length is a const
/// generic rather than a literal, and one longer than [`MAX_TUPLE_ARITY`].
pub fn tuple_element_types(clean: &str) -> Option<Vec<&str>> {
    if let Some(inner) = clean.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        // `()` is the unit type: a tuple of no elements at all.
        if inner.is_empty() {
            return Some(Vec::new());
        }
        // A one-element tuple keeps the trailing comma Rust needs to tell
        // `(T,)` from a parenthesized `T`, which leaves an empty last part.
        return Some(
            split_top_level(inner)
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect(),
        );
    }

    let inner = clean.strip_prefix('[').and_then(|r| r.strip_suffix(']'))?;
    let (element, length) = inner.rsplit_once(';')?;
    match length.parse::<usize>() {
        Ok(length) if length <= MAX_TUPLE_ARITY => Some(vec![element; length]),
        _ => None,
    }
}

/// Standard-library maps, which RON writes as `{key: value, ...}`.
pub const MAP_WRAPPERS: [&str; 2] = ["HashMap<", "BTreeMap<"];

/// The declared key and value types of a map, when `clean` — an
/// already-normalized type name — is one of the [`MAP_WRAPPERS`]: `("String",
/// "Listener")` for `HashMap<String,Listener>`.
///
/// `None` for anything else, including a map whose arguments don't split into
/// exactly a key and a value (a `HashMap<K, V, S>` with a custom hasher keeps
/// its first two).
pub fn map_entry_types(clean: &str) -> Option<(&str, &str)> {
    let inner = MAP_WRAPPERS
        .iter()
        .find_map(|wrapper| extract_inner_type(clean, wrapper))?;
    match split_top_level(inner).as_slice() {
        [key, value, ..] if !key.is_empty() && !value.is_empty() => Some((key, value)),
        _ => None,
    }
}

/// Split a comma-separated list of type names, leaving alone the commas nested
/// inside a generic argument list, a tuple or an array — `(Vec<(u8, u8)>, u16)`
/// has two elements, not four.
fn split_top_level(list: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;

    for (offset, character) in list.char_indices() {
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&list[start..offset]);
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&list[start..]);

    parts
}

/// Largest absolute edit distance we ever treat as a plausible typo. Real
/// misspellings are almost always one or two edits away; beyond that the
/// "match" is a different word, so suggesting it is just noise.
const MAX_EDIT_DISTANCE: usize = 2;

/// Optimal String Alignment distance between two strings, compared
/// case-insensitively. This is Levenshtein extended so that an adjacent
/// transposition (a very common typo, e.g. `prot` for `port`) costs one edit
/// instead of two. Used to power "did you mean?" suggestions.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: String = a.chars().flat_map(char::to_lowercase).collect();
    let b: String = b.chars().flat_map(char::to_lowercase).collect();
    strsim::osa_distance(&a, &b)
}

/// Find the candidate most similar to `target` for a "did you mean?" hint.
///
/// Returns the closest candidate whose edit distance is small enough to be a
/// plausible typo. To keep suggestions from becoming noise, the match must be
/// genuinely close: at most [`MAX_EDIT_DISTANCE`] edits, no more than a third
/// of the longer name's length, and strictly less than `target`'s length so an
/// entirely different word is never suggested. The bounds mean very short names
/// (≤2 chars) only match exactly. Returns `None` when nothing is close enough
/// or `candidates` is empty; ties break to the first occurrence.
pub fn closest_name<'a>(
    target: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let target_len = target.chars().count();
    let mut best: Option<(&str, usize)> = None;

    for candidate in candidates {
        let dist = edit_distance(target, candidate);
        let longer = target_len.max(candidate.chars().count());
        let threshold = (longer / 3).min(MAX_EDIT_DISTANCE);

        if dist <= threshold
            && dist < target_len
            && best.is_none_or(|(_, best_dist)| dist < best_dist)
        {
            best = Some((candidate, dist));
        }
    }

    best.map(|(name, _)| name)
}

/// Fields that must be present in the RON: not `Option<T>`, no default.
/// Takes `(serialized_name, field)` pairs (see `TypeInfo::effective_fields` /
/// `EnumVariant::effective_fields`); a field counts as present under any name
/// serde would accept for it — its serialized name, its Rust name, or any
/// `#[serde(alias = "...")]`. Missing the aliases here would report a field as
/// absent even though the unknown-field check just accepted the alias that
/// supplies it.
pub fn missing_required_fields(
    fields: &[(String, FieldInfo)],
    is_present: impl Fn(&str) -> bool,
) -> Vec<(String, FieldInfo)> {
    fields
        .iter()
        .filter(|(name, f)| {
            !is_present(name)
                && !is_present(&f.name)
                && !f.aliases.iter().any(|a| is_present(a))
                && !f.is_optional()
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_name() {
        assert_eq!(short_name("crate::models::User"), "User");
        assert_eq!(short_name("User"), "User");
    }

    #[test]
    fn test_extract_inner_type() {
        assert_eq!(extract_inner_type("Vec<User>", "Vec<"), Some("User"));
        assert_eq!(
            extract_inner_type("Option<Vec<i32>>", "Option<"),
            Some("Vec<i32>")
        );
        assert_eq!(extract_inner_type("Vec<User>", "Option<"), None);
        assert_eq!(extract_inner_type("User", "Vec<"), None);
    }

    #[test]
    fn test_strip_outer_generic() {
        assert_eq!(strip_outer_generic("Option<Post>"), "Post");
        assert_eq!(strip_outer_generic("Vec < Post >"), "Post");
        assert_eq!(strip_outer_generic("Post"), "Post");
        // Only the outermost layer is stripped.
        assert_eq!(strip_outer_generic("Option<Vec<Post>>"), "Vec<Post>");
    }

    /// A crate that doesn't import a standard-library type names it by its
    /// module path, and the analyzer records that spelling verbatim. Every
    /// type check in the LSP matches the bare name, so the path has to come
    /// off first or the type falls through as a user-defined one.
    #[test]
    fn test_normalize_type_unqualifies_std_names() {
        assert_eq!(
            normalize_type("std::collections::HashMap<String, u32>"),
            "HashMap<String,u32>"
        );
        assert_eq!(normalize_type("std::option::Option<u64>"), "Option<u64>");
        assert_eq!(normalize_type("std::vec::Vec<String>"), "Vec<String>");
        assert_eq!(normalize_type("std::string::String"), "String");
        assert_eq!(normalize_type("core::option::Option<u8>"), "Option<u8>");
        assert_eq!(normalize_type("alloc::boxed::Box<Server>"), "Box<Server>");
        // `use std::collections;` leaves only the one segment in front.
        assert_eq!(normalize_type("collections::BTreeSet<u8>"), "BTreeSet<u8>");

        // Only the outermost name's path is dropped; a user-defined generic
        // argument keeps the path that resolves it.
        assert_eq!(
            normalize_type("std::vec::Vec<crate::config::Server>"),
            "Vec<crate::config::Server>"
        );
    }

    /// A user-defined type keeps its path: that is what distinguishes two
    /// types registered under the same final segment from different modules.
    #[test]
    fn test_normalize_type_keeps_other_paths() {
        assert_eq!(
            normalize_type("crate::config::Server"),
            "crate::config::Server"
        );
        assert_eq!(normalize_type("config::Server"), "config::Server");
        // A composite type has no single name in front to unqualify, and must
        // not be cut at its last `::` either.
        assert_eq!(
            normalize_type("(u8, std::collections::HashMap<String, u32>)"),
            "(u8,std::collections::HashMap<String,u32>)"
        );
    }

    /// A standard-library type with no generic arguments is not something the
    /// analyzer could have registered, so reading it as user-defined meant a
    /// field declared as one drew `unknown-type` on whatever value was written
    /// for it, however correct that value was.
    #[test]
    fn test_is_custom_type_excludes_std_types() {
        for name in [
            "PathBuf",
            "std::path::PathBuf",
            "SocketAddr",
            "Ipv4Addr",
            "NonZeroU32",
            "Duration",
            // The generics that were missing from the wrapper list.
            "VecDeque<String>",
            "LinkedList<u8>",
            "BinaryHeap<u8>",
            "Cow<'static, str>",
            "Mutex<Config>",
            "NonZero<u32>",
            // ... and through a wrapper, or embedded in a composite.
            "Option<PathBuf>",
            "Vec<SocketAddr>",
            "(PathBuf, u16)",
            "&'a str",
        ] {
            assert!(!is_custom_type(name), "{name} is not a user-defined type");
        }

        // A user-defined type whose name merely resembles one stays custom.
        for name in ["Server", "Durations", "NonZeroThing", "MyPathBuf"] {
            assert!(is_custom_type(name), "{name} is a user-defined type");
        }
    }

    /// The standard-library wrapper lists are what the validator, the
    /// completion generator and the quick-fix all read a wrapped value
    /// through, so each entry has to be spelled the way
    /// [`extract_inner_type`] expects: the name and its opening `<`.
    #[test]
    fn test_wrapper_lists_unwrap_their_inner_type() {
        for wrapper in TRANSPARENT_WRAPPERS {
            let declared = format!("{wrapper}Server>");
            assert_eq!(
                extract_inner_type(&declared, wrapper),
                Some("Server"),
                "{wrapper}"
            );
        }
        for wrapper in SEQUENCE_WRAPPERS {
            let declared = format!("{wrapper}Server>");
            assert_eq!(
                extract_inner_type(&declared, wrapper),
                Some("Server"),
                "{wrapper}"
            );
        }

        // Every one of them is also a name the path strip and the generic
        // classifier know, or a qualified spelling would fall through as a
        // user-defined type again.
        for wrapper in TRANSPARENT_WRAPPERS.iter().chain(SEQUENCE_WRAPPERS.iter()) {
            let name = wrapper.trim_end_matches('<');
            assert!(
                crate::rust_analyzer::STD_GENERIC_NAMES.contains(&name),
                "{name} is missing from STD_GENERIC_NAMES"
            );
        }
    }

    /// The classifiers normalize internally, so a qualified std type lands on
    /// the same side of each of them as the bare spelling does.
    #[test]
    fn test_qualified_std_types_are_classified_as_std() {
        assert!(is_primitive_type("std::string::String"));
        assert!(!is_custom_type("std::string::String"));
        assert!(!is_custom_type("std::collections::HashMap<String, u32>"));
        assert!(!is_custom_type("std::option::Option<u64>"));
        assert!(is_custom_type("crate::config::Server"));
    }

    /// [`crate::rust_analyzer::is_std_generic`] takes a name the way
    /// [`is_custom_type`] hands it over, so the tests normalize first.
    fn is_std_generic_type(type_name: &str) -> bool {
        crate::rust_analyzer::is_std_generic(&normalize_type(type_name))
    }

    #[test]
    fn test_is_std_generic_type() {
        // Plain wrappers, including whitespace variants.
        assert!(is_std_generic_type("Option<Post>"));
        assert!(is_std_generic_type("Vec < Post >"));
        assert!(is_std_generic_type("HashMap<String, u32>"));
        assert!(is_std_generic_type("Box<Config>"));

        // Embedded in a composite type is matched consistently for every
        // wrapper, not just the map/set family.
        assert!(is_std_generic_type("(u8, Vec<Config>)"));
        assert!(is_std_generic_type("(u8, Box<Config>)"));
        assert!(is_std_generic_type("(u8, HashMap<String, u32>)"));

        // A struct/enum named similarly to a wrapper is not a std generic: the
        // trailing `<` is required.
        assert!(!is_std_generic_type("Options"));
        assert!(!is_std_generic_type("Post"));
    }

    /// A wrapper only counts immediately in front of a `<`, and every `<` in
    /// the name is a candidate rather than just the first — a wrapper nested
    /// inside a user-defined generic, or sitting after one, counts the same.
    #[test]
    fn test_is_std_generic_type_scans_every_generic() {
        assert!(is_std_generic_type("MyWrapper<Vec<u8>>"));
        assert!(is_std_generic_type("(MyGeneric<u8>,Arc<Config>)"));

        // A wrapper's name with no `<` behind it is just a name.
        assert!(!is_std_generic_type("Vec"));
        assert!(!is_std_generic_type("Option"));
        // ... and a generic that is nobody's wrapper stays custom.
        assert!(!is_std_generic_type("MyGeneric<u8>"));
        assert!(is_custom_type("MyGeneric<u8>"));
    }

    /// A name that merely *ends* with a wrapper's name counts as that wrapper:
    /// `MyVec<T>` reads as a `Vec`. That is what matching the substring `Vec<`
    /// has always done, and either way such a type resolves to nothing, so the
    /// behaviour is pinned here rather than quietly changed.
    #[test]
    fn test_is_std_generic_type_matches_a_name_ending_in_a_wrapper() {
        assert!(is_std_generic_type("MyVec<u8>"));
        assert!(!is_custom_type("MyVec<u8>"));
    }

    /// A composite type keeps its qualified names — the character walk in
    /// `unqualify_std_type` is what decides that, and the cheaper check for a
    /// colon that now runs in front of it must not reach a different answer.
    #[test]
    fn test_normalize_type_keeps_a_composite_holding_a_qualified_name() {
        assert_eq!(
            normalize_type("(u8, std::string::String)"),
            "(u8,std::string::String)"
        );
        assert_eq!(
            normalize_type("&std::string::String"),
            "&std::string::String"
        );
    }

    #[test]
    fn test_is_custom_type() {
        assert!(is_custom_type("Post"));
        assert!(!is_custom_type("bool"));
        assert!(!is_custom_type("Vec<Post>"));
        // A tuple embedding a std wrapper is not a resolvable custom type.
        assert!(!is_custom_type("(u8, Vec<Config>)"));
    }

    /// A tuple, an array, a slice and a reference are standard-library
    /// composites: the analyzer never registers one, and none of them is
    /// spelled with a `<` for [`crate::rust_analyzer::is_std_generic`] to
    /// catch. They used to be classified as user-defined types, which meant
    /// resolving them against the registered types, failing, and reporting
    /// `unknown-type` on every field declared as one — however correct the
    /// value written for it.
    #[test]
    fn test_is_custom_type_excludes_std_composites() {
        assert!(!is_custom_type("(String, u16)"));
        assert!(!is_custom_type("[u32; 3]"));
        assert!(!is_custom_type("[u8]"));
        assert!(!is_custom_type("&[u8]"));
        assert!(!is_custom_type("()"));
        // A name that merely *contains* one is still a type of its own.
        assert!(is_custom_type("Pair"));
    }

    /// RON writes a Rust tuple and a fixed-size array alike as a parenthesized
    /// tuple, so both reduce to the same thing here: a fixed list of
    /// positions, each with a declared type. The names are already normalized,
    /// which is how [`is_custom_type`] hands them over.
    #[test]
    fn test_tuple_element_types() {
        assert_eq!(
            tuple_element_types("(String,u16)"),
            Some(vec!["String", "u16"])
        );
        assert_eq!(
            tuple_element_types("[u32;3]"),
            Some(vec!["u32", "u32", "u32"])
        );
        // The unit type is a tuple of no elements.
        assert_eq!(tuple_element_types("()"), Some(Vec::new()));
        // A one-element tuple keeps the comma Rust needs to tell `(T,)` from a
        // parenthesized `T`, which must not count as a second position.
        assert_eq!(tuple_element_types("(u8,)"), Some(vec!["u8"]));
        // The commas inside a nested type belong to it, not to the tuple.
        assert_eq!(
            tuple_element_types("(Vec<(u8,u8)>,HashMap<String,u8>)"),
            Some(vec!["Vec<(u8,u8)>", "HashMap<String,u8>"])
        );

        // Not written as a tuple at all.
        assert_eq!(tuple_element_types("Vec<u8>"), None);
        assert_eq!(tuple_element_types("u8"), None);
        assert_eq!(tuple_element_types("Server"), None);
        // A sequence whose length the value decides, not the type, has no
        // fixed list of positions — nor has one the name doesn't spell out.
        assert_eq!(tuple_element_types("[u8]"), None);
        assert_eq!(tuple_element_types("[u8;LEN]"), None);
        // Nor one too long to be worth enumerating.
        assert_eq!(tuple_element_types("[u8;257]"), None);
        assert_eq!(tuple_element_types("[u8;256]").map(|e| e.len()), Some(256));
    }

    #[test]
    fn test_map_entry_types() {
        assert_eq!(
            map_entry_types("HashMap<String,Listener>"),
            Some(("String", "Listener"))
        );
        assert_eq!(
            map_entry_types("BTreeMap<u8,Vec<(u8,u8)>>"),
            Some(("u8", "Vec<(u8,u8)>"))
        );
        // A custom hasher is not part of what the document writes.
        assert_eq!(
            map_entry_types("HashMap<String,u8,RandomState>"),
            Some(("String", "u8"))
        );
        assert_eq!(map_entry_types("HashSet<String>"), None);
        assert_eq!(map_entry_types("Vec<u8>"), None);
        assert_eq!(map_entry_types("MyHashMap<String,u8>"), None);
    }

    #[test]
    fn test_edit_distance() {
        assert_eq!(edit_distance("name", "name"), 0);
        assert_eq!(edit_distance("nam", "name"), 1);
        assert_eq!(edit_distance("Name", "name"), 0); // case-insensitive
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn test_closest_name_suggests_typo() {
        let fields = ["ephemeral", "database", "timeout"];
        assert_eq!(closest_name("ephemerl", fields), Some("ephemeral"));
        assert_eq!(closest_name("databse", fields), Some("database"));
        // Case-only difference is still a match.
        assert_eq!(closest_name("Timeout", fields), Some("timeout"));
    }

    #[test]
    fn test_closest_name_rejects_unrelated() {
        let fields = ["ephemeral", "database", "timeout"];
        // Nothing close enough to be a plausible typo.
        assert_eq!(closest_name("hostname", fields), None);
        // A completely different short word should not match a short candidate.
        assert_eq!(closest_name("id", ["ip"]), None);
        // No candidates.
        assert_eq!(closest_name("anything", std::iter::empty()), None);
        // Three edits on a long name: within the old length/3 bound but past
        // the absolute cap, so it is no longer treated as a plausible typo.
        assert_eq!(closest_name("abcdefXYZj", ["abcdefghij"]), None);
    }

    #[test]
    fn test_missing_required_fields_accepts_alias() {
        let field = FieldInfo {
            name: "debug_mode".to_string(),
            type_name: "bool".to_string(),
            aliases: vec!["verbose".to_string()],
            ..Default::default()
        };
        // The container renames to camelCase, so the serialized name is
        // "debugMode" — but serde also accepts the alias.
        let fields = [("debugMode".to_string(), field)];

        assert!(
            missing_required_fields(&fields, |n| n == "verbose").is_empty(),
            "a field supplied under its alias is present"
        );
        assert!(
            missing_required_fields(&fields, |n| n == "debugMode").is_empty(),
            "a field supplied under its serialized name is present"
        );
        assert!(
            missing_required_fields(&fields, |n| n == "debug_mode").is_empty(),
            "a field supplied under its Rust name is present"
        );
        assert_eq!(
            missing_required_fields(&fields, |_| false).len(),
            1,
            "a field supplied under no name at all is missing"
        );
    }

    #[test]
    fn test_closest_name_picks_nearest() {
        // "prot" is one edit from "port" but two from "protocol"; pick the nearest.
        assert_eq!(closest_name("prot", ["port", "protocol"]), Some("port"));
    }
}
