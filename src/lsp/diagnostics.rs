use super::type_utils::{
    closest_name, extract_inner_type, is_custom_type, is_primitive_type, missing_required_fields,
    normalize_type, short_name,
};
use crate::rust_analyzer::{FieldInfo, RustAnalyzer, TypeInfo, TypeKind};
use ron::Value;
use std::sync::Arc;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Tree;

/// Stable diagnostic codes. Clients and code actions can match on these
/// instead of parsing diagnostic messages.
pub mod codes {
    pub const SYNTAX_ERROR: &str = "syntax-error";
    pub const UNKNOWN_FIELD: &str = "unknown-field";
    pub const DUPLICATE_FIELD: &str = "duplicate-field";
    pub const MISSING_REQUIRED_FIELD: &str = "missing-required-field";
    pub const UNKNOWN_VARIANT: &str = "unknown-variant";
    pub const UNKNOWN_TYPE: &str = "unknown-type";
    pub const TYPE_MISMATCH: &str = "type-mismatch";
}

fn code(code: &str) -> Option<NumberOrString> {
    Some(NumberOrString::String(code.to_string()))
}

/// Build a `" (did you mean 'x'?)"` suffix when one of `candidates` is a close
/// match for the misspelled `target`, or an empty string otherwise. The suffix
/// is meant to be appended directly to a diagnostic message.
fn did_you_mean<'a>(target: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    match closest_name(target, candidates) {
        Some(best) => format!(" (did you mean '{}'?)", best),
        None => String::new(),
    }
}

/// What a `struct` node in the document is expected to contain.
///
/// RON spells a struct value and an enum's struct variant the same way, so both
/// are described by this and validated by [`validate_struct_node`]. Everything
/// that differs between the two lives here.
struct ExpectedStruct<'a> {
    /// The names serde accepts, as `(serialized_name, field)` pairs: `skip`
    /// excluded, `flatten` expanded.
    fields: Vec<(String, FieldInfo)>,
    /// A `flatten` target the analyzer can't resolve (e.g. a `HashMap`) makes
    /// serde accept arbitrary extra keys, so unknown fields aren't reported.
    allow_unknown_fields: bool,
    /// The container derives `Default`, so an absent field is not an error.
    has_default: bool,
    /// The enum variant this value spells out, when it is one. Only affects the
    /// wording of the diagnostics.
    variant: Option<&'a str>,
    /// True only for the document's root value, which is the only place the
    /// informational "field: Type" hints are emitted.
    at_root: bool,
}

impl ExpectedStruct<'_> {
    /// `" in variant 'V'"` when this value is an enum variant, else empty.
    fn in_variant(&self) -> String {
        match self.variant {
            Some(name) => format!(" in variant '{}'", name),
            None => String::new(),
        }
    }

    /// The field serde would fill from a RON field named `field_name`.
    ///
    /// `name` is the serialized name as the *containing* type spells it, which
    /// for a flattened field is not what the field's own container would
    /// produce, so it is compared separately from `accepts_name`.
    fn matching_field(&self, field_name: &str) -> Option<&FieldInfo> {
        self.fields
            .iter()
            .find(|(name, f)| *name == field_name || f.accepts_name(field_name, None))
            .map(|(_, f)| f)
    }
}

/// Every diagnostic for a RON document whose root value is declared to be
/// `type_info`: a syntax error if it doesn't parse, and otherwise whatever a
/// walk of the parse tree turns up.
pub async fn validate_ron_with_analyzer(
    content: &str,
    tree: Option<&Tree>,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Reuse the document's tree if provided; otherwise parse the content here.
    let local_tree;
    let tree = match tree {
        Some(t) => t,
        None => match super::ts_utils::parse(content) {
            Some(t) => {
                local_tree = t;
                &local_tree
            }
            None => return diagnostics,
        },
    };

    // Parse RON once and check for syntax errors from the result
    // Try to parse the RON content
    let parsed_value = ron::from_str::<Value>(content);

    // If parsing failed, return syntax error
    if let Err(e) = &parsed_value {
        let error_msg = e.to_string();
        let (line, col) = parse_error_position(&error_msg, content);
        let simplified_msg = simplify_ron_error(&error_msg);

        diagnostics.push(Diagnostic {
            range: Range::new(Position::new(line, col), Position::new(line, col + 1)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: simplified_msg,
            code: code(codes::SYNTAX_ERROR),
            ..Default::default()
        });
        return diagnostics;
    }

    // Everything else is a single walk of the parse tree from the root value
    // down, validating each value against the type it is declared as. Each
    // value is visited once, so nothing is reported twice.
    if let Some(main_value) = super::ts_utils::find_main_value(tree) {
        diagnostics.extend(
            validate_node_with_type_info(&main_value, content, type_info, &analyzer, true).await,
        );
    }

    diagnostics
}

/// Validate a `struct` node's named fields against what it is expected to
/// contain: duplicate fields, unknown fields, each field's value, and missing
/// required fields.
///
/// Every named-field value in the document goes through here — a struct type's
/// value and an enum's struct variant alike — so the field-matching,
/// type-checking and missing-field rules exist in exactly one place.
async fn validate_struct_node(
    node: &tree_sitter::Node<'_>,
    content: &str,
    expected: &ExpectedStruct<'_>,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    use super::ts_utils;
    let mut diagnostics = Vec::new();

    if node.kind() != "struct" {
        return diagnostics;
    }
    // Tuple/newtype structs and variants have positional fields ("0", "1", ...)
    // — named-field validation doesn't apply to them.
    if expected.fields.iter().all(|(_, f)| f.is_positional()) {
        return diagnostics;
    }

    let mut present_fields: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for field_node in ts_utils::struct_fields(node) {
        let Some(field_name) = ts_utils::field_name(&field_node, content) else {
            continue;
        };
        let name_range = ts_utils::node_to_lsp_range(&field_node.child(0).unwrap_or(field_node));

        // Duplicate field — report at the second occurrence
        if !present_fields.insert(field_name) {
            diagnostics.push(Diagnostic {
                range: name_range,
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("Duplicate field '{}'", field_name),
                code: code(codes::DUPLICATE_FIELD),
                ..Default::default()
            });
            continue;
        }

        let Some(field_info) = expected.matching_field(field_name) else {
            if !expected.allow_unknown_fields {
                diagnostics.push(Diagnostic {
                    range: name_range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: format!(
                        "Unknown field '{}'{}{}",
                        field_name,
                        expected.in_variant(),
                        did_you_mean(
                            field_name,
                            expected.fields.iter().map(|(name, _)| name.as_str())
                        )
                    ),
                    code: code(codes::UNKNOWN_FIELD),
                    ..Default::default()
                });
            }
            continue;
        };

        let Some(value_node) = ts_utils::field_value(&field_node) else {
            continue;
        };

        // Hint the field's declared type, but only when the RON doesn't name it already
        if expected.at_root
            && ts_utils::struct_name(&value_node, content).is_none()
            && value_node.kind() != "identifier"
        {
            diagnostics.push(Diagnostic {
                range: name_range,
                severity: Some(DiagnosticSeverity::INFORMATION),
                message: format!("{}: {}", field_info.name, field_info.type_name),
                ..Default::default()
            });
        }

        diagnostics.extend(
            Box::pin(validate_typed_value(
                &value_node,
                content,
                &field_info.type_name,
                analyzer,
            ))
            .await,
        );
    }

    // Missing required fields: compare expected fields against those we saw above.
    if !expected.has_default {
        let missing =
            missing_required_fields(&expected.fields, |name| present_fields.contains(name));
        if !missing.is_empty() {
            let missing_names: Vec<&str> = missing.iter().map(|(name, _)| name.as_str()).collect();
            diagnostics.push(Diagnostic {
                range: struct_name_range(node),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!(
                    "Required fields{}: {}",
                    expected.in_variant(),
                    missing_names.join(", ")
                ),
                code: code(codes::MISSING_REQUIRED_FIELD),
                ..Default::default()
            });
        }
    }

    diagnostics
}

/// Validate one value against the type it is declared as.
///
/// Recursion into the value comes first: a custom struct/enum, or a generic
/// wrapper around one, is checked field by field. Only when that finds nothing
/// — including when the type isn't one we recurse into at all — is the value's
/// own shape checked against the declared primitive or std type.
///
/// Every typed value in the document is checked here, whether it is a struct
/// field's value or a tuple variant's payload.
async fn validate_typed_value(
    value_node: &tree_sitter::Node<'_>,
    content: &str,
    declared_type: &str,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    // Deep validation: Vec<T>, Option<T>, plain custom structs/enums.
    // validate_nested_value handles all generic-wrapper cases uniformly, so
    // there are no per-container special cases here.
    let deep = Box::pin(validate_nested_value(
        value_node,
        content,
        declared_type,
        analyzer,
    ))
    .await;
    if !deep.is_empty() {
        return deep;
    }

    // Primitive / surface-level type check. The value's own source text carries
    // everything the check needs, so it runs at every nesting depth.
    //
    // Only non-custom types (primitives and std generic wrappers) are checked
    // against the typed RON value; custom structs and enums are judged from the
    // source text alone. Parsing lazily keeps a nested struct's whole subtree
    // from being re-parsed once per level.
    let value_text = super::ts_utils::node_text(value_node, content);
    let parsed_value = match is_custom_type(declared_type) {
        true => None,
        false => value_text.and_then(|text| ron::from_str::<Value>(text).ok()),
    };

    let Some(error_msg) = check_type_mismatch_with_enum_validation(
        parsed_value.as_ref(),
        declared_type,
        value_text,
        analyzer,
    )
    .await
    else {
        return Vec::new();
    };

    vec![Diagnostic {
        range: first_line_range(value_node, content),
        severity: Some(DiagnosticSeverity::ERROR),
        message: format!("Type mismatch: {}", error_msg),
        code: code(codes::TYPE_MISMATCH),
        ..Default::default()
    }]
}

/// The node's range, clipped to its first line. A multi-line node would
/// otherwise produce an inverted column range.
fn first_line_range(node: &tree_sitter::Node, content: &str) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    let end_col = if end.row > start.row {
        content.lines().nth(start.row).unwrap_or("").len() as u32
    } else {
        end.column as u32
    };
    Range::new(
        Position::new(start.row as u32, start.column as u32),
        Position::new(start.row as u32, end_col),
    )
}

