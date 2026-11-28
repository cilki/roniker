//! Navigation through nested RON type contexts.
use super::tree_sitter_parser::TypeContext;
use super::type_utils::{
    ARRAY_WRAPPERS, map_entry_types, normalize_type, short_name, single_value_wrapped_type,
    wrapped_type,
};
use crate::rust_analyzer::{RustAnalyzer, TypeInfo};

/// Navigate through nested type contexts to find the innermost type.
/// The first context is the document's root value, which the caller has
/// already resolved into `start`; navigation begins from the second context.
///
/// Every answer is one of the analyzer's own registered types, borrowed from
/// it rather than copied: a type carries every field's name, type and docs,
/// and a request used to copy all of that once per nesting level it stepped
/// through.
pub fn navigate_type_contexts<'a>(
    analyzer: &'a RustAnalyzer,
    start: Option<&'a TypeInfo>,
    contexts: &[TypeContext],
) -> Option<&'a TypeInfo> {
    contexts.iter().skip(1).try_fold(start?, |info, context| {
        step_type_context(analyzer, info, context)
    })
}

/// Resolve one step of the nesting path: the type of the struct value that
/// `context` describes, nested directly inside `info`.
///
/// Callers that already know the enclosing struct's type — a walk down the
/// document, say — can step with this instead of navigating the whole path from
/// the root again.
pub fn step_type_context<'a>(
    analyzer: &'a RustAnalyzer,
    info: &TypeInfo,
    context: &TypeContext,
) -> Option<&'a TypeInfo> {
    match &context.type_name {
        Some(type_name) => step_by_type_name(analyzer, info, type_name),
        // The value omitted its struct name, so there is no name to look the
        // type up by. The field it is the value of declares it.
        None => context
            .field_name
            .as_deref()
            .and_then(|field| step_by_field_name(analyzer, info, field)),
    }
}

/// Resolve the type of a named struct value nested in `info`.
fn step_by_type_name<'a>(
    analyzer: &'a RustAnalyzer,
    info: &TypeInfo,
    context_name: &str,
) -> Option<&'a TypeInfo> {
    // Try to find the context type as a field's type.
    // Use exact match on the last component of the type name to avoid
    // substring matches (e.g., don't match "Post" with "PostType").
    if let Some(fields) = info.fields()
        && let Some(field) = fields.iter().find(|f| {
            let field_type_last = short_name(&f.type_name);
            // Remove generic parameters for comparison
            let field_type_base = field_type_last.split('<').next().unwrap_or(field_type_last);
            field_type_base == context_name
        })
    {
        return struct_value_type(analyzer, &normalize_type(&field.type_name));
    }

    // Try as direct type lookup
    if let Some(direct_lookup) = analyzer.get_type_info(context_name) {
        return Some(direct_lookup);
    }

    // The context name might be a variant name.
    // Check if current type is an enum with this variant.
    if let Some(variant) = info.find_variant(context_name)
        && variant.fields.len() == 1
    {
        // For tuple variants with one field, navigate to that field's type
        return analyzer.get_type_info(&variant.fields[0].type_name);
    }

    // If not found as a variant of the current type, try to find it in field types
    if let Some(fields) = info.fields() {
        for field in fields {
            if let Some(field_type_info) = value_type(analyzer, &field.type_name)
                && field_type_info.find_variant(context_name).is_some()
            {
                return Some(field_type_info);
            }
        }
    }

    // Last, since nothing the workspace defines may be shadowed by it: a
    // `Duration(..)` written behind an `Option` or inside a `Vec`.
    std_record_type(context_name)
}

/// Resolve the type of an unnamed struct value from the field it is the value
/// of, through whatever wrappers the field's type is declared with — see
/// [`value_type`].
fn step_by_field_name<'a>(
    analyzer: &'a RustAnalyzer,
    info: &TypeInfo,
    field_name: &str,
) -> Option<&'a TypeInfo> {
    let field = info.find_field_serialized(field_name)?;
    value_type_by(&field.type_name, |name| struct_value_type(analyzer, name))
}

/// The type whose fields a struct value written for a field declared as
/// `type_name` (already normalized, wrappers removed) is made of: a registered
/// type, or failing that one of the standard-library types `ron` reads as a
/// struct of their own — see [`std_record_type`].
fn struct_value_type<'a>(analyzer: &'a RustAnalyzer, type_name: &str) -> Option<&'a TypeInfo> {
    analyzer
        .get_type_info(type_name)
        .or_else(|| std_record_type(type_name))
}

