use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Strip the spaces out of a type name so that `Vec < Post >` and `Vec<Post>`
/// compare equal. Type names reach the analyzer through
/// `quote!(#ty).to_string()`, which tokenizes with spaces around `::` and `<>`,
/// while cache keys are stored space-free.
///
/// Borrows when there is nothing to strip, which is the overwhelmingly common
/// case: a type name is normalized several times per value in a document, and
/// allocating a copy of each one was a measurable share of the time spent
/// validating a large file.
pub(crate) fn strip_type_whitespace(type_name: &str) -> Cow<'_, str> {
    match type_name.contains(' ') {
        true => Cow::Owned(type_name.replace(' ', "")),
        false => Cow::Borrowed(type_name),
    }
}

/// Drop the lifetime arguments out of a type name, which is never part of what
/// a RON document writes: `&'a str` is written exactly like a `&str` and
/// `Cow<'static, str>` like the `str` it borrows.
///
/// Applied before [`strip_type_whitespace`], not after: the space is the only
/// thing keeping the lifetime apart from the type that follows it, and
/// `& 'a str` stripped of its spaces first is the unsplittable `&'astr`. A
/// lifetime that is the first of several generic arguments takes the comma
/// behind it with it, so `Cow<'a, str>` reduces to `Cow<str>` and goes through
/// the same wrapper handling as a `Box<str>`.
///
/// Borrows unless the name actually holds a lifetime, which almost none do: a
/// configuration type is owned data.
pub(crate) fn strip_lifetimes(type_name: &str) -> Cow<'_, str> {
    if !type_name.contains('\'') {
        return Cow::Borrowed(type_name);
    }

    let mut out = String::with_capacity(type_name.len());
    let mut rest = type_name;
    while let Some(quote) = rest.find('\'') {
        out.push_str(&rest[..quote]);
        let after = &rest[quote + 1..];
        // The lifetime's name, then whatever separates it from the next
        // argument: a comma belongs to the lifetime being dropped, since the
        // argument list is one item shorter without it.
        let name_end = after
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(after.len());
        rest = after[name_end..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest);
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Standard-library generics the rest of the crate special-cases by name —
/// `Option<T>` makes a field optional, `Vec<T>` is written as an array, a
/// `HashMap` as a map, and so on. Every one of those checks matches the bare
/// spelling, by equality or by a `Name<` prefix.
pub(crate) const STD_GENERIC_NAMES: [&str; 19] = [
    "Option",
    "Vec",
    "VecDeque",
    "LinkedList",
    "BinaryHeap",
    "HashMap",
    "BTreeMap",
    "HashSet",
    "BTreeSet",
    "Result",
    "Box",
    "Rc",
    "Arc",
    "Cow",
    "Cell",
    "RefCell",
    "Mutex",
    "RwLock",
    "NonZero",
];

/// The RON value a standard-library type that takes no generic arguments is
/// written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StdShape {
    /// A quoted string: a path, an address.
    Text,
    /// An integer literal, bounded the way the primitive behind it is.
    Integer,
    /// A struct of fields the analyzer has no definition of, so nothing beyond
    /// "it is written as a struct" is checked.
    Record,
}

/// A standard-library type that takes no generic arguments and that the
/// analyzer will therefore never register, since it is not defined in the
/// workspace it reads.
///
/// Both fields are read by the `lsp` half alone. Without that feature the only
/// question asked of one of these is whether the name is a std type at all, so
/// what it is written as goes unread.
#[cfg_attr(not(feature = "lsp"), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct StdType {
    /// What RON writes it as.
    pub shape: StdShape,
    /// A value of that shape `ron` accepts, for the quick-fix that fills a
    /// required field of this type in. `""` is a `PathBuf` but not an
    /// `IpAddr`, so each type names its own rather than sharing one per shape.
    pub placeholder: &'static str,
}

/// The standard-library type `clean` — an already-normalized name — refers to,
/// or `None` when the name is not one of them.
///
/// Every check in the crate that asks whether a type is user-defined is really
/// asking whether the analyzer should have registered it, and the answer for
/// these is no: they are std types with no generic arguments to give them away
/// (unlike [`STD_GENERIC_NAMES`]) and no bracket to mark them as a composite.
/// Without this they were all read as user-defined, so a field declared as one
/// drew `unknown-type` on whatever value was written for it, the value drew
/// `type-mismatch` under the LSP's own idea of what the type accepts, and the
/// quick-fix for a missing one inserted `PathBuf()`.
pub(crate) fn std_type(clean: &str) -> Option<StdType> {
    let (shape, placeholder) = match clean {
        "PathBuf" | "Path" | "OsString" | "OsStr" => (StdShape::Text, "\"\""),
        "IpAddr" | "Ipv4Addr" => (StdShape::Text, "\"0.0.0.0\""),
        "Ipv6Addr" => (StdShape::Text, "\"::\""),
        "SocketAddr" | "SocketAddrV4" => (StdShape::Text, "\"0.0.0.0:0\""),
        "SocketAddrV6" => (StdShape::Text, "\"[::]:0\""),
        // The `NonZero` family, under either spelling, and whose own
        // placeholder cannot be the `0` the integer primitives get. The one
        // generic name here, since `NonZero<u32>` is written as the integer
        // rather than as anything of its own.
        _ if clean.starts_with("NonZero") && integer_bounds(clean).is_some() => {
            (StdShape::Integer, "1")
        }
        // Serde's representation of these two, which is stable and is the only
        // spelling `ron` reads them from.
        "Duration" => (StdShape::Record, "(secs: 0, nanos: 0)"),
        "SystemTime" => (
            StdShape::Record,
            "(secs_since_epoch: 0, nanos_since_epoch: 0)",
        ),
        _ => return None,
    };
    Some(StdType { shape, placeholder })
}