/// The range to report a whole-struct diagnostic at: the struct's name, or a
/// zero-width range at its start when it is written with unnamed syntax.
fn struct_name_range(node: &tree_sitter::Node) -> Range {
    match node.child(0) {
        Some(name) if name.kind() == "identifier" => super::ts_utils::node_to_lsp_range(&name),
        _ => {
            let pos = node.start_position();
            let pos = Position::new(pos.row as u32, pos.column as u32);
            Range::new(pos, pos)
        }
    }
}

/// The single value inside an explicit `Some(...)`, which is how RON spells an
/// inhabited `Option`. `None` for every other shape, including the bare `value`
/// form that RON also accepts and a struct that happens to be named `Some`.
fn unwrap_some<'a>(node: &tree_sitter::Node<'a>, content: &str) -> Option<tree_sitter::Node<'a>> {
    use super::ts_utils;

    if ts_utils::struct_name(node, content) != Some("Some") {
        return None;
    }
    match ts_utils::struct_values(node, content).as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// Recurse into a value whose declared type is a custom struct/enum, or a
/// generic wrapper around one.
///
/// This is the single place that decides how to unwrap generic wrappers
/// (Vec<T>, Option<T>, …). Nothing is reported for a type we don't recurse
/// into — the caller ([`validate_typed_value`]) checks those against the
/// value's own shape instead.
async fn validate_nested_value<'a>(
    value_node: &tree_sitter::Node<'a>,
    content: &str,
    field_type: &str,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let field_type_normalized = normalize_type(field_type);

    // Helper closure to emit an "Unknown type" diagnostic at the value node
    let unknown_type_diag = |inner: &str| Diagnostic {
        range: super::ts_utils::node_to_lsp_range(value_node),
        severity: Some(DiagnosticSeverity::ERROR),
        message: format!("Unknown type '{}'", inner),
        code: code(codes::UNKNOWN_TYPE),
        ..Default::default()
    };

    if let Some(inner_type) = extract_inner_type(&field_type_normalized, "Vec<") {
        // Vec<CustomType> — validate every array element against the inner type
        if is_custom_type(inner_type) {
            if let Some(inner_type_info) = analyzer.get_type_info(inner_type).cloned() {
                if value_node.kind() == "array" {
                    let mut cursor = value_node.walk();
                    for elem_node in value_node.children(&mut cursor) {
                        if elem_node.kind() != "["
                            && elem_node.kind() != "]"
                            && elem_node.kind() != ","
                        {
                            let elem_diags = Box::pin(validate_node_with_type_info(
                                &elem_node,
                                content,
                                &inner_type_info,
                                analyzer,
                                false,
                            ))
                            .await;
                            diagnostics.extend(elem_diags);
                        }
                    }
                }
            } else {
                diagnostics.push(unknown_type_diag(inner_type));
            }
        }
    } else if let Some(inner_type) = ["Option<", "Box<", "Arc<", "Rc<"]
        .iter()
        .find_map(|w| extract_inner_type(&field_type_normalized, w))
    {
        // Single-element wrapper — check the inner type is known
        if is_custom_type(inner_type) {
            if let Some(inner_type_info) = analyzer.get_type_info(inner_type).cloned() {
                // An `Option` field is usually written `Some(value)`. Validate
                // the wrapped value rather than the `Some` wrapper, or the
                // inner type's own fields go unchecked.
                let inner_node = unwrap_some(value_node, content).unwrap_or(*value_node);
                let nested_diags = Box::pin(validate_node_with_type_info(
                    &inner_node,
                    content,
                    &inner_type_info,
                    analyzer,
                    false,
                ))
                .await;
                diagnostics.extend(nested_diags);
            } else {
                diagnostics.push(unknown_type_diag(inner_type));
            }
        }
    } else if is_custom_type(&field_type_normalized) {
        // Plain custom struct/enum — validate the node directly
        if let Some(nested_type_info) = analyzer.get_type_info(&field_type_normalized).cloned() {
            let nested_diags = Box::pin(validate_node_with_type_info(
                value_node,
                content,
                &nested_type_info,
                analyzer,
                false,
            ))
            .await;
            diagnostics.extend(nested_diags);
        } else {
            // Unknown custom type — report an error at the value node
            diagnostics.push(Diagnostic {
                range: super::ts_utils::node_to_lsp_range(value_node),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("Unknown type '{}'", field_type_normalized),
                code: code(codes::UNKNOWN_TYPE),
                ..Default::default()
            });
        }
    }

    diagnostics
}

