//! Navigation through nested RON type contexts.
use super::tree_sitter_parser::TypeContext;
use super::type_utils::{
    SEQUENCE_WRAPPERS, SET_WRAPPERS, TRANSPARENT_WRAPPERS, extract_inner_type, map_entry_types,
    normalize_type, short_name,
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
        return analyzer.get_type_info(&field.type_name);
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

    None
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
    value_type(analyzer, &field.type_name)
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
    let mut current = normalize_type(type_name).into_owned();
    for _ in 0..MAX_WRAPPER_DEPTH {
        if let Some(info) = analyzer.get_type_info(&current) {
            return Some(info);
        }
        let inner = std::iter::once("Option<")
            .chain(TRANSPARENT_WRAPPERS)
            .chain(SEQUENCE_WRAPPERS)
            .chain(SET_WRAPPERS)
            .find_map(|wrapper| extract_inner_type(&current, wrapper))
            .or_else(|| map_entry_types(&current).map(|(_, value)| value))?;
        current = inner.to_string();
    }
    None
}