/// The fields of a standard-library type that `ron` reads as a struct of its
/// own, `Duration` as `(secs: 0, nanos: 0)` and `SystemTime` as
/// `(secs_since_epoch: 0, nanos_since_epoch: 0)`, or `None` for any other
/// name.
///
/// Nothing in `std` is ever registered with the analyzer, so a value of either
/// had no type to resolve to. A cursor inside `timeout: (|)` was completed as
/// the field's value and offered the whole `(secs: 0, nanos: 0)` again — into
/// parentheses already written, making `((secs: 0, nanos: 0))` — and one inside
/// `timeout: (secs: 1, |)` was offered nothing, not even the `nanos` that is
/// still missing. These describe the two to the features that look a struct
/// value's fields up: completion and hover. Validating a value is left to
/// deserializing it into the real type, as before.
///
/// A type of the consumer's own that shares the name is still found first, by
/// [`struct_value_type`].
pub fn std_record_type(type_name: &str) -> Option<&'static TypeInfo> {
    use crate::rust_analyzer::{FieldInfo, TypeKind};
    use std::sync::LazyLock;

    fn record(name: &str, docs: &str, fields: [(&str, &str, &str); 2]) -> TypeInfo {
        TypeInfo {
            name: name.to_string(),
            docs: Some(docs.to_string()),
            kind: TypeKind::Struct(
                fields
                    .into_iter()
                    .map(|(name, type_name, docs)| FieldInfo {
                        name: name.to_string(),
                        type_name: type_name.to_string(),
                        docs: Some(docs.to_string()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    static DURATION: LazyLock<TypeInfo> = LazyLock::new(|| {
        record(
            "Duration",
            "`std::time::Duration`, as serde writes it.",
            [
                ("secs", "u64", "Whole seconds."),
                (
                    "nanos",
                    "u32",
                    "Nanoseconds past `secs`, below one billion.",
                ),
            ],
        )
    });
    static SYSTEM_TIME: LazyLock<TypeInfo> = LazyLock::new(|| {
        record(
            "SystemTime",
            "`std::time::SystemTime`, as serde writes it.",
            [
                (
                    "secs_since_epoch",
                    "u64",
                    "Whole seconds since the Unix epoch.",
                ),
                (
                    "nanos_since_epoch",
                    "u32",
                    "Nanoseconds past `secs_since_epoch`, below one billion.",
                ),
            ],
        )
    });

    match type_name {
        "Duration" => Some(&DURATION),
        "SystemTime" => Some(&SYSTEM_TIME),
        _ => None,
    }
}

/// Deepest a chain of wrappers is followed by [`value_type`]. No declared type
/// nests further than this by hand, and the bound keeps a malformed name from
/// costing more than a few lookups.
const MAX_WRAPPER_DEPTH: usize = 16;

/// The registered type a struct or enum value written for a field declared as
/// `type_name` belongs to, seeing through every wrapper on the way down — the
/// way diagnostics recurse into a field's value.
///
/// A value of `Option<Box<Inner>>` is written `Some((a: 1))`, an element of
/// `Vec<Backend>` is written `Local(root: "/")`, and a value of
/// `HashMap<String, Limits>` is written `{"a": (max: 1)}`: what sits inside is
/// a value of `Inner`, `Backend` and `Limits` respectively. Stripping a single
/// generic layer stopped at `Box<Inner>`, and a map's `String,Limits` named no
/// type at all, so hover, completion and go-to-definition found nothing there.
pub fn value_type<'a>(analyzer: &'a RustAnalyzer, type_name: &str) -> Option<&'a TypeInfo> {
    value_type_by(type_name, |name| analyzer.get_type_info(name))
}

/// [`value_type`], with the type a struct value's fields belong to looked up
/// by `lookup` rather than in the analyzer alone.
fn value_type_by<'a>(
    type_name: &str,
    lookup: impl Fn(&str) -> Option<&'a TypeInfo>,
) -> Option<&'a TypeInfo> {
    let mut current = normalize_type(type_name).into_owned();
    for _ in 0..MAX_WRAPPER_DEPTH {
        if let Some(info) = lookup(&current) {
            return Some(info);
        }
        let inner = single_value_wrapped_type(&current)
            .or_else(|| wrapped_type(&current, &ARRAY_WRAPPERS))
            .or_else(|| map_entry_types(&current).map(|(_, value)| value))?;
        current = inner.to_string();
    }
    None
}