/// The "unknown variant" diagnostic for a name the enum `type_info` does not
/// accept, reported at `range` with a "did you mean" suggestion drawn from the
/// enum's serialized variant names.
fn unknown_variant(type_info: &TypeInfo, variant_name: &str, range: Range) -> Diagnostic {
    let rename_all = type_info.rename_all.as_deref();
    let known: Vec<String> = match &type_info.kind {
        TypeKind::Enum(variants) => variants
            .iter()
            .map(|v| v.serialized_name(rename_all))
            .collect(),
        TypeKind::Struct(_) => Vec::new(),
    };
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        message: format!(
            "Unknown variant '{}' for enum '{}'{}",
            variant_name,
            type_info.name,
            did_you_mean(variant_name, known.iter().map(|s| s.as_str()))
        ),
        code: code(codes::UNKNOWN_VARIANT),
        ..Default::default()
    }
}

/// Validate the value `node` against the type it is declared as, recursing into
/// the tree. The document's root value and every nested value go through here,
/// so a type is validated the same way wherever it appears.
///
/// `at_root` is true only for the document's root value, and gates the
/// informational type hints, which are deliberately top-level only.
async fn validate_node_with_type_info<'a>(
    node: &tree_sitter::Node<'a>,
    content: &str,
    type_info: &TypeInfo,
    analyzer: &Arc<RustAnalyzer>,
    at_root: bool,
) -> Vec<Diagnostic> {
    match &type_info.kind {
        TypeKind::Struct(_) => {
            let expected = ExpectedStruct {
                // The names serde accepts: serialized names, `skip` excluded,
                // `flatten` expanded when the analyzer can resolve the target.
                fields: type_info.effective_fields(analyzer),
                allow_unknown_fields: type_info.has_unresolved_flatten(analyzer),
                has_default: type_info.has_default,
                variant: None,
                at_root,
            };
            Box::pin(validate_struct_node(node, content, &expected, analyzer)).await
        }
        TypeKind::Enum(_) => Box::pin(validate_enum_node(node, content, type_info, analyzer)).await,
    }
}

/// Validate a value written where an enum is expected: the variant it names
/// must exist, and whatever data it carries must match that variant's fields.
///
/// RON spells a unit variant as a bare name (`Prod`), a tuple variant as
/// `Prod(30)` and a struct variant as `Prod(retries: 3)` — the last two both
/// parsing as a `struct` node whose leading identifier is the variant name.
async fn validate_enum_node(
    node: &tree_sitter::Node<'_>,
    content: &str,
    type_info: &TypeInfo,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    use super::ts_utils;

    // A value that names no variant — an unnamed struct, or a primitive written
    // where the enum belongs — is left to the caller's surface type check.
    let Some(named) = ts_utils::extract_enum_variant(node, content) else {
        return Vec::new();
    };

    let Some(variant) = type_info.find_variant(&named.name) else {
        return vec![unknown_variant(type_info, &named.name, named.range)];
    };

    // A bare name carries no data, so the variant is already fully checked.
    if node.kind() != "struct" {
        return Vec::new();
    }

    let payload = ts_utils::struct_values(node, content);
    let named_fields = ts_utils::struct_fields(node);

    if variant.fields.is_empty() {
        if payload.is_empty() && named_fields.is_empty() {
            return Vec::new();
        }
        return vec![Diagnostic {
            range: named.range,
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!(
                "Variant '{}' is a unit variant and cannot have data",
                named.name
            ),
            code: code(codes::TYPE_MISMATCH),
            ..Default::default()
        }];
    }

    let expected = variant.effective_fields();

    // A tuple variant's fields are named by position ("0", "1", ...), so they
    // are matched to the payload values in order rather than by name.
    if expected.iter().all(|(_, f)| f.is_positional()) {
        let mut diagnostics = Vec::new();
        for (value_node, (_, field)) in payload.iter().zip(&expected) {
            diagnostics.extend(
                Box::pin(validate_typed_value(
                    value_node,
                    content,
                    &field.type_name,
                    analyzer,
                ))
                .await,
            );
        }
        return diagnostics;
    }

    let expected = ExpectedStruct {
        fields: expected,
        allow_unknown_fields: false,
        has_default: type_info.has_default,
        variant: Some(&named.name),
        at_root: false,
    };
    Box::pin(validate_struct_node(node, content, &expected, analyzer)).await
}

/// Type checking with enum variant validation (async, uses analyzer)
async fn check_type_mismatch_with_enum_validation(
    value: Option<&Value>,
    expected_type: &str,
    value_text: Option<&str>,
    analyzer: &Arc<RustAnalyzer>,
) -> Option<String> {
    // If the expected type is custom (not primitive), we need special handling
    if is_custom_type(expected_type)
        && let Some(field_value_text) = value_text
    {
        let trimmed = field_value_text.trim();

        // Extract the type/variant name from the RON text
        let type_in_ron = trimmed.split('(').next().unwrap_or(trimmed).trim();

        // Check if the expected type is a known enum first — if so, validate the variant name
        // regardless of case (serde rename_all can produce lowercase/snake_case variant names)
        if !type_in_ron.is_empty() && !is_primitive_type(type_in_ron) {
            if let Some(type_info) = analyzer.get_type_info(expected_type).cloned() {
                if matches!(type_info.kind, TypeKind::Enum(_)) {
                    return match type_info.find_variant(type_in_ron) {
                        Some(_) => None,
                        None => Some(format!(
                            "unknown variant '{}' for enum {}",
                            type_in_ron, expected_type
                        )),
                    };
                } else if type_in_ron
                    .chars()
                    .next()
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false)
                {
                    // Expected type is a struct - the name in RON should match or be unnamed
                    let expected_simple = short_name(expected_type);
                    if type_in_ron != expected_simple {
                        // Different type name - check if it's a known type or unknown
                        if analyzer.get_type_info(type_in_ron).is_none() {
                            return Some(format!("unknown type '{}'", type_in_ron));
                        }
                        // Known but different type - fall through to basic check
                    } else {
                        // Type names match - no type mismatch
                        return None;
                    }
                }
            } else {
                // Expected type is not registered in analyzer
                // If the RON type matches the expected type name, that's fine
                let expected_simple = short_name(expected_type);
                if type_in_ron == expected_simple {
                    return None;
                }
                // Different type - check if it's known
                if trimmed.contains('(') && analyzer.get_type_info(type_in_ron).is_none() {
                    return Some(format!("unknown type '{}'", type_in_ron));
                }
                // RON type is known or no parens - fall through
            }
        }
    }

    // Do basic type checking for remaining cases
    check_type_mismatch_deep(value, expected_type, value_text)
}