/// The inclusive range of integers the named Rust integer type accepts, or
/// `None` when `type_name` is not an integer type.
///
/// `isize`/`usize` are sized after the host, which is the machine the
/// configured application is built on in practice.
///
/// A `NonZero<T>` is ranged as its `T`, except that the unsigned ones start at
/// one: `ron` refuses a `0` for them outright ("Expected a nonzero u32"). A
/// zero in a *signed* `NonZero` is refused just the same but sits in the middle
/// of the range rather than at an end, so a range cannot say so and it goes
/// unreported — a missed error rather than a false one.
pub(crate) fn integer_bounds(type_name: &str) -> Option<(i128, i128)> {
    if let Some(tail) = type_name.strip_prefix("NonZero") {
        // `NonZeroU32` and `NonZero<u32>` are the one type under the two
        // spellings Rust accepts, and a document writes both as the integer.
        let primitive = match tail.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
            Some(argument) => Cow::Borrowed(argument),
            None => Cow::Owned(tail.to_ascii_lowercase()),
        };
        let (min, max) = integer_bounds(&primitive)?;
        // Only an unsigned range has the zero at its edge to raise.
        return Some((if min == 0 { 1 } else { min }, max));
    }
    Some(match type_name {
        "i8" => (i8::MIN as i128, i8::MAX as i128),
        "i16" => (i16::MIN as i128, i16::MAX as i128),
        "i32" => (i32::MIN as i128, i32::MAX as i128),
        "i64" => (i64::MIN as i128, i64::MAX as i128),
        "isize" => (isize::MIN as i128, isize::MAX as i128),
        "u8" => (0, u8::MAX as i128),
        "u16" => (0, u16::MAX as i128),
        "u32" => (0, u32::MAX as i128),
        "u64" => (0, u64::MAX as i128),
        "usize" => (0, usize::MAX as i128),
        // Everything a RON integer literal can be fits in these, so there is
        // nothing left to check — but they are still integer types, and saying
        // so is what keeps a value in one from being reported as a non-numeric
        // field.
        "i128" | "u128" => (i128::MIN, i128::MAX),
        _ => return None,
    })
}

/// Whether `head` names a standard-library type, and so one whose module path
/// [`unqualify_std_type`] drops. `String`/`str` are in the set because the
/// type checks match them by their bare spelling too, even though they are
/// primitives rather than one of the categories above.
fn is_std_type_name(head: &str) -> bool {
    STD_GENERIC_NAMES.contains(&head)
        || std_type(head).is_some()
        || matches!(head, "String" | "str")
}

/// Whether an already-normalized type name uses one of [`STD_GENERIC_NAMES`]
/// as a generic.
///
/// A wrapper counts anywhere it appears, not only at the start, so composite
/// types that merely embed one — e.g. the tuple `(u8, Vec<Config>)` — are
/// treated the same regardless of which wrapper they contain. Previously only
/// the map/set wrappers were matched this way, so a tuple embedding a `Vec` or
/// `Box` was misclassified as a custom type and drew a spurious "Unknown type"
/// diagnostic while the same tuple embedding a `HashMap` did not.
///
/// A name only counts when it is actually used as a generic, i.e. immediately
/// followed by `<` — a struct named `Options` is not an `Option`. Driven by
/// where the `<`s are rather than by the wrapper list: a wrapper can only
/// appear immediately in front of one, so a name holding no `<` at all is
/// settled by a single scan for it. Asking the question once per wrapper
/// instead meant a substring search per name — each building its own searcher
/// — over every type name the validator classified, and most of those are a
/// bare `Config` with no `<` anywhere in them.
pub(crate) fn is_std_generic(clean: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = clean[from..].find('<') {
        let before = &clean[..from + offset];
        if STD_GENERIC_NAMES.iter().any(|name| before.ends_with(name)) {
            return true;
        }
        from += offset + 1;
    }
    false
}

/// Drop the module path in front of a standard-library type name, so that the
/// `std::collections::HashMap<String, u32>` a crate that doesn't import the
/// type writes is recognised as the map it is.
///
/// The analyzer records a field's type exactly as the source spells it, and
/// `std::`-qualified names used to fall through every one of the checks in
/// [`is_std_type_name`] as if they named a user-defined type: a correct document
/// came back with `type-mismatch` on its arrays and `Some(..)`s,
/// `unknown-type` on its strings, an `Option` field reported as required, and
/// an "add missing fields" quick-fix that inserted `HashMap<String,u32>()`.
///
/// Anything else keeps its path: a user-defined `crate::config::Server` is
/// resolved against the registered types by that path, and only its final
/// segment going missing would make two types in different modules
/// indistinguishable.
fn unqualify_std_type(type_name: &str) -> &str {
    // Only the path in front of the outermost name is a candidate. The generic
    // arguments keep theirs and are unqualified when they are visited in turn.
    let head = &type_name[..type_name.find('<').unwrap_or(type_name.len())];

    // A name with no `:` in it carries no path to drop, which is almost every
    // name there is. One scan for the colon settles those, where the character
    // walk below costs a Unicode class lookup per character and the substring
    // search after it builds a searcher — and a type name is normalized
    // several times per value in a document.
    if !head.contains(':') {
        return type_name;
    }

    // A composite type — a tuple, a reference, an array — has no single head to
    // unqualify, and `rfind` below would otherwise cut it in half.
    if !head
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
    {
        return type_name;
    }

    let Some(offset) = head.rfind("::") else {
        return type_name;
    };
    match is_std_type_name(&head[offset + 2..]) {
        true => &type_name[offset + 2..],
        false => type_name,
    }
}

