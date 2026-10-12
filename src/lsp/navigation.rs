//! Navigation through nested RON type contexts.
use super::tree_sitter_parser::TypeContext;
use super::type_utils::{short_name, strip_outer_generic};
use crate::rust_analyzer::{RustAnalyzer, TypeInfo};

/// Navigate through nested type contexts to find the innermost type.
/// The first context is the document's root value, which the caller has
/// already resolved into `start`; navigation begins from the second context.
pub fn navigate_type_contexts(
    analyzer: &RustAnalyzer,
    start: Option<TypeInfo>,
    contexts: &[TypeContext],
) -> Option<TypeInfo> {
    let mut current_type_info = start;

    for context in contexts.iter().skip(1) {
        // Once a step fails there is nothing left to navigate from, and `take`
        // has already left the answer as `None`.
        let Some(info) = current_type_info.take() else {
            break;
        };
        current_type_info = step_type_context(analyzer, &info, context);
    }

    current_type_info
}

/// Resolve one step of the nesting path: the type of the struct value that
/// `context` describes, nested directly inside `info`.
///
/// Callers that already know the enclosing struct's type — a walk down the
/// document, say — can step with this instead of navigating the whole path from
/// the root again.
pub fn step_type_context(
    analyzer: &RustAnalyzer,
    info: &TypeInfo,
    context: &TypeContext,
) -> Option<TypeInfo> {
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
fn step_by_type_name(
    analyzer: &RustAnalyzer,
    info: &TypeInfo,
    context_name: &str,
) -> Option<TypeInfo> {
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
        return analyzer.get_type_info(&field.type_name).cloned();
    }

    // Try as direct type lookup
    if let Some(direct_lookup) = analyzer.get_type_info(context_name) {
        return Some(direct_lookup.clone());
    }

    // The context name might be a variant name.
    // Check if current type is an enum with this variant.
    if let Some(variant) = info.find_variant(context_name)
        && variant.fields.len() == 1
    {
        // For tuple variants with one field, navigate to that field's type
        return analyzer
            .get_type_info(&variant.fields[0].type_name)
            .cloned();
    }

    // If not found as a variant of the current type, try to find it in field types
    if let Some(fields) = info.fields() {
        for field in fields {
            if let Some(field_type_info) = analyzer.get_type_info(&field.type_name)
                && field_type_info.find_variant(context_name).is_some()
            {
                return Some(field_type_info.clone());
            }
        }
    }

    None
}

/// Resolve the type of an unnamed struct value from the field it is the value
/// of. A single generic layer is stripped so that `Option<Server>` and
/// `Vec<Server>` resolve to `Server`, matching how diagnostics recurse into
/// field values.
fn step_by_field_name(
    analyzer: &RustAnalyzer,
    info: &TypeInfo,
    field_name: &str,
) -> Option<TypeInfo> {
    let field = info.find_field_serialized(field_name)?;
    analyzer
        .get_type_info(&field.type_name)
        .or_else(|| analyzer.get_type_info(&strip_outer_generic(&field.type_name)))
        .cloned()
}