/// Deep type checking that also validates custom types by looking at raw text.
///
/// `value` is the field's value parsed as RON, needed only to check primitives
/// and standard library generic types; callers may leave it out for custom
/// types, which are judged from `value_text` alone.
fn check_type_mismatch_deep(
    value: Option<&Value>,
    expected_type: &str,
    value_text: Option<&str>,
) -> Option<String> {
    let clean_type = normalize_type(expected_type);

    // First check if it's a primitive type or standard library generic type
    if !is_custom_type(expected_type) {
        return check_type_mismatch(value?, expected_type);
    }

    // For custom types (structs/enums), we need to check the raw text
    // because Value loses the type information
    let trimmed = value_text?.trim();

    // Check if expected type is a custom struct/enum (starts with uppercase)
    if clean_type
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
    {
        // Expected a struct/enum
        // The value should start with TypeName( or be a variant name

        // Check if it looks like a struct instantiation TypeName(...) or unnamed (...)
        if trimmed.contains('(') {
            // Extract the type name before the paren
            let type_in_text = trimmed.split('(').next().unwrap_or("").trim();
            let expected_simple = short_name(&clean_type);

            // Allow unnamed struct syntax - empty type_in_text means type is inferred
            if !type_in_text.is_empty() && type_in_text != expected_simple {
                return Some(format!("expected {}, got {}", expected_type, type_in_text));
            }
        } else {
            // It's a bare value - could be an enum variant or a primitive
            // If it's a number, string literal, or bool, that's wrong
            if trimmed.parse::<i64>().is_ok() {
                return Some(format!("expected {}, got integer", expected_type));
            }
            if trimmed.parse::<f64>().is_ok() {
                return Some(format!("expected {}, got float", expected_type));
            }
            if trimmed.starts_with('"') {
                return Some(format!("expected {}, got string", expected_type));
            }
            if trimmed == "true" || trimmed == "false" {
                return Some(format!("expected {}, got bool", expected_type));
            }
            // Otherwise assume it's an enum variant (we'd need more context to validate)
        }
    }

    None
}

/// Check if a RON value matches the expected Rust type
fn check_type_mismatch(value: &Value, expected_type: &str) -> Option<String> {
    let clean_type = normalize_type(expected_type);
    // Use clean_type for error messages to avoid extra spaces
    let display_type = &clean_type;

    // Handle Option types - None is always valid for Option<T>
    if clean_type.starts_with("Option<") {
        if matches!(value, Value::Option(None)) {
            return None;
        }
        // Unwrap Some(value) to its inner value; a bare value is fine too (it
        // will be wrapped). Either way, validate against Option's inner type.
        if let Some(inner_type) = extract_inner_type(&clean_type, "Option<") {
            let value = match value {
                Value::Option(Some(inner)) => inner,
                other => other,
            };
            return check_type_mismatch(value, inner_type);
        }
    }

    // Handle Box, Rc, Arc - they serialize as just the inner value
    if let Some(inner_type) = ["Box<", "Rc<", "Arc<"]
        .iter()
        .find_map(|w| extract_inner_type(&clean_type, w))
    {
        return check_type_mismatch(value, inner_type);
    }

    match value {
        Value::Bool(_) if clean_type != "bool" => {
            return Some(format!("expected {}, got bool", display_type));
        }
        Value::Number(n) => {
            // Check for integer types
            let integer_types = [
                "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128",
                "usize",
            ];
            let float_types = ["f32", "f64"];

            let is_integer_type = integer_types.contains(&clean_type.as_str());
            let is_float_type = float_types.contains(&clean_type.as_str());

            // Check if it's a float or integer based on the Number variant
            let is_float_value = matches!(n, ron::Number::F32(_) | ron::Number::F64(_));
            let is_int_value = !is_float_value;

            if is_float_value && is_integer_type {
                return Some(format!("expected {}, got float", display_type));
            }
            if is_int_value && is_float_type {
                return Some(format!("expected {}, got integer", display_type));
            }
            // If not a numeric type at all, it's an error
            if !is_integer_type && !is_float_type {
                return Some(format!("expected {}, got number", display_type));
            }
        }
        Value::String(_) => {
            // String is valid for String types
            if clean_type == "String" || clean_type == "&str" || clean_type == "str" {
                return None;
            }
            // Otherwise it's an error
            return Some(format!("expected {}, got string", display_type));
        }
        Value::Seq(seq) => {
            if clean_type.starts_with("Vec<") {
                // Check element types if possible
                if let Some(elem_type) = extract_inner_type(&clean_type, "Vec<") {
                    for elem in seq {
                        if let Some(err) = check_type_mismatch(elem, elem_type) {
                            return Some(format!("in Vec: {}", err));
                        }
                    }
                }
            } else if clean_type.contains("HashSet<") || clean_type.contains("BTreeSet<") {
                // Sets are serialized as arrays in RON
                return None;
            } else if clean_type.starts_with("Result<") {
                // Result variants like Ok(...) and Err(...) are serialized as tuples/arrays
                return None;
            } else if clean_type.starts_with("[") {
                // Array type
                return None; // Arrays are similar to Vec, accept them
            } else {
                return Some(format!("expected {}, got array", display_type));
            }
        }
        Value::Map(_) => {
            // Maps could be structs or actual maps
            if clean_type.contains("HashMap<") || clean_type.contains("BTreeMap<") {
                // It's a map type, which is fine
                return None;
            }
            // Check if it's a custom struct (starts with uppercase)
            if clean_type
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                // Could be a struct, allow it
                return None;
            }
            return Some(format!("expected {}, got map/struct", display_type));
        }
        Value::Option(Some(_)) if !clean_type.starts_with("Option<") => {
            return Some(format!("expected {}, got Some(...)", display_type));
        }
        Value::Option(None) if !clean_type.starts_with("Option<") => {
            return Some(format!("expected {}, got None", display_type));
        }
        Value::Unit if clean_type != "()" && clean_type != "unit" => {
            return Some(format!("expected {}, got ()", display_type));
        }
        _ => {}
    }

    None
}

/// Parse error position from RON error message
fn parse_error_position(error_msg: &str, content: &str) -> (u32, u32) {
    // RON error messages often contain position info like "1:5" or "line 1 column 5"

    // Try to find "line X column Y" pattern
    if let Some(line_start) = error_msg.find("line ") {
        let rest = &error_msg[line_start + 5..];
        if let Some(line_end) = rest.find(|c: char| !c.is_numeric())
            && let Ok(line) = rest[..line_end].parse::<u32>()
            && let Some(col_start) = rest.find("column ")
        {
            let col_rest = &rest[col_start + 7..];
            if let Some(col_end) = col_rest.find(|c: char| !c.is_numeric())
                && let Ok(col) = col_rest[..col_end].parse::<u32>()
            {
                // RON reports 1-indexed, LSP expects 0-indexed
                return (line.saturating_sub(1), col.saturating_sub(1));
            }
        }
    }

    // Try to find "X:Y" pattern (common in parsers)
    if let Some(colon_pos) = error_msg.find(':') {
        let before = &error_msg[..colon_pos];
        // Find the last number before the colon
        if let Some(line_start) = before.rfind(|c: char| !c.is_numeric()) {
            let line_str = &before[line_start + 1..];
            if let Ok(line) = line_str.parse::<u32>() {
                let after = &error_msg[colon_pos + 1..];
                if let Some(col_end) = after.find(|c: char| !c.is_numeric())
                    && let Ok(col) = after[..col_end].parse::<u32>()
                {
                    return (line.saturating_sub(1), col.saturating_sub(1));
                }
            }
        }
    }

    // If we can't parse position, try to find likely error location by looking for common issues
    let lines: Vec<&str> = content.lines().collect();

    // Check for missing commas between fields
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // If a line ends with a value (not comma, not open brace) and next line starts with a field
        if !trimmed.is_empty()
            && !trimmed.ends_with(',')
            && !trimmed.ends_with('(')
            && !trimmed.ends_with('{')
            && !trimmed.ends_with('[')
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && idx + 1 < lines.len()
        {
            let next_line = lines[idx + 1].trim();
            // Next line looks like a field (word followed by colon)
            if next_line.contains(':') && !next_line.starts_with("//") {
                // Likely missing comma
                return (idx as u32, line.len().saturating_sub(1) as u32);
            }
        }
    }

    // Default to start of file
    (0, 0)
}