/// Reduce a type name to the spelling the crate's type checks are written
/// against: no lifetime arguments ([`strip_lifetimes`]), no whitespace
/// ([`strip_type_whitespace`]) and no module path in front of a
/// standard-library name ([`unqualify_std_type`]).
pub(crate) fn normalize_type_name(type_name: &str) -> Cow<'_, str> {
    match strip_lifetimes(type_name) {
        Cow::Borrowed(name) => unqualify_whitespace(name),
        // A name holding a lifetime has already been copied, and there are few
        // enough of those that reusing the copy is not worth threading the two
        // strips together.
        Cow::Owned(name) => Cow::Owned(unqualify_whitespace(&name).into_owned()),
    }
}

/// [`normalize_type_name`] past the lifetime strip.
fn unqualify_whitespace(type_name: &str) -> Cow<'_, str> {
    match strip_type_whitespace(type_name) {
        Cow::Borrowed(clean) => Cow::Borrowed(unqualify_std_type(clean)),
        // The whitespace strip already allocated, so reuse that string unless
        // the path strip actually shortens it.
        Cow::Owned(clean) => match unqualify_std_type(&clean) {
            unqualified if unqualified.len() == clean.len() => Cow::Owned(clean),
            unqualified => Cow::Owned(unqualified.to_string()),
        },
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FieldInfo {
    pub name: String,
    pub type_name: String,
    pub docs: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub has_default: bool,
    /// `#[serde(rename = "...")]` on the field
    #[serde(default)]
    pub rename: Option<String>,
    /// `#[serde(alias = "...")]` names on the field. Serde accepts each of these
    /// (in addition to the serialized name) when deserializing, so they must not
    /// be flagged as unknown fields.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// `#[serde(skip)]` or `#[serde(skip_deserializing)]` on the field
    #[serde(default)]
    pub skip: bool,
    /// `#[serde(flatten)]` on the field
    #[serde(default)]
    pub flatten: bool,
}

impl FieldInfo {
    pub fn is_optional(&self) -> bool {
        if self.has_default {
            return true;
        }
        // Normalize first: type names from `quote!` are tokenized with spaces
        // (e.g. "Option < String >"), and a crate that doesn't import the type
        // spells it `std::option::Option<String>`. Borrowing when there is
        // nothing to strip matters here: every struct value in a document asks
        // this of every field its type declares, and copying the type name to
        // answer it was an allocation per field per value.
        normalize_type_name(&self.type_name).starts_with("Option<")
    }

    /// Whether this field is positional rather than named — i.e. a field of a
    /// tuple or newtype struct/variant, whose name is its index (`"0"`, `"1"`,
    /// ...) instead of a real identifier. Named-field validation does not apply
    /// to these.
    pub fn is_positional(&self) -> bool {
        self.name.parse::<usize>().is_ok()
    }

    /// The name serde expects for this field, honoring `#[serde(rename)]`
    /// and the container's `#[serde(rename_all)]` convention.
    ///
    /// Borrows unless a `rename_all` convention actually has to be applied.
    /// Matching a RON field name against a type's fields asks this of every
    /// candidate, so a type with a dozen fields answered a single lookup with a
    /// dozen throwaway copies of its own field names.
    pub fn serialized_name(&self, container_rename_all: Option<&str>) -> Cow<'_, str> {
        if let Some(rename) = &self.rename {
            return Cow::Borrowed(rename);
        }
        match container_rename_all {
            Some(convention) => Cow::Owned(rename_all_field(&self.name, convention)),
            None => Cow::Borrowed(&self.name),
        }
    }

    /// Whether serde would accept `name` for this field when deserializing:
    /// its serialized name (honoring rename/rename_all), its Rust name, or any
    /// `#[serde(alias = "...")]`.
    pub fn accepts_name(&self, name: &str, container_rename_all: Option<&str>) -> bool {
        self.name == name
            || self.serialized_name(container_rename_all) == name
            || self.aliases.iter().any(|a| a == name)
    }
}

/// Apply a serde `rename_all` convention to a field name.
/// Mirrors serde's conversions, which assume snake_case Rust field names.
///
/// The hyphenated conventions are converted faithfully, but the resulting name
/// is not a bare RON identifier and the LSP cannot currently round-trip it —
/// see the README's "Serde attributes" section.
pub fn rename_all_field(name: &str, convention: &str) -> String {
    match convention {
        "lowercase" | "snake_case" => name.to_string(),
        "UPPERCASE" | "SCREAMING_SNAKE_CASE" => name.to_ascii_uppercase(),
        "PascalCase" => name
            .split('_')
            .map(capitalize)
            .collect(),
        "camelCase" => {
            let pascal = rename_all_field(name, "PascalCase");
            uncapitalize(&pascal)
        }
        "kebab-case" => name.replace('_', "-"),
        "SCREAMING-KEBAB-CASE" => name.to_ascii_uppercase().replace('_', "-"),
        _ => name.to_string(),
    }
}

