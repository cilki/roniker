use super::type_utils::{
    closest_name, extract_inner_type, is_custom_type, is_primitive_type, missing_required_fields,
    normalize_type, short_name,
};
use crate::rust_analyzer::{FieldInfo, RustAnalyzer, TypeInfo, TypeKind};
use ron::Value;
use std::borrow::Cow;
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
    /// excluded, `flatten` expanded. Borrowed from the analyzer's memo for a
    /// registered struct, which is every struct a document walk reaches; owned
    /// only for an enum variant, whose fields are not indexed separately.
    fields: Cow<'a, [(String, FieldInfo)]>,
    /// A `flatten` target the analyzer can't resolve (e.g. a `HashMap`) makes
    /// serde accept arbitrary extra keys, so unknown fields aren't reported.
    allow_unknown_fields: bool,
    /// The container carries `#[serde(default)]`, so an absent field is not an
    /// error. Note that a bare `#[derive(Default)]` does *not* do this.
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
pub fn validate_ron_with_analyzer(
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

    // Whether the document parses as RON at all. `IgnoredAny` makes `ron` run
    // its parser over the whole document and throw the result away, which is
    // all this needs: the only thing read from it is the error. Deserializing
    // into a `Value` instead would build a copy of the entire document as an
    // owned tree, which on a large file costs more than the rest of validation
    // put together.
    if let Err(e) = ron::from_str::<serde::de::IgnoredAny>(content) {
        diagnostics.push(Diagnostic {
            range: parse_error_range(&e.span, content),
            severity: Some(DiagnosticSeverity::ERROR),
            // `e.code` is the message without the span prefix that `e` itself
            // would print; the span is already carried by `range`.
            message: format!("RON syntax error: {}", e.code),
            code: code(codes::SYNTAX_ERROR),
            ..Default::default()
        });
        return diagnostics;
    }

    // Everything else is a single walk of the parse tree from the root value
    // down, validating each value against the type it is declared as. Each
    // value is visited once, so nothing is reported twice.
    if let Some(main_value) = super::ts_utils::find_main_value(tree) {
        diagnostics.extend(validate_node_with_type_info(
            &main_value,
            content,
            type_info,
            &analyzer,
            true,
        ));
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
fn validate_struct_node(
    node: &tree_sitter::Node<'_>,
    content: &str,
    expected: &ExpectedStruct<'_>,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    use super::ts_utils;
    let mut diagnostics = Vec::new();

    // RON's unnamed struct syntax with no field written is just `()`, which the
    // grammar reports as a `unit` node of its own rather than as an empty
    // `struct`. serde still reads it as the struct the field declares, and
    // still refuses it when that struct has a field with no default — so it is
    // validated here as a struct value that supplied nothing, which is exactly
    // what it is. Skipping it instead left `server: ()`, and a whole document
    // of `()`, reported as clean and then rejected by the application.
    if !matches!(node.kind(), "struct" | "unit") {
        return diagnostics;
    }
    // Tuple/newtype structs and variants have positional fields ("0", "1", ...)
    // — named-field validation doesn't apply to them.
    if expected.fields.iter().all(|(_, f)| f.is_positional()) {
        return diagnostics;
    }

    // The field names the document writes out, in the order it writes them.
    // A list rather than a set: a struct has a handful of fields, few enough
    // that scanning them costs less than hashing each name into a table — and
    // this is built once per struct value in the document, not once per
    // document.
    let mut present_fields: Vec<&str> = Vec::new();

    for field_node in ts_utils::struct_fields(node) {
        let Some(field_name) = ts_utils::field_name(&field_node, content) else {
            continue;
        };
        // Only ever needed by a diagnostic, and almost no field has one, so it
        // is derived where it is reported rather than for every field walked.
        let name_range =
            || ts_utils::node_to_lsp_range(&field_node.child(0).unwrap_or(field_node), content);

        // Duplicate field — report at the second occurrence
        if present_fields.contains(&field_name) {
            diagnostics.push(Diagnostic {
                range: name_range(),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("Duplicate field '{}'", field_name),
                code: code(codes::DUPLICATE_FIELD),
                ..Default::default()
            });
            continue;
        }
        present_fields.push(field_name);

        let Some(field_info) = expected.matching_field(field_name) else {
            if !expected.allow_unknown_fields {
                diagnostics.push(Diagnostic {
                    range: name_range(),
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
                range: name_range(),
                severity: Some(DiagnosticSeverity::INFORMATION),
                message: format!("{}: {}", field_info.name, field_info.type_name),
                ..Default::default()
            });
        }

        diagnostics.extend(validate_typed_value(
            &value_node,
            content,
            &field_info.type_name,
            analyzer,
        ));
    }

    // Missing required fields: compare expected fields against those we saw above.
    if !expected.has_default {
        let missing =
            missing_required_fields(&expected.fields, |name| present_fields.contains(&name));
        if !missing.is_empty() {
            let missing_names: Vec<&str> = missing.iter().map(|(name, _)| name.as_str()).collect();
            diagnostics.push(Diagnostic {
                range: struct_name_range(node, content),
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
fn validate_typed_value(
    value_node: &tree_sitter::Node<'_>,
    content: &str,
    declared_type: &str,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    // A literal written straight into a primitive field — most of what a
    // configuration file is made of — needs none of the work below: the parse
    // tree has already classified the literal, and a kind that satisfies the
    // declared type settles the question on its own.
    if literal_satisfies(value_node, content, declared_type) {
        return Vec::new();
    }

    // Deep validation: Vec<T>, Option<T>, plain custom structs/enums.
    // validate_nested_value handles all generic-wrapper cases uniformly, so
    // there are no per-container special cases here.
    let deep = validate_nested_value(value_node, content, declared_type, analyzer);
    if !deep.diagnostics.is_empty() {
        return deep.diagnostics;
    }
    // The walk above went through the value itself and reported whatever it
    // found there, value by value and at each value's own position, so there is
    // nothing left for the surface check to learn — and learning it would mean
    // deserializing the value all over again, which for the long array of
    // structs a large config file mostly consists of is the whole document.
    if deep.settled {
        return Vec::new();
    }

    // Primitive / surface-level type check. The value's own source text carries
    // everything the check needs, so it runs at every nesting depth.
    //
    // Only non-custom types (primitives and std generic wrappers) are checked
    // against the typed RON value; custom structs and enums are judged from the
    // source text alone. Parsing lazily keeps a nested struct's whole subtree
    // from being re-parsed once per level.
    let value_text = super::ts_utils::node_text(value_node, content);
    let parsed_value = if is_custom_type(declared_type) {
        None
    } else {
        value_text.and_then(|text| ron::from_str::<Value>(text).ok())
    };

    let Some(error_msg) =
        check_declared_type(parsed_value.as_ref(), declared_type, value_text, analyzer)
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

/// Whether the parse tree on its own settles that this value fits
/// `declared_type`.
///
/// Tree-sitter has already classified every literal in the document, so a
/// literal whose kind plainly satisfies the primitive it is written into needs
/// nothing further: there is nothing to recurse into, and nothing to learn
/// from deserializing a value whose shape its own node kind already names.
/// That is what most of a configuration file consists of, and it was costing a
/// separate `ron` parse per value.
///
/// Answering `false` is always safe: the value then goes through
/// [`validate_nested_value`] and [`check_declared_type`] exactly as before, so
/// this can only ever skip a check that would have found nothing.
fn literal_satisfies(node: &tree_sitter::Node, content: &str, declared_type: &str) -> bool {
    tree_satisfies(node, content, &normalize_type(declared_type))
}

/// [`literal_satisfies`] on an already-normalized type name, so that the
/// recursion through generic wrappers doesn't renormalize at every layer.
///
/// The wrappers are seen *through*: a document writes `Option<T>` as `Some(v)`,
/// `None` or a bare `v`, and `Box<T>`/`Rc<T>`/`Arc<T>` as the inner value
/// outright, so in each case the question is the one the wrapped type asks of
/// the value the tree already holds. A `Vec<T>` is an array whose every element
/// has to answer it.
///
/// Only while the wrapped type is not a custom struct or enum, though. Those
/// are [`validate_nested_value`]'s business: it reports things no amount of
/// looking at the tree can — a `Vec<T>` whose `T` the analyzer never
/// registered, say — from the very same spellings, so a wrapper around one is
/// never settled here.
fn tree_satisfies(node: &tree_sitter::Node, content: &str, declared: &str) -> bool {
    if let Some(inner) = extract_inner_type(declared, "Option<") {
        if is_custom_type(inner) {
            return false;
        }
        // `None` is the one spelling that carries no value to look at.
        if node.kind() == "identifier" {
            return super::ts_utils::node_text(node, content) == Some("None");
        }
        let inner_node = super::ts_utils::unwrap_some(node, content).unwrap_or(*node);
        return tree_satisfies(&inner_node, content, inner);
    }
    if let Some(inner) = ["Box<", "Rc<", "Arc<"]
        .iter()
        .find_map(|wrapper| extract_inner_type(declared, wrapper))
    {
        return !is_custom_type(inner) && tree_satisfies(node, content, inner);
    }
    if let Some(element) = extract_inner_type(declared, "Vec<") {
        return !is_custom_type(element)
            && node.kind() == "array"
            && super::ts_utils::array_elements(node)
                .iter()
                .all(|value| tree_satisfies(value, content, element));
    }

    match (node.kind(), declared) {
        ("boolean", "bool") | ("char", "char") => true,
        ("string", "String" | "&str" | "str") => true,
        // An integer literal in a float field is fine — serde widens it — and
        // [`check_type_mismatch`] deliberately lets it through.
        ("float" | "integer", "f32" | "f64") => true,
        ("integer", _) => {
            // The document has already been deserialized once, so the literal
            // is one `ron` accepts; what is left to check is whether the
            // field's type can hold it. Only a plain decimal literal is read
            // here, and RON's hexadecimal, binary and digit-separated forms
            // fall through to the full check.
            let Some((min, max)) = integer_bounds(declared) else {
                return false;
            };
            super::ts_utils::node_text(node, content)
                .and_then(|text| text.parse::<i128>().ok())
                .is_some_and(|literal| literal >= min && literal <= max)
        }
        _ => false,
    }
}

/// The node's range, clipped to its first line. A multi-line node would
/// otherwise produce an inverted column range.
fn first_line_range(node: &tree_sitter::Node, content: &str) -> Range {
    use super::ts_utils;

    let start = ts_utils::node_start_position(node, content);
    let end_col = if node.end_position().row > node.start_position().row {
        ts_utils::line_end_column(content, node.start_byte())
    } else {
        ts_utils::node_end_position(node, content).character
    };
    Range::new(start, Position::new(start.line, end_col))
}

/// The range to report a whole-struct diagnostic at: the struct's name, or a
/// zero-width range at its start when it is written with unnamed syntax.
fn struct_name_range(node: &tree_sitter::Node, content: &str) -> Range {
    match node.child(0) {
        Some(name) if name.kind() == "identifier" => {
            super::ts_utils::node_to_lsp_range(&name, content)
        }
        _ => {
            let pos = super::ts_utils::node_start_position(node, content);
            Range::new(pos, pos)
        }
    }
}

/// What recursing into a value turned up.
struct Nested {
    diagnostics: Vec<Diagnostic>,
    /// Whether the walk went through the value itself, leaving the surface
    /// check in [`validate_typed_value`] nothing to add. Set only where the
    /// walk reached the value the document actually wrote — the elements of an
    /// array written for a `Vec`, the value inside an explicit `Some(..)` —
    /// since that is what makes the surface check redundant rather than merely
    /// expensive: it would have to deserialize the whole subtree to conclude
    /// what the walk has just reported value by value, at each value's own
    /// position.
    settled: bool,
}

/// Recurse into a value whose declared type is a custom struct/enum, or a
/// generic wrapper around one.
///
/// This is the single place that decides how to unwrap generic wrappers
/// (Vec<T>, Option<T>, …). Nothing is reported for a type we don't recurse
/// into — the caller ([`validate_typed_value`]) checks those against the
/// value's own shape instead.
fn validate_nested_value<'a>(
    value_node: &tree_sitter::Node<'a>,
    content: &str,
    field_type: &str,
    analyzer: &Arc<RustAnalyzer>,
) -> Nested {
    let mut diagnostics = Vec::new();
    let mut settled = false;
    let field_type_normalized = normalize_type(field_type);

    // Helper closure to emit an "Unknown type" diagnostic at the value node
    let unknown_type_diag = |inner: &str| Diagnostic {
        range: super::ts_utils::node_to_lsp_range(value_node, content),
        severity: Some(DiagnosticSeverity::ERROR),
        message: format!("Unknown type '{}'", inner),
        code: code(codes::UNKNOWN_TYPE),
        ..Default::default()
    };

    if let Some(inner_type) = extract_inner_type(&field_type_normalized, "Vec<") {
        // Vec<CustomType> — every element is validated against the inner type
        // exactly the way a field's value is validated against the field's
        // type, so an element of the wrong shape is reported where it is
        // written rather than at the array as a whole. Having been through the
        // array, we also know it is one, which is all the surface check in
        // [`validate_typed_value`] still wants from it.
        //
        // A `Vec` of primitives is left to that check instead: its elements
        // carry no fields to recurse into, and deserializing them together
        // costs less than visiting them one at a time.
        if is_custom_type(inner_type) {
            if analyzer.get_type_info(inner_type).is_none() {
                diagnostics.push(unknown_type_diag(inner_type));
            } else if value_node.kind() == "array" {
                for element in super::ts_utils::array_elements(value_node) {
                    diagnostics.extend(validate_typed_value(
                        &element, content, inner_type, analyzer,
                    ));
                }
                settled = true;
            }
        }
    } else if let Some(inner_type) = ["Option<", "Box<", "Arc<", "Rc<"]
        .iter()
        .find_map(|w| extract_inner_type(&field_type_normalized, w))
    {
        // Single-element wrapper — check the inner type is known
        if is_custom_type(inner_type) {
            if let Some(inner_type_info) = analyzer.get_type_info(inner_type) {
                // An `Option` field is usually written `Some(value)`. Validate
                // the wrapped value rather than the `Some` wrapper, or the
                // inner type's own fields go unchecked.
                //
                // What is inside an explicit `Some(..)` is checked against the
                // inner type exactly the way a field's value is checked
                // against the field's type — the same way a `Vec`'s elements
                // are above — so a wrapped value of the wrong shape is
                // reported where it is written and against the type that was
                // actually expected there. Having been through it, the surface
                // check in [`validate_typed_value`] has nothing left to add.
                match super::ts_utils::unwrap_some(value_node, content) {
                    Some(inner) => {
                        settled = true;
                        diagnostics
                            .extend(validate_typed_value(&inner, content, inner_type, analyzer));
                    }
                    // A bare value written for the `Option`, which RON also
                    // accepts. There is no wrapper to see through, so the
                    // value is the inner type's own and the surface check
                    // still reads it as the wrapper's.
                    None => diagnostics.extend(validate_node_with_type_info(
                        value_node,
                        content,
                        inner_type_info,
                        analyzer,
                        false,
                    )),
                }
            } else {
                diagnostics.push(unknown_type_diag(inner_type));
            }
        }
    } else if is_custom_type(&field_type_normalized) {
        // Plain custom struct/enum — validate the node directly
        if let Some(nested_type_info) = analyzer.get_type_info(&field_type_normalized) {
            diagnostics.extend(validate_node_with_type_info(
                value_node,
                content,
                nested_type_info,
                analyzer,
                false,
            ));
        } else {
            // Unknown custom type — report an error at the value node
            diagnostics.push(Diagnostic {
                range: super::ts_utils::node_to_lsp_range(value_node, content),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("Unknown type '{}'", field_type_normalized),
                code: code(codes::UNKNOWN_TYPE),
                ..Default::default()
            });
        }
    }

    Nested {
        diagnostics,
        settled,
    }
}

/// The "unknown variant" diagnostic for a name the enum `type_info` does not
/// accept, reported at `range` with a "did you mean" suggestion drawn from the
/// enum's serialized variant names.
fn unknown_variant(type_info: &TypeInfo, variant_name: &str, range: Range) -> Diagnostic {
    let rename_all = type_info.rename_all.as_deref();
    let known: Vec<String> = match &type_info.kind {
        TypeKind::Enum(variants) => variants
            .iter()
            .map(|v| v.serialized_name(rename_all).into_owned())
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
fn validate_node_with_type_info<'a>(
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
                fields: analyzer.effective_fields(type_info),
                allow_unknown_fields: type_info.has_unresolved_flatten(analyzer),
                has_default: type_info.has_default,
                variant: None,
                at_root,
            };
            validate_struct_node(node, content, &expected, analyzer)
        }
        TypeKind::Enum(_) => validate_enum_node(node, content, type_info, analyzer),
    }
}

/// Validate a value written where an enum is expected: the variant it names
/// must exist, and whatever data it carries must match that variant's fields.
///
/// RON spells a unit variant as a bare name (`Prod`), a tuple variant as
/// `Prod(30)` and a struct variant as `Prod(retries: 3)` — the last two both
/// parsing as a `struct` node whose leading identifier is the variant name.
fn validate_enum_node(
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
            diagnostics.extend(validate_typed_value(
                value_node,
                content,
                &field.type_name,
                analyzer,
            ));
        }
        return diagnostics;
    }

    let expected = ExpectedStruct {
        fields: Cow::Owned(expected),
        allow_unknown_fields: false,
        has_default: type_info.has_default,
        variant: Some(&named.name),
        at_root: false,
    };
    validate_struct_node(node, content, &expected, analyzer)
}

/// What is wrong with a value declared to be `expected_type`, worded as the
/// tail of a "Type mismatch: ..." message, or `None` when nothing is.
///
/// A primitive or a standard-library generic is judged from `value`, the value
/// parsed as RON. A custom struct or enum cannot be: `ron::Value` keeps a
/// struct's shape but not the type it named, so those are judged from
/// `value_text`, the value's own source text — which is why callers who know
/// the declared type is custom may leave `value` out entirely.
fn check_declared_type(
    value: Option<&Value>,
    expected_type: &str,
    value_text: Option<&str>,
    analyzer: &RustAnalyzer,
) -> Option<String> {
    if !is_custom_type(expected_type) {
        return check_type_mismatch(value?, expected_type);
    }

    let expected = normalize_type(expected_type);
    let trimmed = value_text?.trim();
    // `Name(..)` and a bare `Name` both name a type or variant. RON's unnamed
    // struct syntax (`(..)`) leaves nothing before the paren and so names
    // nothing, which never contradicts the declared type.
    let named = trimmed.split('(').next().unwrap_or("").trim();

    // A name that could be a type or a variant is resolved against the
    // analyzer; anything else (`42`, `"x"`) is judged by its shape below.
    if !named.is_empty() && !is_primitive_type(named) {
        // The declared type resolves here: [`validate_nested_value`] runs
        // first and reports an unresolvable one itself, which stops this check
        // from running at all.
        if let Some(type_info) = analyzer.get_type_info(&expected)
            && matches!(type_info.kind, TypeKind::Enum(_))
        {
            // An enum's value spells out one of its variants. Which spellings
            // are accepted is serde's business — `rename_all` can lower-case
            // them — so `find_variant` has the last word.
            return match type_info.find_variant(named) {
                Some(_) => None,
                None => Some(format!(
                    "unknown variant '{}' for enum {}",
                    named, expected_type
                )),
            };
        }

        // Otherwise a struct, whose value names the struct itself.
        if named == short_name(&expected) {
            return None;
        }
        // A different name that nothing in the workspace defines is a
        // misspelled or missing type, which is worth saying instead of just
        // reporting it as the wrong one. Only an uppercase name is read as a
        // type at all.
        if named.starts_with(char::is_uppercase) && analyzer.get_type_info(named).is_none() {
            return Some(format!("unknown type '{}'", named));
        }
    }

    // Nothing named the declared type, so the value's own shape is what is
    // left to report — and only when the declared type is something a RON
    // value could have named in the first place, which a tuple type is not.
    if !expected.starts_with(char::is_uppercase) {
        return None;
    }
    if trimmed.contains('(') {
        if named.is_empty() {
            // Unnamed struct syntax infers the type, so it always fits.
            return None;
        }
        return Some(format!("expected {}, got {}", expected_type, named));
    }
    // A bare value. Anything that is not a literal is most likely an enum
    // variant, which there is no enum definition here to check it against.
    Some(format!(
        "expected {}, got {}",
        expected_type,
        literal_kind(trimmed)?
    ))
}

/// What a bare RON literal is, named the way a "got ..." message wants it.
/// `None` when the text is not a literal at all.
fn literal_kind(trimmed: &str) -> Option<&'static str> {
    if trimmed.parse::<i64>().is_ok() {
        Some("integer")
    } else if trimmed.parse::<f64>().is_ok() {
        Some("float")
    } else if trimmed.starts_with('"') {
        Some("string")
    } else if trimmed == "true" || trimmed == "false" {
        Some("bool")
    } else {
        None
    }
}

/// The inclusive range of integers the named Rust integer type accepts, or
/// `None` when `type_name` is not an integer type.
///
/// `isize`/`usize` are sized after the host, which is the machine the
/// configured application is built on in practice.
fn integer_bounds(type_name: &str) -> Option<(i128, i128)> {
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
        // Everything `integer_value` can return fits in these, so there is
        // nothing left to check — but they are still integer types, and saying
        // so is what keeps a value in one from being reported as a non-numeric
        // field.
        "i128" | "u128" => (i128::MIN, i128::MAX),
        _ => return None,
    })
}

/// The exact value of an integer `ron::Number`, or `None` for a float.
///
/// `ron` parses an integer literal into the narrowest variant that holds it, so
/// the variant says nothing about the field's declared type — only the value
/// matters here, and every integer variant `ron` yields fits in an `i128`.
fn integer_value(n: &ron::Number) -> Option<i128> {
    Some(match n {
        ron::Number::I8(v) => i128::from(*v),
        ron::Number::I16(v) => i128::from(*v),
        ron::Number::I32(v) => i128::from(*v),
        ron::Number::I64(v) => i128::from(*v),
        ron::Number::U8(v) => i128::from(*v),
        ron::Number::U16(v) => i128::from(*v),
        ron::Number::U32(v) => i128::from(*v),
        ron::Number::U64(v) => i128::from(*v),
        _ => return None,
    })
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
            let is_float_value = matches!(n, ron::Number::F32(_) | ron::Number::F64(_));
            let is_float_type = matches!(clean_type.as_ref(), "f32" | "f64");
            let bounds = integer_bounds(&clean_type);

            if is_float_value {
                // A float literal only goes into a float field; `ron` will not
                // truncate it into an integer one.
                if !is_float_type {
                    return Some(format!("expected {}, got float", display_type));
                }
            } else if bounds.is_none() && !is_float_type {
                // Not a numeric field at all.
                return Some(format!("expected {}, got number", display_type));
            } else if let Some((min, max)) = bounds
                && let Some(literal) = integer_value(n)
                && (literal < min || literal > max)
            {
                // An integer literal the field's type cannot hold: `ron`
                // refuses the document with "Integer is out of bounds", so the
                // config looks fine in the editor and then fails to load. Note
                // that an integer literal in a *float* field is fine — serde
                // widens it — which is why that case falls through to `None`.
                return Some(format!("{} is out of range for {}", literal, display_type));
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

/// Where to report a syntax error: the span RON failed in, translated into LSP
/// coordinates.
///
/// RON numbers lines and columns from one and counts columns in characters; LSP
/// numbers lines from zero and counts columns in UTF-16 code units.
fn parse_error_range(span: &ron::error::Span, content: &str) -> Range {
    let start = error_position(&span.start, content);
    let end = error_position(&span.end, content);

    // An empty range is invisible in most editors, so widen it to cover the
    // character the parser stopped on.
    if start == end {
        Range::new(start, Position::new(start.line, start.character + 1))
    } else {
        Range::new(start, end)
    }
}

/// One end of a RON error span as an LSP position.
fn error_position(position: &ron::error::Position, content: &str) -> Position {
    use super::ts_utils;

    let line = position.line.saturating_sub(1) as u32;
    let char_col = position.col.saturating_sub(1);
    let text = ts_utils::line_at(content, line as usize);
    let byte_col = text
        .char_indices()
        .nth(char_col)
        .map_or(text.len(), |(byte, _)| byte);
    Position::new(line, ts_utils::utf16_column(text, byte_col))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_analyzer::{EnumVariant, FieldInfo};

    /// A struct with two required fields, for tests that only care about where a
    /// diagnostic lands rather than what it says.
    fn two_field_config() -> TypeInfo {
        TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "host".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "port".to_string(),
                    type_name: "u16".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }
    }

    fn syntax_error(content: &str) -> Diagnostic {
        let analyzer = Arc::new(RustAnalyzer::new());
        let diagnostics =
            validate_ron_with_analyzer(content, None, &two_field_config(), analyzer);
        assert_eq!(
            diagnostics.len(),
            1,
            "a document that doesn't parse gets exactly one diagnostic. Got: {:?}",
            diagnostics
        );
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String(codes::SYNTAX_ERROR.to_string())),
            "Got: {:?}",
            diagnostics[0]
        );
        diagnostics[0].clone()
    }

    /// A syntax error has to be reported where it is. RON hands back the span it
    /// failed in, and anchoring every error at the start of the file instead
    /// underlines the first character of a document whose mistake is twenty
    /// lines down.
    #[test]
    fn test_syntax_error_reported_at_the_offending_line() {
        // The colon after `port` is missing.
        let content = "Config(\n    host: \"localhost\",\n    port 8080,\n)";
        let diagnostic = syntax_error(content);

        assert_eq!(
            diagnostic.range.start.line, 2,
            "the error is on the `port` line. Got: {:?}",
            diagnostic
        );
        assert!(
            diagnostic.range.start.character >= 4,
            "the error is at or past the start of `port`. Got: {:?}",
            diagnostic
        );
    }

    /// An unclosed container is reported at the end of the document, not at its
    /// start.
    #[test]
    fn test_syntax_error_at_end_of_document() {
        let content = "Config(\n    host: \"localhost\",\n    port: 8080,\n";
        let diagnostic = syntax_error(content);

        assert!(
            diagnostic.range.start.line >= 2,
            "an unterminated struct is reported at the end. Got: {:?}",
            diagnostic
        );
    }

    /// The reported range is never empty: a zero-width diagnostic doesn't render
    /// in most editors.
    #[test]
    fn test_syntax_error_range_is_not_empty() {
        for content in [
            "Config(",
            "Config(\n    host: \"localhost\"\n    port: 8080,\n)",
            "Config(\n    host: ,\n)",
            ")",
            "Config(\n    host: \"unterminated,\n)",
        ] {
            let range = syntax_error(content).range;
            let start = (range.start.line, range.start.character);
            let end = (range.end.line, range.end.character);
            assert!(
                start < end,
                "range should be non-empty for {content:?}. Got: {range:?}"
            );
        }
    }

    /// The message is RON's own description of what went wrong, without the
    /// `3:6-4:1:` span prefix that `SpannedError` prints — the span is already
    /// carried by the diagnostic's range, and repeating it as text is noise the
    /// editor shows inline.
    #[test]
    fn test_syntax_error_message_has_no_span_prefix() {
        let content = "Config(\n    host: \"localhost\",\n    port 8080,\n)";
        let diagnostic = syntax_error(content);

        assert_eq!(
            diagnostic.message, "RON syntax error: Expected colon",
            "Got: {:?}",
            diagnostic
        );
    }

    /// Columns go out in UTF-16 code units, while RON counts them in characters.
    /// A non-ASCII string earlier on the line is what tells the two apart.
    #[test]
    fn test_syntax_error_column_is_utf16() {
        // The comma after the string is missing, so the error lands on `port`.
        let content = "Config(host: \"münchen 🚀\" port: 80)";
        let diagnostic = syntax_error(content);

        let expected = content
            .split_once("port")
            .map(|(before, _)| before.encode_utf16().count() as u32)
            .unwrap();
        assert_eq!(
            diagnostic.range.start.line, 0,
            "Got: {:?}",
            diagnostic.range
        );
        assert_eq!(
            diagnostic.range.start.character, expected,
            "column should be the UTF-16 offset of `port` ({expected}). Got: {:?}",
            diagnostic.range
        );
    }

    /// The fields of a struct variant used deep inside a document are checked
    /// against the variant's definition, which means resolving the type that
    /// owns the variant's field. That has to work however the enclosing values
    /// are spelled: on one line or several, and with or without their struct
    /// names (RON lets those be omitted).
    #[test]
    fn test_struct_variant_fields_in_nested_value() {
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
                    validate_ron_with_analyzer(&content, None, &config, analyzer.clone());
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
                validate_ron_with_analyzer(&good, None, &config, analyzer.clone());
            assert!(
                !diagnostics
                    .iter()
                    .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
                "a well-formed variant should not error ({label}). Got: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn test_enum_variant_validation() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert_eq!(
            diagnostics.len(),
            0,
            "Long should be valid. Got errors: {:?}",
            diagnostics
        );

        // Another valid variant
        let content = "Short";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert_eq!(
            diagnostics.len(),
            0,
            "Short should be valid. Got errors: {:?}",
            diagnostics
        );

        // Invalid enum variant
        let content = "Medium";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert_eq!(diagnostics.len(), 1, "Medium should be invalid");
        assert!(
            diagnostics[0].message.contains("Unknown variant 'Medium'"),
            "Expected unknown variant error, got: {}",
            diagnostics[0].message
        );

        // Invalid enum variant (typo)
        let content = "Longs";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert_eq!(diagnostics.len(), 1, "Longs should be invalid");
        assert!(
            diagnostics[0].message.contains("Unknown variant 'Longs'"),
            "Expected unknown variant error, got: {}",
            diagnostics[0].message
        );
    }

    #[test]
    fn test_struct_with_enum_field() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)
                    && d.message.contains("PostType")),
            "Should error on unknown type 'PostType'. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_nested_enum_field_variant_validation() {
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
        let diagnostics = validate_ron_with_analyzer(bad, None, &config, analyzer.clone());
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
        let diagnostics = validate_ron_with_analyzer(bad, None, &config, analyzer.clone());
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
        let diagnostics = validate_ron_with_analyzer(good, None, &config, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Valid nested enum variant should not error. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_struct_field_expects_struct_not_primitive() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);
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

    #[test]
    fn test_serde_alias_field_accepted() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field")),
            "Aliased field name should be accepted. Got: {:?}",
            diagnostics
        );

        // The canonical name still works
        let content = r#"(port: 8080)"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field")),
            "Canonical field name should be accepted. Got: {:?}",
            diagnostics
        );

        // A genuinely unknown field is still reported
        let content = r#"(bogus: 8080)"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field 'bogus'")),
            "Unknown field should still be reported. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_type_mismatch_primitives() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);
        assert!(!diagnostics.is_empty(), "Should error on type mismatch");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Type mismatch")),
            "Should have type mismatch error. Got: {:?}",
            diagnostics
        );
    }

    /// A single-field struct whose field has the given declared type, for
    /// driving the numeric checks below.
    fn single_field_type(type_name: &str) -> TypeInfo {
        TypeInfo {
            name: "Cfg".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "n".to_string(),
                type_name: type_name.to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    /// The error messages validation produces for `Cfg(n: <literal>)` where `n`
    /// is declared as `type_name`.
    fn numeric_errors(type_name: &str, literal: &str) -> Vec<String> {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = single_field_type(type_name);
        let content = format!("Cfg(n: {literal})");
        validate_ron_with_analyzer(&content, None, &type_info, analyzer)
            
            .into_iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .map(|d| d.message)
            .collect()
    }

    /// An integer literal the field's type cannot hold makes `ron` refuse the
    /// whole document with "Integer is out of bounds". Letting it through means
    /// the config looks clean in the editor and then fails to load, which is
    /// the one thing this diagnostic exists to prevent.
    #[test]
    fn test_integer_literal_out_of_range_is_reported() {
        for (type_name, literal) in [
            ("u16", "-1"),
            ("u16", "99999"),
            ("u8", "256"),
            ("u32", "-1"),
            ("usize", "-5"),
            ("i8", "128"),
            ("i8", "-129"),
            ("i16", "32768"),
            ("i32", "2147483648"),
        ] {
            let errors = numeric_errors(type_name, literal);
            assert!(
                errors.iter().any(|m| m.contains(literal)
                    && m.contains("out of range")
                    && m.contains(type_name)),
                "{literal} should be out of range for {type_name}, got {errors:?}"
            );
        }
    }

    /// The boundary values themselves, and anything inside them, deserialize
    /// fine and must not be reported.
    #[test]
    fn test_integer_literal_in_range_is_accepted() {
        for (type_name, literal) in [
            ("u8", "0"),
            ("u8", "255"),
            ("u16", "65535"),
            ("u32", "4294967295"),
            ("u64", "18446744073709551615"),
            ("i8", "-128"),
            ("i8", "127"),
            ("i16", "-32768"),
            ("i32", "2147483647"),
            ("i64", "-9223372036854775808"),
            // Wide enough to hold every literal `ron` can hand back.
            ("i128", "18446744073709551615"),
            ("u128", "18446744073709551615"),
        ] {
            let errors = numeric_errors(type_name, literal);
            assert!(
                errors.is_empty(),
                "{literal} is in range for {type_name}, got {errors:?}"
            );
        }
    }

    /// serde widens an integer literal into a float field, so `ratio: 1` for an
    /// `f64` is a perfectly good document. Reporting it put a red squiggle on
    /// config that loads without complaint.
    #[test]
    fn test_integer_literal_accepted_for_float_field() {
        for (type_name, literal) in [
            ("f32", "0"),
            ("f32", "1"),
            ("f64", "1"),
            ("f64", "-3"),
            ("f64", "1.5"),
        ] {
            let errors = numeric_errors(type_name, literal);
            assert!(
                errors.is_empty(),
                "{literal} is valid for {type_name}, got {errors:?}"
            );
        }
    }

    /// A float literal in an integer field is still wrong — `ron` will not
    /// truncate it — and a number in a field that isn't numeric at all is still
    /// wrong too.
    #[test]
    fn test_float_literal_rejected_for_integer_field() {
        let errors = numeric_errors("u16", "1.5");
        assert!(
            errors.iter().any(|m| m.contains("expected u16, got float")),
            "got {errors:?}"
        );

        let errors = numeric_errors("String", "1");
        assert!(
            errors
                .iter()
                .any(|m| m.contains("expected String, got number")),
            "got {errors:?}"
        );
    }

    /// The range check has to reach through the generic wrappers, since that is
    /// where the integer actually lands.
    #[test]
    fn test_integer_range_checked_through_wrappers() {
        for (type_name, literal) in [
            ("Option<u8>", "Some(300)"),
            ("Option<u8>", "300"),
            ("Box<u8>", "300"),
            ("Vec<u8>", "[1, 300]"),
        ] {
            let errors = numeric_errors(type_name, literal);
            assert!(
                errors
                    .iter()
                    .any(|m| m.contains("300") && m.contains("out of range") && m.contains("u8")),
                "{literal} should be out of range for {type_name}, got {errors:?}"
            );
        }

        // `None` and in-range payloads stay clean.
        for (type_name, literal) in [
            ("Option<u8>", "None"),
            ("Option<u8>", "Some(200)"),
            ("Box<u8>", "200"),
            ("Vec<u8>", "[1, 200]"),
        ] {
            let errors = numeric_errors(type_name, literal);
            assert!(
                errors.is_empty(),
                "{literal} is valid for {type_name}, got {errors:?}"
            );
        }
    }

    /// A value whose declared type bottoms out in primitives is settled from
    /// the parse tree alone, which is only ever allowed to *skip* a check that
    /// would have found nothing. These are the spellings the tree looks right
    /// at a glance and is wrong about, so each has to come back reported
    /// exactly as it would be if nothing were skipped.
    #[test]
    fn test_tree_shortcut_does_not_swallow_wrapper_mismatches() {
        for (type_name, literal, expected) in [
            // A wrapper the tree can see through still gets its contents
            // checked, element by element and layer by layer.
            ("Vec<String>", r#"["a", 1]"#, "in Vec: expected String, got"),
            ("Vec<u16>", r#"[1, "x"]"#, "in Vec: expected u16, got string"),
            ("Option<String>", "Some(1)", "expected String, got"),
            ("Option<bool>", "Some(\"x\")", "expected bool, got string"),
            ("Box<u8>", "\"x\"", "expected u8, got string"),
            ("Vec<Vec<u8>>", "[[1], [300]]", "out of range for u8"),
            // A non-array written for a `Vec` is not an array of anything.
            ("Vec<String>", "5", "expected Vec<String>, got"),
            // `None` is the one spelling with nothing inside it to look at,
            // and it only fits an `Option`.
            ("Vec<String>", "None", "expected Vec<String>, got"),
            ("u8", "None", "expected u8, got None"),
        ] {
            let errors = numeric_errors(type_name, literal);
            assert!(
                errors.iter().any(|m| m.contains(expected)),
                "{literal} in {type_name} should report {expected:?}, got {errors:?}"
            );
        }
    }

    /// The analyzer resolves the type a wrapper wraps, and a wrapper around
    /// one it has never heard of is the build script missing a source file —
    /// which has to be reported however the document spells the wrapper,
    /// including the spellings that carry no value to look at.
    #[test]
    fn test_unknown_wrapped_type_reported_for_every_spelling() {
        for (type_name, literal) in [
            ("Option<Ghost>", "None"),
            ("Option<Ghost>", "Some(Ghost())"),
            ("Vec<Ghost>", "[]"),
            ("Vec<Ghost>", "[Ghost()]"),
            ("Box<Ghost>", "Ghost()"),
        ] {
            let errors = numeric_errors(type_name, literal);
            assert!(
                errors.iter().any(|m| m.contains("Unknown type 'Ghost'")),
                "{literal} in {type_name} should report the unregistered type, got {errors:?}"
            );
        }
    }

    /// What an explicit `Some(..)` wraps is checked against the type the
    /// `Option` declares, the same way a `Vec`'s elements are: against the
    /// inner type, and reported where the offending value is written rather
    /// than at the wrapper around it.
    #[test]
    fn test_some_payload_checked_against_the_inner_type() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Retry".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "attempts".to_string(),
                type_name: "u8".to_string(),
                has_default: true,
                ..Default::default()
            }]),
            ..Default::default()
        });
        let analyzer = Arc::new(analyzer);
        let config = single_field_type("Option<Retry>");

        // A literal where the wrapped struct belongs is a mismatch against
        // that struct, pointed at the literal itself.
        let content = "Cfg(n: Some(42))";
        let errors: Vec<_> = validate_ron_with_analyzer(content, None, &config, analyzer.clone())
            .into_iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        assert_eq!(errors.len(), 1, "got {errors:?}");
        assert!(
            errors[0].message.contains("expected Retry"),
            "got {:?}",
            errors[0].message
        );
        let offending = content.find("42").unwrap() as u32;
        assert_eq!(
            (errors[0].range.start.character, errors[0].range.end.character),
            (offending, offending + 2),
            "the payload is underlined, not the `Some(..)` around it"
        );

        // And the well-formed spellings stay clean.
        for literal in ["None", "Some(Retry())", "Some(Retry(attempts: 3))"] {
            let content = format!("Cfg(n: {literal})");
            let errors: Vec<_> =
                validate_ron_with_analyzer(&content, None, &config, analyzer.clone())
                    .into_iter()
                    .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                    .map(|d| d.message)
                    .collect();
            assert!(errors.is_empty(), "{literal} is valid, got {errors:?}");
        }
    }

    #[test]
    fn test_unnamed_struct_syntax() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);
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

    #[test]
    fn test_enum_with_tuple_variant() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert_eq!(
            diagnostics.len(),
            0,
            "Tuple variant should be valid. Got errors: {:?}",
            diagnostics
        );

        // Another valid variant
        let content = r#"Str("hello")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);
        assert_eq!(
            diagnostics.len(),
            0,
            "Tuple variant with string should be valid. Got errors: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_unit_variant_with_data_error() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);
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

    #[test]
    fn test_enum_with_vec_of_custom_type_missing_fields() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);

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

    #[test]
    fn test_unknown_type_in_ron_file() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);

        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)
                    && d.message.contains("Unknown type")),
            "Should report unknown type error for unregistered field type. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_known_type_in_ron_file_no_error() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer);

        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("unknown type")),
            "Should NOT report unknown type error when type is registered. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_serde_rename_all_fields() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "camelCase name should be accepted. Got: {:?}",
            diagnostics
        );

        // The Rust name is tolerated as a lenient fallback
        let content = "(max_connections: 5)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Rust name fallback should be accepted. Got: {:?}",
            diagnostics
        );

        // A missing required field is reported under its serialized name
        let content = "(debugMode: true)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("maxConnections")),
            "Missing field should use serialized name. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_serde_field_rename() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Renamed field should be accepted. Got: {:?}",
            diagnostics
        );

        let content = r#"(totally_unknown: "full")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field 'totally_unknown'")),
            "Unknown field should still error. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_serde_skip_and_flatten() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Flattened field should be accepted. Got: {:?}",
            diagnostics
        );

        // Missing the flattened required field is an error
        let content = r#"(name: "app")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            diagnostics.iter().any(|d| d.message.contains("verbose")),
            "Missing flattened field should error. Got: {:?}",
            diagnostics
        );

        // A skipped field in the file is unknown to serde
        let content = r#"(name: "app", verbose: true, runtime_state: "x")"#;
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field 'runtime_state'")),
            "Skipped field should be unknown. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_serde_unresolved_flatten_allows_unknown_fields() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown field")),
            "Unresolved flatten should suppress unknown-field errors. Got: {:?}",
            diagnostics
        );
    }

    /// A crate that doesn't import its standard-library types names them by
    /// their module path, and the analyzer records that spelling. Every type
    /// check here matched the bare name only, so a correct document came back
    /// with `type-mismatch` on its arrays, maps and `Some(..)`s,
    /// `unknown-type` on its strings, and the `Option` field reported as a
    /// `missing-required-field`.
    #[test]
    fn test_qualified_std_field_types_accept_a_correct_document() {
        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "crate::config::Server".to_string(),
            kind: TypeKind::Struct(vec![field("host", "String")]),
            ..Default::default()
        });
        let type_info = TypeInfo {
            name: "Root".to_string(),
            kind: TypeKind::Struct(vec![
                field("labels", "std::collections::HashMap<String, String>"),
                field("enabled", "std::collections::HashSet<String>"),
                field("timeout", "std::option::Option<u64>"),
                field("hosts", "std::vec::Vec<String>"),
                field("server", "std::boxed::Box<Server>"),
                field("name", "std::string::String"),
            ]),
            ..Default::default()
        };
        let analyzer = Arc::new(analyzer);

        let content = concat!(
            "Root(\n",
            "    labels: {\"a\": \"b\"},\n",
            "    enabled: [\"f\"],\n",
            "    timeout: Some(5),\n",
            "    hosts: [\"h\"],\n",
            "    server: Server(host: \"h\"),\n",
            "    name: \"n\",\n",
            ")",
        );
        let errors: Vec<_> =
            validate_ron_with_analyzer(content, None, &type_info, analyzer.clone())
                .into_iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .collect();
        assert!(errors.is_empty(), "Got: {errors:?}");

        // The `Option` field is optional, so leaving it out is not an error
        // either.
        let content = concat!(
            "Root(\n",
            "    labels: {},\n",
            "    enabled: [],\n",
            "    hosts: [],\n",
            "    server: Server(host: \"h\"),\n",
            "    name: \"n\",\n",
            ")",
        );
        let errors: Vec<_> =
            validate_ron_with_analyzer(content, None, &type_info, analyzer.clone())
                .into_iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .collect();
        assert!(errors.is_empty(), "Got: {errors:?}");
    }

    /// The qualified spelling must not make a mismatch go unreported either:
    /// unqualifying the name is only meant to put the check back on the
    /// footing the bare spelling is on.
    #[test]
    fn test_qualified_std_field_types_still_report_a_mismatch() {
        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            ..Default::default()
        };
        let type_info = TypeInfo {
            name: "Root".to_string(),
            kind: TypeKind::Struct(vec![
                field("hosts", "std::vec::Vec<String>"),
                field("name", "std::string::String"),
            ]),
            ..Default::default()
        };
        let analyzer = Arc::new(RustAnalyzer::new());

        let content = "Root(\n    hosts: [1],\n    name: 7,\n)";
        let messages: Vec<_> = validate_ron_with_analyzer(content, None, &type_info, analyzer)
            .into_iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .map(|d| (d.code, d.message))
            .collect();

        assert_eq!(
            messages,
            vec![
                (
                    code(codes::TYPE_MISMATCH),
                    "Type mismatch: in Vec: expected String, got number".to_string()
                ),
                (
                    code(codes::TYPE_MISMATCH),
                    "Type mismatch: expected String, got number".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_duplicate_field_diagnostic() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
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

    #[test]
    fn test_duplicate_field_diagnostic_nested() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
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
    #[test]
    fn test_root_and_nested_struct_validation_agree() {
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
            validate_ron_with_analyzer(as_root, None, &inner, analyzer.clone()),
        );
        let nested_codes = error_codes(
            validate_ron_with_analyzer(as_nested, None, &outer, analyzer.clone()),
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
    #[test]
    fn test_type_mismatch_reported_at_every_depth() {
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
                validate_ron_with_analyzer(content, None, &root, analyzer.clone());
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
                validate_ron_with_analyzer(content, None, &root, analyzer.clone());
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

    /// Every element of a `Vec<CustomType>` is checked against the element
    /// type, and an element that isn't even the right kind of value is reported
    /// where it is written — the array's own shape, meanwhile, is still checked
    /// against the field's type.
    #[test]
    fn test_vec_element_shape_is_checked_per_element() {
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
        let root = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "leaves".to_string(),
                type_name: "Vec<Leaf>".to_string(),
                has_default: true,
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        };
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(leaf);
        let analyzer = Arc::new(analyzer);

        // Line 3 holds the one element that is not a struct at all.
        let content = "(\n    leaves: [\n        (port: 80),\n        42,\n    ],\n)";
        let diagnostics = validate_ron_with_analyzer(content, None, &root, analyzer.clone());
        let mismatches: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == Some(NumberOrString::String(codes::TYPE_MISMATCH.to_string())))
            .collect();
        assert_eq!(
            mismatches.len(),
            1,
            "only the bad element is a mismatch. Got: {:?}",
            diagnostics
        );
        assert!(
            mismatches[0].message.contains("expected Leaf, got integer"),
            "Got: {:?}",
            mismatches[0]
        );
        assert_eq!(
            mismatches[0].range.start.line, 3,
            "reported at the element, not at the array. Got: {:?}",
            mismatches[0]
        );

        // The array itself still has to be an array.
        let diagnostics =
            validate_ron_with_analyzer("(leaves: 42)", None, &root, analyzer.clone());
        assert!(
            diagnostics.iter().any(|d| {
                d.code == Some(NumberOrString::String(codes::TYPE_MISMATCH.to_string()))
                    && d.message.contains("expected Vec<Leaf>, got number")
            }),
            "a non-array where a Vec belongs is a mismatch. Got: {:?}",
            diagnostics
        );

        // ...and a well-typed array raises nothing.
        let diagnostics = validate_ron_with_analyzer(
            "(leaves: [(port: 80), Leaf(port: 81)])",
            None,
            &root,
            analyzer,
        )
        ;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_serde_renamed_enum_variant() {
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
                validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
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
            validate_ron_with_analyzer("bogus_mode", None, &type_info, analyzer.clone());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown variant 'bogus_mode'")),
            "Unknown variant should still error. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_unknown_field_suggests_closest() {
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
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("Unknown field 'ephemerl' (did you mean 'ephemeral'?)")),
            "Should suggest 'ephemeral'. Got: {:?}",
            diagnostics
        );

        // A field bearing no resemblance to any known field gets no suggestion.
        let content = "(\n    hostname: true,\n)";
        let diagnostics = validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
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
    #[test]
    fn test_variant_fields_validated_like_struct_fields() {
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
                validate_ron_with_analyzer(content, None, root_type, analyzer.clone());

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
        ;
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
                validate_ron_with_analyzer(content, None, &config, analyzer.clone());
            assert!(
                !diagnostics
                    .iter()
                    .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
                "'{content}' is valid and should raise nothing. Got: {diagnostics:?}"
            );
        }
    }

    /// End to end from Rust source: `#[derive(Default)]` on a config struct must
    /// not suppress the missing-field diagnostic. `#[derive(Debug, Clone,
    /// Default, Serialize, Deserialize)]` is the standard shape of a config
    /// type — the README's own example and this repository's example types both
    /// use it — so folding the derive into "has a default" meant
    /// `missing-required-field` never fired for anybody, while serde went on
    /// refusing to deserialize the file.
    #[cfg(feature = "analyze")]
    #[test]
    fn test_derive_default_still_requires_fields() {
        let mut analyzer = RustAnalyzer::new();
        analyzer
            .add_source_with_prefix(
                "crate",
                r#"
                #[derive(Debug, Clone, Default, Serialize, Deserialize)]
                pub struct AppConfig {
                    #[serde(default)]
                    pub debug: bool,
                    pub port: u16,
                    pub server: ServerConfig,
                }

                #[derive(Debug, Clone, Default, Serialize, Deserialize)]
                pub struct ServerConfig {
                    pub host: String,
                    #[serde(default)]
                    pub mode: String,
                }

                #[derive(Debug, Clone, Default, Serialize, Deserialize)]
                #[serde(default)]
                pub struct Tunables {
                    pub retries: u32,
                }
                "#,
            )
            .unwrap();

        let root = analyzer
            .get_type_info("crate::AppConfig")
            .expect("AppConfig")
            .clone();
        let analyzer = Arc::new(analyzer);

        // `port` is absent at the root and `host` is absent in the nested
        // struct. Both are required, at both depths.
        let missing = |content: &'static str| {
            validate_ron_with_analyzer(content, None, &root, analyzer.clone())
                .into_iter()
                .filter(|d| d.code == code(codes::MISSING_REQUIRED_FIELD))
                .map(|d| d.message)
                .collect::<Vec<_>>()
        };

        let no_port = "AppConfig(\n    debug: true,\n    server: ServerConfig(host: \"x\"),\n)";
        let no_host = "AppConfig(\n    port: 80,\n    server: ServerConfig(mode: \"fast\"),\n)";
        let complete = "AppConfig(\n    port: 80,\n    server: ServerConfig(host: \"x\"),\n)";

        assert_eq!(
            missing(no_port),
            vec!["Required fields: port".to_string()],
            "a root struct that merely derives Default still requires its fields"
        );
        assert_eq!(
            missing(no_host),
            vec!["Required fields: host".to_string()],
            "a nested struct that merely derives Default still requires its fields"
        );

        // The fully specified document is still clean.
        assert!(missing(complete).is_empty());

        // A container `#[serde(default)]` does still excuse everything.
        let tunables = analyzer.get_type_info("crate::Tunables").expect("Tunables");
        assert!(
            validate_ron_with_analyzer("Tunables()", None, tunables, analyzer.clone())
                
                .iter()
                .all(|d| d.code != code(codes::MISSING_REQUIRED_FIELD)),
            "container #[serde(default)] still makes every field optional"
        );
    }

    /// A tuple variant's payload is matched to its positional fields in order
    /// and checked against their declared types, like any other value.
    #[test]
    fn test_tuple_variant_payload_type_checked() {
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
            validate_ron_with_analyzer(r#"Pair(1, "a")"#, None, &type_info, analyzer.clone());
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "a well-typed payload should raise nothing. Got: {diagnostics:?}"
        );

        // The second element is declared String, so a number is wrong — and the
        // error belongs at that element, not at the start of the variant.
        let diagnostics =
            validate_ron_with_analyzer("Pair(1, 2)", None, &type_info, analyzer.clone());
        let mismatch = diagnostics
            .iter()
            .find(|d| d.message.contains("expected String, got number"))
            .unwrap_or_else(|| {
                panic!("a tuple variant's payload must be checked. Got: {diagnostics:?}")
            });
        assert_eq!(mismatch.range.start, Position::new(0, 8));
    }

    #[test]
    fn test_unknown_variant_suggests_closest() {
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
            validate_ron_with_analyzer("Ephemerl", None, &type_info, analyzer.clone());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("(did you mean 'Ephemeral'?)")),
            "Should suggest 'Ephemeral'. Got: {:?}",
            diagnostics
        );
    }

    /// A diagnostic's range is a column in UTF-16 code units, which is what an
    /// editor slices the line with. Reporting tree-sitter's byte column instead
    /// put the squiggle one column right for every extra byte earlier on the
    /// line, so a document with an accented or emoji string value underlined
    /// the wrong text.
    #[test]
    fn test_diagnostic_columns_are_utf16() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "host".to_string(),
                    type_name: "String".to_string(),
                    has_default: true,
                    ..Default::default()
                },
                FieldInfo {
                    name: "port".to_string(),
                    type_name: "u16".to_string(),
                    has_default: true,
                    ..Default::default()
                },
            ]),
            has_default: true,
            ..Default::default()
        };

        // `ü` is two bytes; `🚀` is four bytes and two UTF-16 code units.
        for host in ["x", "münchen", "🚀"] {
            let content = format!("Config(host: \"{host}\", bogus: 1, port: 80)");
            let diagnostics =
                validate_ron_with_analyzer(&content, None, &type_info, analyzer.clone());

            let unknown = diagnostics
                .iter()
                .find(|d| d.code == code(codes::UNKNOWN_FIELD))
                .unwrap_or_else(|| panic!("'bogus' must be reported. Got: {diagnostics:?}"));

            // Slice the line the way a client does: by UTF-16 code units.
            let units: Vec<u16> = content.encode_utf16().collect();
            let start = unknown.range.start.character as usize;
            let end = unknown.range.end.character as usize;
            let under = String::from_utf16(&units[start..end]).unwrap();
            assert_eq!(under, "bogus", "the squiggle must cover the offending field");
        }
    }

    /// The same for a type mismatch, whose range is clipped to the value's
    /// first line.
    #[test]
    fn test_type_mismatch_columns_are_utf16() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "host".to_string(),
                    type_name: "String".to_string(),
                    has_default: true,
                    ..Default::default()
                },
                FieldInfo {
                    name: "port".to_string(),
                    type_name: "u16".to_string(),
                    has_default: true,
                    ..Default::default()
                },
            ]),
            has_default: true,
            ..Default::default()
        };

        let content = "Config(host: \"🚀\", port: \"nope\")";
        let diagnostics =
            validate_ron_with_analyzer(content, None, &type_info, analyzer.clone());
        let mismatch = diagnostics
            .iter()
            .find(|d| d.code == code(codes::TYPE_MISMATCH))
            .unwrap_or_else(|| panic!("the port value must be reported. Got: {diagnostics:?}"));

        let units: Vec<u16> = content.encode_utf16().collect();
        let under = String::from_utf16(
            &units[mismatch.range.start.character as usize..mismatch.range.end.character as usize],
        )
        .unwrap();
        assert_eq!(under, "\"nope\"");
    }

    /// `Config(mode: Mode, server: Server)`, with a second struct `Other` that
    /// happens to have the same field as `Server` — so that a value naming the
    /// wrong type still passes the field checks and gets as far as the
    /// surface-level type check.
    fn named_value_fixture() -> (TypeInfo, Arc<RustAnalyzer>) {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
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
        });
        for name in ["Server", "Other"] {
            analyzer.add_type(TypeInfo {
                name: name.to_string(),
                kind: TypeKind::Struct(vec![FieldInfo {
                    name: "host".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            });
        }
        let config = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "mode".to_string(),
                    type_name: "Mode".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "server".to_string(),
                    type_name: "Server".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        (config, Arc::new(analyzer))
    }

    /// The type-mismatch messages for a document, in order.
    fn type_mismatches(content: &str) -> Vec<String> {
        let (config, analyzer) = named_value_fixture();
        validate_ron_with_analyzer(content, None, &config, analyzer)
            
            .into_iter()
            .filter(|d| d.code == code(codes::TYPE_MISMATCH))
            .map(|d| d.message)
            .collect()
    }

    /// What a value *names* is checked against the type the field is declared
    /// as, once the field-by-field checks have found nothing to say. Each of
    /// these spellings is wrong in a different way and the message has to name
    /// which: a bare value in an enum field is not one of its variants, a
    /// struct value naming a type that doesn't exist is a different mistake
    /// from one naming a type that does, and a literal where a struct belongs
    /// is reported by what the literal is.
    #[test]
    fn test_named_type_is_checked_against_the_declared_type() {
        for (content, expected) in [
            // A value that names no variant of the enum it is declared as.
            (
                r#"Config(mode: 42, server: Server(host: "x"))"#,
                "Type mismatch: unknown variant '42' for enum Mode",
            ),
            // A struct value naming a type that is registered, but not the one
            // the field is declared as.
            (
                r#"Config(mode: Ephemeral, server: Other(host: "x"))"#,
                "Type mismatch: expected Server, got Other",
            ),
            // A struct value naming a type nothing in the workspace defines.
            (
                r#"Config(mode: Ephemeral, server: Bogus(host: "x"))"#,
                "Type mismatch: unknown type 'Bogus'",
            ),
            // A literal where a struct belongs, reported by what it is.
            (
                r#"Config(mode: Ephemeral, server: 7)"#,
                "Type mismatch: expected Server, got integer",
            ),
            (
                r#"Config(mode: Ephemeral, server: "x")"#,
                "Type mismatch: expected Server, got string",
            ),
            (
                r#"Config(mode: Ephemeral, server: true)"#,
                "Type mismatch: expected Server, got bool",
            ),
        ] {
            assert_eq!(
                type_mismatches(content),
                vec![expected.to_string()],
                "for {content}"
            );
        }
    }

    /// Every error carries one of the codes [`codes`] declares, which is the
    /// whole point of having them: an editor filters on the code rather than on
    /// the message. An error published without one, or under a code nobody
    /// documents, is invisible to that filter.
    #[test]
    fn test_every_error_carries_a_documented_code() {
        let documented = [
            codes::SYNTAX_ERROR,
            codes::UNKNOWN_FIELD,
            codes::DUPLICATE_FIELD,
            codes::MISSING_REQUIRED_FIELD,
            codes::UNKNOWN_VARIANT,
            codes::UNKNOWN_TYPE,
            codes::TYPE_MISMATCH,
        ];

        for content in [
            // Doesn't parse as RON at all.
            "Config(mode: ",
            // An unknown field, a name no variant of `Mode` answers to, and a
            // value naming a struct other than the declared one.
            r#"Config(bogus: 1, mode: Nope, server: Other(host: "x"))"#,
            // The same field twice, and a nested struct missing its own field.
            r#"Config(mode: Ephemeral, mode: Ephemeral, server: ())"#,
        ] {
            let (config, analyzer) = named_value_fixture();
            let diagnostics = validate_ron_with_analyzer(content, None, &config, analyzer);
            assert!(!diagnostics.is_empty(), "nothing reported for {content}");

            for diagnostic in &diagnostics {
                if diagnostic.severity != Some(DiagnosticSeverity::ERROR) {
                    continue;
                }
                let Some(NumberOrString::String(found)) = &diagnostic.code else {
                    panic!("an error with no code: {diagnostic:?} for {content}");
                };
                assert!(
                    documented.contains(&found.as_str()),
                    "undocumented code {found:?} for {content}"
                );
            }
        }
    }

    /// The informational type hints are the one thing published without a code,
    /// so their severity is all a client has to tell them apart by — which is
    /// exactly what `Backend::publish_diagnostics` filters on when they are
    /// switched off.
    #[test]
    fn test_informational_hint_is_identified_by_severity_alone() {
        let (config, analyzer) = named_value_fixture();
        let diagnostics = validate_ron_with_analyzer(
            r#"Config(mode: Ephemeral, server: (host: "x"))"#,
            None,
            &config,
            analyzer,
        )
        ;

        let hints: Vec<&Diagnostic> = diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::INFORMATION))
            .collect();
        assert_eq!(
            hints.len(),
            1,
            "the root field whose value names nothing is annotated. Got: {diagnostics:?}"
        );
        assert_eq!(hints[0].message, "server: Server");
        assert_eq!(hints[0].code, None, "Got: {:?}", hints[0]);
    }

    /// RON lets a struct value omit its type name, which leaves nothing to
    /// disagree with the declared type — at the root and nested alike.
    #[test]
    fn test_unnamed_value_never_mismatches_its_declared_type() {
        for content in [
            r#"Config(mode: Ephemeral, server: (host: "x"))"#,
            r#"(mode: Ephemeral, server: (host: "x"))"#,
        ] {
            assert_eq!(
                type_mismatches(content),
                Vec::<String>::new(),
                "for {content}"
            );
        }
    }

    /// An unnamed struct with nothing in it is written `()`, which the grammar
    /// hands back as a `unit` token rather than as an empty `struct`. serde
    /// reads it as the struct the position declares and refuses it when that
    /// struct has a field with no default — `ron::from_str` answers
    /// `MissingStructField` — so the fields it leaves out have to be reported
    /// like any other struct's. Letting the node kind decide meant `()` was
    /// the one way to write a struct that the server never looked inside,
    /// which is exactly the spelling someone reaches for to mean "defaults
    /// here".
    #[test]
    fn test_empty_unnamed_struct_still_requires_fields() {
        let missing = |content: &'static str| {
            let (config, analyzer) = named_value_fixture();
            validate_ron_with_analyzer(content, None, &config, analyzer)
                .into_iter()
                .filter(|d| d.code == code(codes::MISSING_REQUIRED_FIELD))
                .map(|d| (d.message, d.range.start))
                .collect::<Vec<_>>()
        };

        // Nested: `Server` still wants its `host`.
        assert_eq!(
            missing("Config(\n    mode: Ephemeral,\n    server: (),\n)"),
            vec![("Required fields: host".to_string(), Position::new(2, 12))],
            "`()` written for a struct supplies none of its fields"
        );

        // The whole document: the root type's own fields are all absent, and
        // the report lands at the value's first character, there being no
        // struct name to put it on.
        assert_eq!(
            missing("()"),
            vec![(
                "Required fields: mode, server".to_string(),
                Position::new(0, 0)
            )],
            "a document that is only `()` is missing every root field"
        );

        // Nothing is invented for a struct whose fields all have defaults.
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Tunables".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "retries".to_string(),
                type_name: "u32".to_string(),
                ..Default::default()
            }]),
            has_default: true,
            ..Default::default()
        });
        let tunables = analyzer
            .get_type_info("Tunables")
            .expect("Tunables")
            .clone();
        assert!(
            validate_ron_with_analyzer("()", None, &tunables, Arc::new(analyzer))
                
                .is_empty(),
            "`()` is a complete value for a struct that needs nothing"
        );
    }

    /// An element of a `Vec<T>` is a value in its own right, so `()` written as
    /// one is checked against `T` the way a field's value is checked against
    /// the type the field declares.
    #[test]
    fn test_empty_unnamed_struct_in_vec_element_requires_fields() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Server".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "host".to_string(),
                type_name: "String".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let config = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "servers".to_string(),
                type_name: "Vec<Server>".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let content = r#"Config(servers: [(host: "x"), ()])"#;
        let missing: Vec<_> =
            validate_ron_with_analyzer(content, None, &config, Arc::new(analyzer))
                
                .into_iter()
                .filter(|d| d.code == code(codes::MISSING_REQUIRED_FIELD))
                .map(|d| (d.message, d.range.start))
                .collect();

        assert_eq!(
            missing,
            vec![("Required fields: host".to_string(), Position::new(0, 30))],
            "the empty element is reported where it is written, the complete one not at all"
        );
    }
}