/// Simplify RON error messages to be more user-friendly
fn simplify_ron_error(error_msg: &str) -> String {
    // Extract the core error without all the implementation details
    if error_msg.contains("expected") {
        if error_msg.contains("`,`") || error_msg.contains("comma") {
            return "Expected comma between fields".to_string();
        }
        if error_msg.contains("`:`") || error_msg.contains("colon") {
            return "Expected colon after field name".to_string();
        }
        if error_msg.contains("`)`") {
            return "Expected closing parenthesis".to_string();
        }
        if error_msg.contains("`}`") {
            return "Expected closing brace".to_string();
        }
        if error_msg.contains("`]`") {
            return "Expected closing bracket".to_string();
        }
    }

    if error_msg.contains("unexpected") {
        return format!(
            "Syntax error: {}",
            error_msg
                .split("unexpected")
                .nth(1)
                .unwrap_or(error_msg)
                .trim()
        );
    }

    // Return simplified version
    format!("RON syntax error: {}", error_msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_analyzer::{EnumVariant, FieldInfo};

    /// The fields of a struct variant used deep inside a document are checked
    /// against the variant's definition, which means resolving the type that
    /// owns the variant's field. That has to work however the enclosing values
    /// are spelled: on one line or several, and with or without their struct
    /// names (RON lets those be omitted).
    #[tokio::test]
    async fn test_struct_variant_fields_in_nested_value() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "PostType".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "Detailed".to_string(),
                fields: vec![FieldInfo {
                    name: "length".to_string(),
                    type_name: "u32".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "Server".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "post_type".to_string(),
                type_name: "PostType".to_string(),
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "server".to_string(),
                type_name: "Server".to_string(),
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        });
        let analyzer = Arc::new(analyzer);
        let config = analyzer.get_type_info("Config").unwrap().clone();

        for (root, nested) in [
            ("Config", "Server"),
            ("Config", ""),
            ("", "Server"),
            ("", ""),
        ] {
            let label = format!("root {:?}, nested {:?}", root, nested);

            // The variant's fields spread over several lines.
            let multiline = format!(
                "{root}(\n    server: {nested}(\n        post_type: Detailed(\n            length: 1,\n            bogus: 2,\n        ),\n    ),\n)"
            );
            // ...and all on one line, where column 0 of the field's line lands
            // in the containing value rather than in the variant itself.
            let single_line = format!(
                "{root}(\n    server: {nested}(\n        post_type: Detailed(length: 1, bogus: 2),\n    ),\n)"
            );

            for (shape, content) in [("multi-line", multiline), ("single-line", single_line)] {
                let diagnostics =
                    validate_ron_with_analyzer(&content, None, &config, analyzer.clone()).await;
                assert!(
                    diagnostics.iter().any(|d| {
                        d.severity == Some(DiagnosticSeverity::ERROR)
                            && d.message
                                .contains("Unknown field 'bogus' in variant 'Detailed'")
                    }),
                    "a field the variant doesn't have should be reported ({shape}, {label}). Got: {diagnostics:?}"
                );
            }

            // The same documents without the stray field must be clean.
            let good = format!(
                "{root}(\n    server: {nested}(\n        post_type: Detailed(\n            length: 1,\n        ),\n    ),\n)"
            );
            let diagnostics =
                validate_ron_with_analyzer(&good, None, &config, analyzer.clone()).await;
            assert!(
                !diagnostics
                    .iter()
                    .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
                "a well-formed variant should not error ({label}). Got: {diagnostics:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_enum_variant_validation() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "PostType".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Short".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                    ..Default::default()
                },
                EnumVariant {
                    name: "Long".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // Valid enum variant
        let content = "Long";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Long should be valid. Got errors: {:?}",
            diagnostics
        );

        // Another valid variant
        let content = "Short";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Short should be valid. Got errors: {:?}",
            diagnostics
        );

        // Invalid enum variant
        let content = "Medium";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert_eq!(diagnostics.len(), 1, "Medium should be invalid");
        assert!(
            diagnostics[0].message.contains("Unknown variant 'Medium'"),
            "Expected unknown variant error, got: {}",
            diagnostics[0].message
        );

        // Invalid enum variant (typo)
        let content = "Longs";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert_eq!(diagnostics.len(), 1, "Longs should be invalid");
        assert!(
            diagnostics[0].message.contains("Unknown variant 'Longs'"),
            "Expected unknown variant error, got: {}",
            diagnostics[0].message
        );
    }

    #[tokio::test]
    async fn test_struct_with_enum_field() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "title".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "post_type".to_string(),
                    type_name: "PostType".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // PostType is not registered with the analyzer, so it should produce an error
        let content = r#"Post(
            id: 1,
            title: "Test",
            post_type: Long,
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)
                    && d.message.contains("PostType")),
            "Should error on unknown type 'PostType'. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_nested_enum_field_variant_validation() {
        // A registered enum used as the value of a nested struct field must be
        // validated: an invalid variant should error, a valid one should not.
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "ServerMode".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Development".to_string(),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Staging".to_string(),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Production".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "Server".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "mode".to_string(),
                type_name: "ServerMode".to_string(),
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        });
        let analyzer = Arc::new(analyzer);

        let config = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "server".to_string(),
                type_name: "Server".to_string(),
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        };

        // A nested variant typo must be flagged with a "did you mean" suggestion.
        let bad = "Config(\n    server: Server(\n        mode: Productio,\n    ),\n)";
        let diagnostics = validate_ron_with_analyzer(bad, None, &config, analyzer.clone()).await;
        let variant_err = diagnostics.iter().find(|d| {
            d.severity == Some(DiagnosticSeverity::ERROR)
                && d.message.contains("Unknown variant 'Productio'")
        });
        assert!(
            variant_err.is_some(),
            "Invalid nested enum variant should error. Got: {:?}",
            diagnostics
        );
        assert!(
            variant_err
                .unwrap()
                .message
                .contains("did you mean 'Production'"),
            "Error should suggest the closest variant. Got: {}",
            variant_err.unwrap().message
        );

        // A completely different word must still error, just without a suggestion.
        let bad = "Config(\n    server: Server(\n        mode: Prod,\n    ),\n)";
        let diagnostics = validate_ron_with_analyzer(bad, None, &config, analyzer.clone()).await;
        assert!(
            diagnostics.iter().any(|d| {
                d.severity == Some(DiagnosticSeverity::ERROR)
                    && d.message.contains("Unknown variant 'Prod'")
            }),
            "Invalid nested enum variant should error. Got: {:?}",
            diagnostics
        );

        // Valid nested variant must not produce any error.
        let good = "Config(\n    server: Server(\n        mode: Production,\n    ),\n)";
        let diagnostics = validate_ron_with_analyzer(good, None, &config, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Valid nested enum variant should not error. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_struct_field_expects_struct_not_primitive() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "author".to_string(),
                    type_name: "User".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // WRONG: author should be User(...), not just 1
        let content = r#"Post(
            id: 101,
            author: 1,
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics.is_empty(),
            "Should error on primitive when expecting struct"
        );
        assert!(
            diagnostics.iter().any(
                |d| d.severity == Some(DiagnosticSeverity::ERROR) && d.message.contains("User")
            ),
            "Should complain about User type. Got: {:?}",
            diagnostics
        );

        // User is not registered with the analyzer, so it should produce an error
        let content = r#"Post(
            id: 101,
            author: User(id: 1, name: "John"),
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;
        assert!(
            diagnostics.iter().any(
                |d| d.severity == Some(DiagnosticSeverity::ERROR) && d.message.contains("User")
            ),
            "Should error on unknown type 'User'. Got: {:?}",
            diagnostics
        );

        // When User IS registered, the same content should produce no errors
        let mut raw_analyzer = RustAnalyzer::new();
        raw_analyzer.add_type(TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        });
        let analyzer = Arc::new(raw_analyzer);
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        assert_eq!(
            errors.len(),
            0,
            "Should have no errors when User is registered. Got: {:?}",
            errors
        );
    }

    #[tokio::test]
    async fn test_serde_alias_field_accepted() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "port".to_string(),
                type_name: "u16".to_string(),
                aliases: vec!["old_port".to_string()],
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        };

        // The alias is accepted — no "Unknown field" diagnostic
        let content = r#"(old_port: 8080)"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field")),
            "Aliased field name should be accepted. Got: {:?}",
            diagnostics
        );

        // The canonical name still works
        let content = r#"(port: 8080)"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field")),
            "Canonical field name should be accepted. Got: {:?}",
            diagnostics
        );

        // A genuinely unknown field is still reported
        let content = r#"(bogus: 8080)"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field 'bogus'")),
            "Unknown field should still be reported. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_type_mismatch_primitives() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // Type mismatch - string for number
        let content = r#"User(
            id: "not a number",
            name: "John",
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;
        assert!(!diagnostics.is_empty(), "Should error on type mismatch");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Type mismatch")),
            "Should have type mismatch error. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_unnamed_struct_syntax() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // Unnamed struct syntax should be valid
        let content = r#"(
            id: 1,
            name: "John",
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        assert_eq!(
            errors.len(),
            0,
            "Unnamed struct syntax should be valid. Got errors: {:?}",
            errors
        );
    }

    #[tokio::test]
    async fn test_enum_with_tuple_variant() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Value".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Int".to_string(),
                    fields: vec![FieldInfo {
                        name: "0".to_string(),
                        type_name: "i32".to_string(),
                        docs: None,
                        line: None,
                        column: None,
                        has_default: false,
                        ..Default::default()
                    }],
                    docs: None,
                    line: None,
                    column: None,
                    ..Default::default()
                },
                EnumVariant {
                    name: "Str".to_string(),
                    fields: vec![FieldInfo {
                        name: "0".to_string(),
                        type_name: "String".to_string(),
                        docs: None,
                        line: None,
                        column: None,
                        has_default: false,
                        ..Default::default()
                    }],
                    docs: None,
                    line: None,
                    column: None,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // Valid tuple variant
        let content = "Int(42)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Tuple variant should be valid. Got errors: {:?}",
            diagnostics
        );

        // Another valid variant
        let content = r#"Str("hello")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Tuple variant with string should be valid. Got errors: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_unit_variant_with_data_error() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Status".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Active".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                    ..Default::default()
                },
                EnumVariant {
                    name: "Inactive".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // Unit variant should not have data
        let content = "Active(123)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;
        // Should get error for providing data to unit variant
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("unit variant")
                    || d.message.contains("cannot have data")),
            "Should error on unit variant with data. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_enum_with_vec_of_custom_type_missing_fields() {
        // Create User type info
        let user_type = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "email".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "age".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "is_active".to_string(),
                    type_name: "bool".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "roles".to_string(),
                    type_name: "Vec<String>".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // Register User type with the analyzer
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(user_type);
        let analyzer = Arc::new(analyzer);

        // Create Message enum with UserTag variant
        let type_info = TypeInfo {
            name: "Message".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "UserTag".to_string(),
                fields: vec![FieldInfo {
                    name: "0".to_string(),
                    type_name: "Vec<User>".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                }],
                docs: None,
                line: None,
                column: None,
                ..Default::default()
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // Test with missing required fields in User structs
        let content = r#"UserTag([User(age: 22), User(email: "hello")])"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;

        // We expect errors about missing fields
        // First User(age: 22) is missing: id, name, email, is_active, roles
        // Second User(email: "hello") is missing: id, name, age, is_active, roles
        assert!(
            !diagnostics.is_empty(),
            "Expected diagnostics for missing fields"
        );

        // Should have 2 diagnostics, one for each struct with missing fields
        assert_eq!(
            diagnostics.len(),
            2,
            "Expected 2 diagnostics (one per struct)"
        );

        // Check first struct error
        assert!(
            diagnostics[0].message.contains("Required fields"),
            "First diagnostic should mention required fields"
        );
        assert!(
            diagnostics[0].message.contains("id"),
            "First struct should be missing 'id' field"
        );
        assert!(
            diagnostics[0].message.contains("name"),
            "First struct should be missing 'name' field"
        );
        assert!(
            diagnostics[0].message.contains("email"),
            "First struct should be missing 'email' field"
        );

        // Check second struct error
        assert!(
            diagnostics[1].message.contains("Required fields"),
            "Second diagnostic should mention required fields"
        );
        assert!(
            diagnostics[1].message.contains("id"),
            "Second struct should be missing 'id' field"
        );
        assert!(
            diagnostics[1].message.contains("age"),
            "Second struct should be missing 'age' field"
        );
    }

    #[tokio::test]
    async fn test_unknown_type_in_ron_file() {
        // Create a struct with a custom type field
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "author".to_string(),
                    type_name: "User".to_string(), // User type - doesn't need to be registered for this test
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // The user writes "UnknownType" in the RON file - this type doesn't exist
        let content = r#"Post(
            id: 1,
            author: UnknownType(name: "John"),
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;

        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)
                    && d.message.contains("Unknown type")),
            "Should report unknown type error for unregistered field type. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_known_type_in_ron_file_no_error() {
        // Register User type with the analyzer
        let user_type = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(user_type);
        let analyzer = Arc::new(analyzer);

        let type_info = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "author".to_string(),
                    type_name: "User".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        // User type is known - should not error
        let content = r#"Post(
            id: 1,
            author: User(id: 42, name: "John"),
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer).await;

        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("unknown type")),
            "Should NOT report unknown type error when type is registered. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_serde_rename_all_fields() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "max_connections".to_string(),
                    type_name: "u32".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "debug_mode".to_string(),
                    type_name: "bool".to_string(),
                    has_default: true,
                    ..Default::default()
                },
            ]),
            rename_all: Some("camelCase".to_string()),
            ..Default::default()
        };

        // The serialized (camelCase) name is what serde expects
        let content = "(maxConnections: 5)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "camelCase name should be accepted. Got: {:?}",
            diagnostics
        );

        // The Rust name is tolerated as a lenient fallback
        let content = "(max_connections: 5)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Rust name fallback should be accepted. Got: {:?}",
            diagnostics
        );

        // A missing required field is reported under its serialized name
        let content = "(debugMode: true)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("maxConnections")),
            "Missing field should use serialized name. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_serde_field_rename() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "config_kind".to_string(),
                type_name: "String".to_string(),
                rename: Some("kind".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let content = r#"(kind: "full")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Renamed field should be accepted. Got: {:?}",
            diagnostics
        );

        let content = r#"(totally_unknown: "full")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field 'totally_unknown'")),
            "Unknown field should still error. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_serde_skip_and_flatten() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Extra".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "verbose".to_string(),
                type_name: "bool".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let analyzer = Arc::new(analyzer);

        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "runtime_state".to_string(),
                    type_name: "String".to_string(),
                    skip: true,
                    ..Default::default()
                },
                FieldInfo {
                    name: "extra".to_string(),
                    type_name: "Extra".to_string(),
                    flatten: true,
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        // Flattened fields are accepted at the top level; skipped fields are
        // not required and not accepted
        let content = r#"(name: "app", verbose: true)"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Flattened field should be accepted. Got: {:?}",
            diagnostics
        );

        // Missing the flattened required field is an error
        let content = r#"(name: "app")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics.iter().any(|d| d.message.contains("verbose")),
            "Missing flattened field should error. Got: {:?}",
            diagnostics
        );

        // A skipped field in the file is unknown to serde
        let content = r#"(name: "app", verbose: true, runtime_state: "x")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field 'runtime_state'")),
            "Skipped field should be unknown. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_serde_unresolved_flatten_allows_unknown_fields() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "extra".to_string(),
                type_name: "HashMap<String, String>".to_string(),
                flatten: true,
                ..Default::default()
            }]),
            ..Default::default()
        };

        // Flattening into a map means serde accepts arbitrary keys
        let content = r#"(anything: "goes", here: "too")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field")),
            "Unresolved flatten should suppress unknown-field errors. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_duplicate_field_diagnostic() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "name".to_string(),
                type_name: "String".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let content = "(\n    name: \"a\",\n    name: \"b\",\n)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        let duplicate: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("Duplicate field 'name'"))
            .collect();
        assert_eq!(duplicate.len(), 1, "Got: {:?}", diagnostics);
        assert_eq!(
            duplicate[0].range.start.line, 2,
            "Duplicate should be reported at the second occurrence"
        );
        assert_eq!(
            duplicate[0].code,
            Some(NumberOrString::String(codes::DUPLICATE_FIELD.to_string()))
        );
    }

    #[tokio::test]
    async fn test_duplicate_field_diagnostic_nested() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Inner".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "value".to_string(),
                type_name: "u32".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let analyzer = Arc::new(analyzer);

        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "inner".to_string(),
                type_name: "Inner".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let content = "(\n    inner: (\n        value: 1,\n        value: 2,\n    ),\n)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Duplicate field 'value'")),
            "Nested duplicate should be reported. Got: {:?}",
            diagnostics
        );
    }

    /// The document root and a nested field value share one struct validator,
    /// so the same struct written in either position must raise the same errors.
    #[tokio::test]
    async fn test_root_and_nested_struct_validation_agree() {
        let inner = TypeInfo {
            name: "Inner".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "kept".to_string(),
                    type_name: "u32".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "other".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let outer = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "inner".to_string(),
                type_name: "Inner".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(inner.clone());
        analyzer.add_type(outer.clone());
        let analyzer = Arc::new(analyzer);

        // An unknown field, a field whose value has the wrong primitive type, a
        // duplicated field, and a missing required field ('other')
        let as_root = "(\n    bogus: 1,\n    kept: \"x\",\n    kept: 2,\n)";
        let as_nested =
            "(\n    inner: (\n        bogus: 1,\n        kept: \"x\",\n        kept: 2,\n    ),\n)";

        let error_codes = |diagnostics: Vec<Diagnostic>| {
            let mut codes: Vec<String> = diagnostics
                .into_iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .filter_map(|d| match d.code {
                    Some(NumberOrString::String(c)) => Some(c),
                    _ => None,
                })
                .collect();
            codes.sort();
            codes.dedup();
            codes
        };

        let root_codes = error_codes(
            validate_ron_with_analyzer(as_root, None, &inner, analyzer.clone()).await,
        );
        let nested_codes = error_codes(
            validate_ron_with_analyzer(as_nested, None, &outer, analyzer.clone()).await,
        );

        assert_eq!(
            root_codes,
            vec![
                codes::DUPLICATE_FIELD,
                codes::MISSING_REQUIRED_FIELD,
                codes::TYPE_MISMATCH,
                codes::UNKNOWN_FIELD
            ]
        );
        assert_eq!(
            nested_codes, root_codes,
            "nested struct validation diverged from the root"
        );
    }

    /// A value of the wrong primitive type is an error wherever it appears, not
    /// just directly under the document root.
    #[tokio::test]
    async fn test_type_mismatch_reported_at_every_depth() {
        let leaf = TypeInfo {
            name: "Leaf".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "port".to_string(),
                type_name: "u16".to_string(),
                has_default: true,
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        };
        let middle = TypeInfo {
            name: "Middle".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "leaf".to_string(),
                    type_name: "Leaf".to_string(),
                    has_default: true,
                    ..Default::default()
                },
                FieldInfo {
                    name: "leaves".to_string(),
                    type_name: "Vec<Leaf>".to_string(),
                    has_default: true,
                    ..Default::default()
                },
                FieldInfo {
                    name: "maybe_leaf".to_string(),
                    type_name: "Option<Leaf>".to_string(),
                    has_default: true,
                    ..Default::default()
                },
            ]),
            has_default: true,
            ..Default::default()
        };
        let root = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "middle".to_string(),
                type_name: "Middle".to_string(),
                has_default: true,
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(leaf);
        analyzer.add_type(middle);
        let analyzer = Arc::new(analyzer);

        // `port` is a u16, so a string is wrong at every one of these depths
        let bad = [
            "(middle: (leaf: (port: \"80\")))",
            "(middle: (leaves: [(port: \"80\")]))",
            "(middle: (maybe_leaf: Some((port: \"80\"))))",
        ];
        for content in bad {
            let diagnostics =
                validate_ron_with_analyzer(content, None, &root, analyzer.clone()).await;
            assert!(
                diagnostics.iter().any(|d| {
                    d.code == Some(NumberOrString::String(codes::TYPE_MISMATCH.to_string()))
                        && d.message.contains("expected u16, got string")
                }),
                "'{}' should report a type mismatch. Got: {:?}",
                content,
                diagnostics
            );
        }

        // ...and the matching well-typed documents must stay quiet
        let good = [
            "(middle: (leaf: (port: 80)))",
            "(middle: (leaves: [(port: 80), Leaf(port: 81)]))",
            "(middle: (maybe_leaf: Some((port: 80))))",
            "(middle: (maybe_leaf: None))",
            "(middle: Middle(leaf: Leaf(port: 80), leaves: [], maybe_leaf: None))",
        ];
        for content in good {
            let diagnostics =
                validate_ron_with_analyzer(content, None, &root, analyzer.clone()).await;
            assert!(
                !diagnostics
                    .iter()
                    .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
                "'{}' is valid and should raise nothing. Got: {:?}",
                content,
                diagnostics
            );
        }
    }

    #[tokio::test]
    async fn test_serde_renamed_enum_variant() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Mode".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "FastMode".to_string(),
                    ..Default::default()
                },
                EnumVariant {
                    name: "OldMode".to_string(),
                    rename: Some("legacy".to_string()),
                    ..Default::default()
                },
            ]),
            rename_all: Some("snake_case".to_string()),
            ..Default::default()
        };

        // rename_all applies to the first variant, explicit rename to the second
        for content in ["fast_mode", "legacy", "FastMode"] {
            let diagnostics =
                validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
            assert!(
                !diagnostics
                    .iter()
                    .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
                "'{}' should be a valid variant. Got: {:?}",
                content,
                diagnostics
            );
        }

        let diagnostics =
            validate_ron_with_analyzer("bogus_mode", None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown variant 'bogus_mode'")),
            "Unknown variant should still error. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_unknown_field_suggests_closest() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "ephemeral".to_string(),
                    type_name: "bool".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "timeout".to_string(),
                    type_name: "u32".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        // A typo in a known field should be reported with a suggestion.
        let content = "(\n    ephemerl: true,\n)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("Unknown field 'ephemerl' (did you mean 'ephemeral'?)")),
            "Should suggest 'ephemeral'. Got: {:?}",
            diagnostics
        );

        // A field bearing no resemblance to any known field gets no suggestion.
        let content = "(\n    hostname: true,\n)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message == "Unknown field 'hostname'"),
            "Unrelated field should have no suggestion. Got: {:?}",
            diagnostics
        );
    }

    /// A struct variant spells out named fields exactly the way a struct value
    /// does, so it goes through the same validator: its field values are
    /// type-checked, recursed into, and reported at their own position — not at
    /// the top of the file — wherever in the document the variant appears.
    #[tokio::test]
    async fn test_variant_fields_validated_like_struct_fields() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Leaf".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "port".to_string(),
                type_name: "u16".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "Mode".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "Tuned".to_string(),
                fields: vec![
                    FieldInfo {
                        name: "retries".to_string(),
                        type_name: "u32".to_string(),
                        ..Default::default()
                    },
                    FieldInfo {
                        name: "inner".to_string(),
                        type_name: "Leaf".to_string(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }]),
            ..Default::default()
        });
        let mode = analyzer.get_type_info("Mode").unwrap().clone();
        let config = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "mode".to_string(),
                type_name: "Mode".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let analyzer = Arc::new(analyzer);

        // The same variant as the document root, and as a nested field value.
        let as_root = (
            "Tuned(\n    retries: \"x\",\n    inner: (port: \"80\"),\n)",
            &mode,
            1,
        );
        let as_nested = (
            "Config(\n    mode: Tuned(\n        retries: \"x\",\n        inner: (port: \"80\"),\n    ),\n)",
            &config,
            2,
        );

        for (content, root_type, retries_line) in [as_root, as_nested] {
            let diagnostics =
                validate_ron_with_analyzer(content, None, root_type, analyzer.clone()).await;

            let retries = diagnostics
                .iter()
                .find(|d| d.message.contains("expected u32, got string"))
                .unwrap_or_else(|| {
                    panic!("a variant field's own type must be checked. Got: {diagnostics:?}")
                });
            assert_eq!(
                retries.range.start.line, retries_line,
                "the mismatch belongs on the offending line, not the top of the file"
            );

            assert!(
                diagnostics
                    .iter()
                    .any(|d| d.message.contains("expected u16, got string")),
                "a struct nested inside a variant field must be validated too. Got: {diagnostics:?}"
            );
        }

        // A required field the variant is missing is reported at the variant's
        // own name, and names the variant it belongs to.
        let diagnostics = validate_ron_with_analyzer(
            "Config(\n    mode: Tuned(retries: 1),\n)",
            None,
            &config,
            analyzer.clone(),
        )
        .await;
        let missing_code = code(codes::MISSING_REQUIRED_FIELD);
        let missing = diagnostics
            .iter()
            .find(|d| d.code == missing_code)
            .unwrap_or_else(|| panic!("a missing variant field must error. Got: {diagnostics:?}"));
        assert_eq!(missing.message, "Required fields in variant 'Tuned': inner");
        assert_eq!(missing.range.start, Position::new(1, 10));

        // ...and the complete variant raises nothing, at either depth.
        for content in [
            "Config(\n    mode: Tuned(retries: 1, inner: (port: 80)),\n)",
            "Config(\n    mode: Tuned(retries: 1, inner: Leaf(port: 80)),\n)",
        ] {
            let diagnostics =
                validate_ron_with_analyzer(content, None, &config, analyzer.clone()).await;
            assert!(
                !diagnostics
                    .iter()
                    .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
                "'{content}' is valid and should raise nothing. Got: {diagnostics:?}"
            );
        }
    }

    /// A tuple variant's payload is matched to its positional fields in order
    /// and checked against their declared types, like any other value.
    #[tokio::test]
    async fn test_tuple_variant_payload_type_checked() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Value".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "Pair".to_string(),
                fields: vec![
                    FieldInfo {
                        name: "0".to_string(),
                        type_name: "i32".to_string(),
                        ..Default::default()
                    },
                    FieldInfo {
                        name: "1".to_string(),
                        type_name: "String".to_string(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };

        let diagnostics =
            validate_ron_with_analyzer(r#"Pair(1, "a")"#, None, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "a well-typed payload should raise nothing. Got: {diagnostics:?}"
        );

        // The second element is declared String, so a number is wrong — and the
        // error belongs at that element, not at the start of the variant.
        let diagnostics =
            validate_ron_with_analyzer("Pair(1, 2)", None, &type_info, analyzer.clone()).await;
        let mismatch = diagnostics
            .iter()
            .find(|d| d.message.contains("expected String, got number"))
            .unwrap_or_else(|| {
                panic!("a tuple variant's payload must be checked. Got: {diagnostics:?}")
            });
        assert_eq!(mismatch.range.start, Position::new(0, 8));
    }

    #[tokio::test]
    async fn test_unknown_variant_suggests_closest() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Mode".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Ephemeral".to_string(),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Persistent".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let diagnostics =
            validate_ron_with_analyzer("Ephemerl", None, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("(did you mean 'Ephemeral'?)")),
            "Should suggest 'Ephemeral'. Got: {:?}",
            diagnostics
        );
    }
}