/// Apply a serde `rename_all` convention to an enum variant name.
/// Mirrors serde's conversions, which assume PascalCase Rust variant names.
///
/// The hyphenated conventions are converted faithfully, but the resulting name
/// is not a bare RON identifier and the LSP cannot currently round-trip it —
/// see the README's "Serde attributes" section.
pub fn rename_all_variant(name: &str, convention: &str) -> String {
    match convention {
        "lowercase" => name.to_ascii_lowercase(),
        "UPPERCASE" => name.to_ascii_uppercase(),
        "PascalCase" => name.to_string(),
        "camelCase" => uncapitalize(name),
        "snake_case" => pascal_to_snake(name),
        "SCREAMING_SNAKE_CASE" => pascal_to_snake(name).to_ascii_uppercase(),
        "kebab-case" => pascal_to_snake(name).replace('_', "-"),
        "SCREAMING-KEBAB-CASE" => pascal_to_snake(name)
            .to_ascii_uppercase()
            .replace('_', "-"),
        _ => name.to_string(),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn uncapitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn pascal_to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnumVariant {
    pub name: String,
    pub fields: Vec<FieldInfo>,
    pub docs: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// `#[serde(rename = "...")]` on the variant
    #[serde(default)]
    pub rename: Option<String>,
}

impl EnumVariant {
    /// The fields serde accepts for this variant as `(serialized_name, field)`
    /// pairs, with `skip` fields excluded.
    ///
    /// No container convention is applied: a field-level `#[serde(rename)]` is
    /// honored, but the enum's `#[serde(rename_all_fields = "...")]` is not
    /// extracted, so variants of such an enum are validated against the Rust
    /// field names rather than the names serde expects. See
    /// <https://git.cilki.net/cilki/roniker/issues/72>.
    pub fn effective_fields(&self) -> Vec<(String, FieldInfo)> {
        self.fields
            .iter()
            .filter(|f| !f.skip)
            .map(|f| (f.serialized_name(None).into_owned(), f.clone()))
            .collect()
    }

    /// The name serde expects for this variant, honoring `#[serde(rename)]`
    /// and the enum's `#[serde(rename_all)]` convention. Borrows unless a
    /// `rename_all` convention actually has to be applied.
    pub fn serialized_name(&self, container_rename_all: Option<&str>) -> Cow<'_, str> {
        if let Some(rename) = &self.rename {
            return Cow::Borrowed(rename);
        }
        match container_rename_all {
            Some(convention) => Cow::Owned(rename_all_variant(&self.name, convention)),
            None => Cow::Borrowed(&self.name),
        }
    }

    /// Whether a document naming `name` is naming this variant: its serialized
    /// name (honoring `rename`/`rename_all`) or its Rust name, the latter also
    /// matched case-insensitively.
    ///
    /// The case-insensitive fallback is deliberately lenient. A `rename_all`
    /// convention the analyzer failed to record would otherwise turn every
    /// variant in a correct document into an "unknown variant" error, and a
    /// false error is worse than a missed one. This is the one place that
    /// decides the question, so every feature answers it the same way.
    pub fn accepts_name(&self, name: &str, container_rename_all: Option<&str>) -> bool {
        self.name == name
            || self.serialized_name(container_rename_all) == name
            || self.name.eq_ignore_ascii_case(name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TypeKind {
    Struct(Vec<FieldInfo>),
    Enum(Vec<EnumVariant>),
}

impl Default for TypeKind {
    fn default() -> Self {
        TypeKind::Struct(Vec::new())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypeInfo {
    pub name: String,
    pub kind: TypeKind,
    pub docs: Option<String>,
    pub source_file: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// Container-level `#[serde(default)]`: serde fills in every absent field,
    /// so none of them are required. A bare `#[derive(Default)]` does not set
    /// this — it has no bearing on what serde will accept.
    pub has_default: bool,
    /// `#[serde(rename_all = "...")]` on the container (e.g. "camelCase")
    #[serde(default)]
    pub rename_all: Option<String>,
}

impl TypeInfo {
    pub fn fields(&self) -> Option<&Vec<FieldInfo>> {
        match &self.kind {
            TypeKind::Struct(fields) => Some(fields),
            TypeKind::Enum(_) => None,
        }
    }

    pub fn find_field(&self, field_name: &str) -> Option<&FieldInfo> {
        match &self.kind {
            TypeKind::Struct(fields) => fields.iter().find(|f| f.name == field_name),
            TypeKind::Enum(variants) => {
                // Search through all variants' fields
                variants
                    .iter()
                    .flat_map(|v| &v.fields)
                    .find(|f| f.name == field_name)
            }
        }
    }

    /// Find the variant that a document naming `variant_name` is referring to,
    /// honoring serde's `rename`/`rename_all` — see
    /// [`EnumVariant::accepts_name`] for exactly which names are accepted.
    /// `None` for a struct.
    pub fn find_variant(&self, variant_name: &str) -> Option<&EnumVariant> {
        let TypeKind::Enum(variants) = &self.kind else {
            return None;
        };
        let rename_all = self.rename_all.as_deref();
        variants
            .iter()
            .find(|v| v.accepts_name(variant_name, rename_all))
    }

    /// Find a field by the name serde expects in the serialized form
    /// (honoring rename/rename_all), falling back to the Rust field name.
    pub fn find_field_serialized(&self, field_name: &str) -> Option<&FieldInfo> {
        let rename_all = self.rename_all.as_deref();
        match &self.kind {
            TypeKind::Struct(fields) => fields
                .iter()
                .find(|f| f.accepts_name(field_name, rename_all)),
            TypeKind::Enum(variants) => variants
                .iter()
                .flat_map(|v| &v.fields)
                .find(|f| f.accepts_name(field_name, None)),
        }
    }

    /// The fields serde accepts for this struct: `skip` fields are excluded,
    /// `flatten` fields are expanded into the flattened type's fields
    /// (recursively, depth-limited), and names are serialized names.
    pub fn effective_fields(&self, analyzer: &RustAnalyzer) -> Vec<(String, FieldInfo)> {
        self.effective_fields_depth(analyzer, 8)
    }

    fn effective_fields_depth(
        &self,
        analyzer: &RustAnalyzer,
        depth: usize,
    ) -> Vec<(String, FieldInfo)> {
        let mut out = Vec::new();
        let Some(fields) = self.fields() else {
            return out;
        };
        for field in fields {
            if field.skip {
                continue;
            }
            if field.flatten {
                if depth > 0
                    && let Some(inner) = analyzer.get_type_info(&field.type_name)
                {
                    out.extend(inner.effective_fields_depth(analyzer, depth - 1));
                }
                continue;
            }
            let serialized = field.serialized_name(self.rename_all.as_deref());
            out.push((serialized.into_owned(), field.clone()));
        }
        out
    }

    /// Whether this struct has a `flatten` field whose target type cannot be
    /// resolved (e.g. a HashMap). Serde then accepts arbitrary extra keys, so
    /// unknown-field checks should be suppressed.
    pub fn has_unresolved_flatten(&self, analyzer: &RustAnalyzer) -> bool {
        self.fields().is_some_and(|fields| {
            fields
                .iter()
                .any(|f| f.flatten && analyzer.get_type_info(&f.type_name).is_none())
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustAnalyzer {
    pub root_type: Option<String>,
    type_cache: HashMap<String, TypeInfo>,
    type_aliases: HashMap<String, String>,
    /// Index from every `::`-delimited tail of a registered type path to that
    /// path, so that resolving the bare name a field declaration uses
    /// (`ServerConfig`) against the fully-qualified key the build script
    /// registered (`crate::config::ServerConfig`) is a hash lookup instead of a
    /// scan of every registered type. Built on first use and dropped whenever
    /// the cache changes; never serialized, since it is derivable.
    #[serde(skip)]
    path_tails: OnceLock<HashMap<String, String>>,
    /// [`TypeInfo::effective_fields`] for every registered type, so that the
    /// diagnostics walk does not re-derive — and re-allocate — a type's field
    /// list once per value written against it. A document holding a thousand
    /// elements of the same struct asked for the same answer a thousand times,
    /// each one a fresh copy of every field's name, type and docs. Built on
    /// first use and dropped whenever the cache changes, exactly like
    /// `path_tails`; never serialized, since it is derivable.
    #[serde(skip)]
    effective_fields: OnceLock<HashMap<String, Vec<(String, FieldInfo)>>>,
}

impl Default for RustAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl RustAnalyzer {
    /// Create a new RustAnalyzer without a root type.
    pub fn new() -> Self {
        Self {
            root_type: None,
            type_cache: HashMap::new(),
            type_aliases: HashMap::new(),
            path_tails: OnceLock::new(),
            effective_fields: OnceLock::new(),
        }
    }

    /// Create a new RustAnalyzer with the given root type path (e.g., "crate::models::Config")
    pub fn with_root_type(root_type: impl Into<String>) -> Self {
        Self {
            root_type: Some(root_type.into()),
            ..Self::new()
        }
    }

    /// Get the TypeInfo for the root type, if one is set.
    pub fn root_type_info(&self) -> Option<&TypeInfo> {
        self.root_type
            .as_deref()
            .and_then(|t| self.get_type_info(t))
    }

    /// Register a type directly with the analyzer.
    ///
    /// This is useful when you have pre-constructed TypeInfo objects.
    pub fn add_type(&mut self, type_info: TypeInfo) {
        self.type_cache.insert(type_info.name.clone(), type_info);
        self.invalidate_derived();
    }

    /// Register a type alias.
    ///
    /// # Arguments
    /// * `alias` - The alias name (e.g., "crate::MyAlias")
    /// * `target` - The target type (e.g., "crate::SomeType")
    pub fn add_type_alias(&mut self, alias: &str, target: &str) {
        self.type_aliases
            .insert(alias.to_string(), target.to_string());
    }

    /// Remove a type from the analyzer.
    ///
    /// # Returns
    /// The removed TypeInfo if it existed
    pub fn remove_type(&mut self, type_path: &str) -> Option<TypeInfo> {
        self.invalidate_derived();
        self.type_cache.remove(type_path)
    }

    /// Clear all types and aliases from the analyzer.
    pub fn clear(&mut self) {
        self.type_cache.clear();
        self.type_aliases.clear();
        self.invalidate_derived();
    }

    /// Drop everything derived from the registered types, so that the next
    /// lookup rebuilds it. Every path that changes `type_cache` has to come
    /// through here, or an index outlives the types it was built from and
    /// keeps answering for them.
    fn invalidate_derived(&mut self) {
        self.path_tails.take();
        self.effective_fields.take();
    }

    /// Every `::`-delimited tail of every registered type path, mapped to the
    /// path it came from: `crate::config::Server` contributes `config::Server`
    /// and `Server`. The full path itself is left out, since an exact lookup
    /// finds that without help.
    ///
    /// When two types share a tail the first one encountered wins, which
    /// matches what the scan this replaces did — it returned whichever
    /// candidate the `HashMap` happened to iterate first.
    fn path_tails(&self) -> &HashMap<String, String> {
        self.path_tails.get_or_init(|| {
            let mut tails = HashMap::new();
            for key in self.type_cache.keys() {
                for (offset, separator) in key.match_indices("::") {
                    tails
                        .entry(key[offset + separator.len()..].to_string())
                        .or_insert_with(|| key.clone());
                }
            }
            tails
        })
    }

    /// The fields serde accepts for `type_info`, answered from a memo when the
    /// type is one of the registered ones — which, in the walks over a
    /// document, it always is.
    ///
    /// [`TypeInfo::effective_fields`] has to copy the field list it returns,
    /// and the same type is asked for its fields once per value written
    /// against it: a document with a long array of one struct paid for a fresh
    /// copy of that struct's whole field list per element. The memo makes that
    /// one copy for the whole document instead.
    pub fn effective_fields(&self, type_info: &TypeInfo) -> Cow<'_, [(String, FieldInfo)]> {
        match self.effective_fields_index().get(&type_info.name) {
            Some(fields) => Cow::Borrowed(fields),
            // A type that was never registered has no entry; it is still
            // reachable, e.g. a root type handed straight to the validator.
            None => Cow::Owned(type_info.effective_fields(self)),
        }
    }

    /// [`TypeInfo::effective_fields`] for every registered type, keyed by the
    /// path it is registered under. Derived once and dropped by
    /// [`Self::invalidate_derived`], like [`Self::path_tails`].
    fn effective_fields_index(&self) -> &HashMap<String, Vec<(String, FieldInfo)>> {
        self.effective_fields.get_or_init(|| {
            self.type_cache
                .iter()
                .map(|(name, info)| (name.clone(), info.effective_fields(self)))
                .collect()
        })
    }

    pub fn get_type_info(&self, type_path: &str) -> Option<&TypeInfo> {
        // Strip whitespace from the lookup key. Type names extracted from `syn`
        // via `quote!(#ty).to_string()` are tokenized with spaces around `::`
        // and `<>`, but cache keys are stored space-free. Without this every
        // call site would have to remember to pre-normalize.
        let normalized = normalize_type_name(type_path);
        let lookup = normalized.as_ref();

        // Resolve type aliases first
        let resolved_type = self
            .type_aliases
            .get(lookup)
            .map(|s| s.as_str())
            .unwrap_or(lookup);

        // Check cache with exact match
        if let Some(info) = self.type_cache.get(resolved_type) {
            return Some(info);
        }
        // Also try the original (normalized) type path
        if let Some(info) = self.type_cache.get(lookup) {
            return Some(info);
        }

        // If not found by exact match, try finding by simple name
        // e.g., "PostType" should match "crate::models::PostType"
        let tails = self.path_tails();
        tails
            .get(resolved_type)
            .or_else(|| tails.get(lookup))
            .and_then(|key| self.type_cache.get(key))
    }

    /// Get all types registered with the analyzer
    pub fn get_all_types(&self) -> Vec<&TypeInfo> {
        self.type_cache.values().collect()
    }

    /// Get the number of types registered with the analyzer
    pub fn type_count(&self) -> usize {
        self.type_cache.len()
    }

    /// Check if a type exists in the analyzer
    pub fn has_type(&self, type_path: &str) -> bool {
        self.get_type_info(type_path).is_some()
    }

    /// Validate that all field types referenced in registered types are known.
    ///
    /// This checks that every struct field and enum variant field references
    /// a type that is either:
    /// - A primitive type (bool, i32, String, etc.)
    /// - A standard library generic (Option, Vec, HashMap, etc.)
    /// - A type registered with this analyzer
    ///
    /// # Returns
    /// A list of (type_name, field_name, unknown_type) tuples for any unknown types found.
    pub fn validate_field_types(&self) -> Vec<(String, String, String)> {
        let mut errors = Vec::new();

        for type_info in self.type_cache.values() {
            let fields: Vec<&FieldInfo> = match &type_info.kind {
                TypeKind::Struct(fields) => fields.iter().collect(),
                TypeKind::Enum(variants) => variants.iter().flat_map(|v| &v.fields).collect(),
            };

            for field in fields {
                if let Some(unknown) = self.check_field_type_known(&field.type_name) {
                    errors.push((type_info.name.clone(), field.name.clone(), unknown));
                }
            }
        }

        errors
    }

    /// Check if a field type is known, returning the unknown type name if not.
    fn check_field_type_known(&self, type_name: &str) -> Option<String> {
        let clean = normalize_type_name(type_name).into_owned();

        // Primitive types are always known
        let primitives = [
            "bool", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128",
            "usize", "f32", "f64", "char", "String", "&str", "str", "()",
        ];
        if primitives.contains(&clean.as_str()) {
            return None;
        }

        // A standard-library type with no generic arguments — a `PathBuf`, a
        // `SocketAddr` — is not defined in the workspace this analyzer reads,
        // so its absence from the cache says nothing about the build script.
        if std_type(&clean).is_some() {
            return None;
        }

        // Check generic wrappers and recurse into inner type
        let wrappers = ["Option<", "Vec<", "Box<", "Rc<", "Arc<"];
        for wrapper in wrappers {
            if clean.starts_with(wrapper) && clean.ends_with('>') {
                let inner = &clean[wrapper.len()..clean.len() - 1];
                return self.check_field_type_known(inner);
            }
        }

        // Every other standard-library generic, which we don't recurse into
        // for now.
        if is_std_generic(&clean) {
            return None;
        }

        // Check if it's a known custom type
        if self.get_type_info(&clean).is_some() {
            return None;
        }

        // Unknown type
        Some(clean)
    }
}

#[cfg(feature = "analyze")]
mod analyze;

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(path: &str) -> RustAnalyzer {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: path.to_string(),
            ..Default::default()
        });
        analyzer
    }

    /// A field declaration names a type the way the Rust source spells it,
    /// which is rarely the fully-qualified path the build script registered it
    /// under. Any `::`-delimited tail of that path has to resolve.
    #[test]
    fn test_get_type_info_resolves_any_path_tail() {
        let analyzer = registered("crate::deeply::nested::MyStruct");

        for lookup in [
            "crate::deeply::nested::MyStruct",
            "deeply::nested::MyStruct",
            "nested::MyStruct",
            "MyStruct",
            "crate :: deeply :: nested :: MyStruct",
        ] {
            assert_eq!(
                analyzer.get_type_info(lookup).map(|t| t.name.as_str()),
                Some("crate::deeply::nested::MyStruct"),
                "{lookup} should resolve"
            );
        }

        // A tail has to start at a path segment, not in the middle of one.
        assert!(analyzer.get_type_info("Struct").is_none());
        assert!(analyzer.get_type_info("ested::MyStruct").is_none());
    }

    /// A lifetime is never part of what a RON document writes, and leaving it
    /// in the normalized name left the type unrecognisable: `Cow<'static,str>`
    /// matched no wrapper and `&'astr` no primitive, so both were read as
    /// user-defined types the analyzer had failed to register.
    #[test]
    fn test_normalize_type_name_drops_lifetimes() {
        for (spelled, normalized) in [
            ("& 'a str", "&str"),
            ("&'a str", "&str"),
            ("& 'static str", "&str"),
            ("Cow < 'static , str >", "Cow<str>"),
            ("Cow<'a, str>", "Cow<str>"),
            ("std :: borrow :: Cow < 'a , str >", "Cow<str>"),
            ("Cow<'a, Vec<String>>", "Cow<Vec<String>>"),
            ("Vec < Request < 'a > >", "Vec<Request<>>"),
            // A reference to a user-defined type keeps the path that resolves
            // it; only the lifetime goes.
            ("& 'a crate::config::Server", "&crate::config::Server"),
        ] {
            assert_eq!(normalize_type_name(spelled), normalized, "{spelled}");
        }

        // Nothing to drop: the overwhelmingly common case, which must not
        // allocate a copy of the name to say so.
        assert!(matches!(
            normalize_type_name("Vec<String>"),
            Cow::Borrowed("Vec<String>")
        ));
    }

    /// The standard-library types with no generic arguments to give them away.
    /// Each is absent from the analyzer's cache because it is not defined in
    /// the workspace the analyzer read, which is not the same thing as a type
    /// the build script forgot.
    #[test]
    fn test_std_type_covers_the_names_configs_use() {
        for (name, shape) in [
            ("PathBuf", StdShape::Text),
            ("SocketAddr", StdShape::Text),
            ("Ipv6Addr", StdShape::Text),
            ("NonZeroU32", StdShape::Integer),
            ("NonZeroIsize", StdShape::Integer),
            ("Duration", StdShape::Record),
            ("SystemTime", StdShape::Record),
        ] {
            assert_eq!(std_type(name).map(|s| s.shape), Some(shape), "{name}");
        }

        // A user-defined type, a primitive and a std generic are all none of
        // these: the generics are [`STD_GENERIC_NAMES`]' business, and a name
        // that merely starts like one is nobody's.
        for name in ["Server", "u32", "Vec<String>", "NonZeroThing", "Durations"] {
            assert!(std_type(name).is_none(), "{name}");
        }
    }

    /// The module path in front of one of them is dropped like any other std
    /// name's, since a crate that doesn't import the type writes it out.
    #[test]
    fn test_qualified_std_types_are_unqualified() {
        for (spelled, normalized) in [
            ("std::path::PathBuf", "PathBuf"),
            ("std :: net :: SocketAddr", "SocketAddr"),
            ("std::time::Duration", "Duration"),
            ("std::num::NonZeroU32", "NonZeroU32"),
            ("std::collections::VecDeque<String>", "VecDeque<String>"),
            // A user-defined type keeps its path, which is what distinguishes
            // two types registered under the same final segment.
            ("crate::config::Server", "crate::config::Server"),
            // A name whose final segment *is* a std name loses its path like
            // any other std name, as `crate::config::Option` already did.
            // Harmless because [`RustAnalyzer::get_type_info`] resolves any
            // path tail, so a registered `crate::config::Duration` is still
            // found under the bare `Duration` — and every caller that can
            // consult the analyzer asks it before the standard-library table.
            ("crate::config::Duration", "Duration"),
        ] {
            assert_eq!(normalize_type_name(spelled), normalized, "{spelled}");
        }
    }

    /// A `NonZero<T>` is ranged as its `T`, except that `ron` refuses a `0`
    /// for the unsigned ones outright, so one is the bottom of the range.
    #[test]
    fn test_integer_bounds_of_the_nonzero_family() {
        assert_eq!(integer_bounds("NonZeroU8"), Some((1, u8::MAX as i128)));
        assert_eq!(integer_bounds("NonZeroU32"), Some((1, u32::MAX as i128)));
        assert_eq!(
            integer_bounds("NonZeroI32"),
            Some((i32::MIN as i128, i32::MAX as i128))
        );
        assert_eq!(integer_bounds("NonZeroUsize").map(|(min, _)| min), Some(1));

        // `NonZero<u32>` is the same type under Rust's other spelling for it,
        // and has to be ranged the same way.
        assert_eq!(integer_bounds("NonZero<u32>"), integer_bounds("NonZeroU32"));
        assert_eq!(integer_bounds("NonZero<i32>"), integer_bounds("NonZeroI32"));
        assert_eq!(
            std_type("NonZero<u32>").map(|s| s.placeholder),
            Some("1"),
            "and offered the same value"
        );

        // Not an integer type, and not one of them just for starting with the
        // name.
        assert_eq!(integer_bounds("NonZeroFoo"), None);
        assert_eq!(integer_bounds("NonZero<Foo>"), None);
        assert_eq!(integer_bounds("PathBuf"), None);
    }

    /// [`RustAnalyzer::validate_field_types`] is the build script's own view of
    /// "did I forget a source file?", and it has to agree with the LSP's: a
    /// `PathBuf` field is not a missing source file.
    #[test]
    fn test_validate_field_types_accepts_std_types() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "crate::Config".to_string(),
            kind: TypeKind::Struct(
                [
                    "PathBuf",
                    "std::path::PathBuf",
                    "Option<PathBuf>",
                    "Vec<SocketAddr>",
                    "NonZeroU32",
                    "Duration",
                    "VecDeque<String>",
                    "Cow<'static, str>",
                    "&'a str",
                ]
                .iter()
                .enumerate()
                .map(|(i, type_name)| FieldInfo {
                    name: format!("f{i}"),
                    type_name: type_name.to_string(),
                    ..Default::default()
                })
                .collect(),
            ),
            ..Default::default()
        });

        assert_eq!(analyzer.validate_field_types(), Vec::new());
    }

    /// A field declared `std::option::Option<T>` — the spelling a crate that
    /// doesn't rely on the prelude uses — is as optional as a bare
    /// `Option<T>`. Reading only the bare spelling reported it as a
    /// `missing-required-field` on a document serde deserializes happily.
    #[test]
    fn test_is_optional_reads_a_qualified_option() {
        let optional = |type_name: &str| {
            FieldInfo {
                type_name: type_name.to_string(),
                ..Default::default()
            }
            .is_optional()
        };

        assert!(optional("Option<u64>"));
        assert!(optional("std::option::Option<u64>"));
        assert!(optional("core::option::Option<u64>"));
        assert!(optional("std :: option :: Option < u64 >"));

        assert!(!optional("u64"));
        // A user-defined type whose name merely ends in `Option` is not one.
        assert!(!optional("crate::config::MyOption<u64>"));
    }

    /// Unqualifying std names must not cost a registered type its own path:
    /// `crate::config::Box` is a user type that happens to share a final
    /// segment with a std one, and the path is what identifies it.
    #[test]
    fn test_get_type_info_resolves_a_user_type_named_like_a_std_one() {
        let analyzer = registered("crate::config::Box");

        for lookup in ["crate::config::Box", "config::Box", "Box"] {
            assert_eq!(
                analyzer.get_type_info(lookup).map(|t| t.name.as_str()),
                Some("crate::config::Box"),
                "{lookup} should resolve"
            );
        }
    }

    /// Tail lookups are answered from an index built on first use, so every way
    /// of changing the registered types has to invalidate it. Looking a type up
    /// before the change is what makes the stale index observable.
    #[test]
    fn test_get_type_info_sees_changes_after_a_lookup() {
        let mut analyzer = registered("crate::config::First");
        assert!(analyzer.get_type_info("First").is_some());

        analyzer.add_type(TypeInfo {
            name: "crate::config::Second".to_string(),
            ..Default::default()
        });
        assert!(
            analyzer.get_type_info("Second").is_some(),
            "a type added after a lookup is still found by its tail"
        );

        analyzer.remove_type("crate::config::Second");
        assert!(
            analyzer.get_type_info("Second").is_none(),
            "a removed type is not found by its tail"
        );

        analyzer.clear();
        assert!(
            analyzer.get_type_info("First").is_none(),
            "a cleared analyzer resolves nothing"
        );
    }

    /// A type's effective fields are answered from an index built on first
    /// use, so redefining the type has to invalidate it — otherwise the
    /// validator keeps checking documents against the fields the type used to
    /// have. Asking once before the change is what makes a stale index
    /// observable.
    #[test]
    fn test_effective_fields_sees_changes_after_a_lookup() {
        let mut analyzer = RustAnalyzer::new();
        let with_fields = |fields: Vec<&str>| TypeInfo {
            name: "crate::config::Config".to_string(),
            kind: TypeKind::Struct(
                fields
                    .into_iter()
                    .map(|name| FieldInfo {
                        name: name.to_string(),
                        type_name: "bool".to_string(),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        };

        analyzer.add_type(with_fields(vec!["host"]));
        let config = analyzer.get_type_info("Config").unwrap().clone();
        assert_eq!(
            analyzer
                .effective_fields(&config)
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["host"]
        );

        analyzer.add_type(with_fields(vec!["host", "port"]));
        let config = analyzer.get_type_info("Config").unwrap().clone();
        assert_eq!(
            analyzer
                .effective_fields(&config)
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["host", "port"],
            "a redefined type's new fields are what the walk is given"
        );

        // Unregistering the type leaves the index with nothing to say about
        // it, so the answer comes from the `TypeInfo` handed in rather than
        // from a leftover entry under the same name.
        analyzer.remove_type("crate::config::Config");
        assert_eq!(
            analyzer
                .effective_fields(&with_fields(vec!["replacement"]))
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["replacement"],
            "a removed type is not answered from the index it used to be in"
        );
    }

    /// A type the analyzer never registered still has to answer for its
    /// fields: the root type can be handed straight to the validator.
    #[test]
    fn test_effective_fields_of_an_unregistered_type() {
        let analyzer = RustAnalyzer::new();
        let config = TypeInfo {
            name: "crate::config::Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "host".to_string(),
                type_name: "String".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert_eq!(
            analyzer
                .effective_fields(&config)
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["host"]
        );
    }

    /// An alias is resolved before the type path is looked up, including when
    /// what it resolves to is only a tail of a registered path.
    #[test]
    fn test_get_type_info_resolves_alias_by_tail() {
        let mut analyzer = registered("crate::config::Target");
        analyzer.add_type_alias("crate::Alias", "Target");

        assert_eq!(
            analyzer
                .get_type_info("crate::Alias")
                .map(|t| t.name.as_str()),
            Some("crate::config::Target")
        );
    }
}
