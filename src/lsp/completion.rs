use super::tree_sitter_parser;
use super::type_utils::{
    SEQUENCE_WRAPPERS, TRANSPARENT_WRAPPERS, extract_inner_type, normalize_type, short_name,
    std_type, strip_outer_generic, tuple_element_types,
};
use crate::rust_analyzer::{EnumVariant, FieldInfo, RustAnalyzer, StdShape, TypeInfo, TypeKind};
use std::borrow::Cow;
use std::sync::Arc;
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, Documentation, InsertTextFormat, MarkupContent, MarkupKind,
    Position,
};
use tree_sitter::Tree;

/// What the cursor is positioned to complete. The value contexts carry the field
/// being given a value, which the tree node under the cursor cannot always be
/// used to recover: at the end of a value being typed it resolves to the
/// enclosing struct rather than to the field.
#[derive(Debug, PartialEq)]
enum CompletionContext {
    /// Completing field names (e.g., after comma or opening paren)
    FieldName,
    /// Completing a value after a colon
    FieldValue(Option<String>),
    /// Completing the struct type name of a nested value
    StructType(Option<String>),
}

/// Determine what we're completing based on cursor position using tree-sitter
fn get_completion_context(tree: &Tree, content: &str, position: Position) -> CompletionContext {
    use super::ts_utils;

    let node = match ts_utils::node_at_position(tree, content, position) {
        Some(n) => n,
        None => return CompletionContext::FieldName,
    };

    // Walk outward from the cursor and stop at whichever comes first: the body
    // of a struct, or a field. Looking only for a `field` ancestor is not
    // enough, because a nested struct is itself the value of an enclosing
    // field — a cursor anywhere inside `server: ServerConfig(...)` would be
    // read as the `server` field's value position, so field names of the
    // nested struct were never offered.
    for ancestor in ts_utils::ancestors(tree, node) {
        match ancestor.kind() {
            // Inside this struct's parentheses. A cursor still on the struct's
            // own name is not in the body yet, so it falls through to the
            // enclosing field instead.
            "struct" if inside_struct_body(&ancestor, content, position) => {
                return struct_body_context(&ancestor, tree, content, position);
            }
            "field" => return field_completion_context(&ancestor, content, position),
            _ => {}
        }
    }

    // Default to field name completion
    CompletionContext::FieldName
}

/// Whether `position` is inside `struct_node`'s parentheses rather than on the
/// struct name that precedes them.
fn inside_struct_body(struct_node: &tree_sitter::Node, content: &str, position: Position) -> bool {
    use super::ts_utils;

    let Some(open_paren) = ts_utils::child_by_kind(struct_node, "(") else {
        // No parentheses parsed yet (an incomplete struct); treat the whole
        // node as its body, which is what the cursor is editing.
        return true;
    };
    let open_end = ts_utils::node_end_position(&open_paren, content);
    position.line > open_end.line
        || (position.line == open_end.line && position.character >= open_end.character)
}

/// The context for a cursor inside a struct's parentheses: the value position of
/// the field it is in the middle of, or a field name when it sits in the space
/// between fields.
///
/// The cursor's deepest node is unreliable here — at the end of a value being
/// typed it resolves to the enclosing struct rather than the value — so the
/// struct's own children are what decide.
fn struct_body_context(
    struct_node: &tree_sitter::Node,
    tree: &Tree,
    content: &str,
    position: Position,
) -> CompletionContext {
    use super::ts_utils;

    let cursor = ts_utils::position_to_byte_offset(content, position);
    let mut walk = struct_node.walk();
    let field = struct_node.children(&mut walk).find(|child| {
        child.kind() == "field" && cursor >= child.start_byte() && cursor <= child.end_byte()
    });
    if let Some(field) = field {
        return field_completion_context(&field, content, position);
    }

    // A value with no terminator yet (`mode: `) defeats tree-sitter's `field`
    // production entirely, so there is no field child to find above.
    match tree_sitter_parser::unterminated_value_field(tree, content, position) {
        Some(field) => CompletionContext::FieldValue(Some(field)),
        None => CompletionContext::FieldName,
    }
}

/// The context for a cursor somewhere within `field_node`: a value (or the name
/// of the type that value is about to be given) once the cursor is past the
/// field's name, and otherwise the field name itself.
fn field_completion_context(
    field_node: &tree_sitter::Node,
    content: &str,
    position: Position,
) -> CompletionContext {
    use super::ts_utils;

    let field_name_node = field_node.child(0);
    if let (Some(field_name), Some(value_node)) =
        (field_name_node, ts_utils::field_value(field_node))
    {
        // If cursor is after the field name, we're completing a value
        let name_end = ts_utils::node_end_position(&field_name, content);
        if position.line > name_end.line
            || (position.line == name_end.line && position.character > name_end.character)
        {
            let field = ts_utils::field_name(field_node, content).map(str::to_string);

            // Check if there's already some text (might be completing a type)
            if let Some(val_text) = ts_utils::node_text(&value_node, content)
                && val_text
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
            {
                return CompletionContext::StructType(field);
            }

            return CompletionContext::FieldValue(field);
        }
    }

    CompletionContext::FieldName
}

/// Generate completions for a given type (already navigated to the innermost type).
/// Type context navigation happens before this call in [`navigation::navigate_type_contexts`],
/// invoked from `lsp/mod.rs`.
///
/// [`navigation::navigate_type_contexts`]: super::navigation::navigate_type_contexts
pub fn generate_completions_for_type(
    tree: &Tree,
    content: &str,
    position: Position,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<CompletionItem> {
    let context = get_completion_context(tree, content, position);

    match context {
        CompletionContext::FieldName => {
            generate_field_completions(tree, content, position, type_info, &analyzer)
        }
        CompletionContext::FieldValue(field) => {
            // Find the field we're completing the value for
            if let Some(field_name) = field.or_else(|| find_current_field(tree, content, position))
            {
                let completions =
                    generate_value_completions_for_field(field_name, type_info, analyzer.clone());

                // Only fall back to offering every workspace type when the field
                // type couldn't be resolved to concrete value completions.
                // Otherwise the type-directed suggestions above are the right
                // (and only) ones — appending all types just buries them under,
                // e.g., unrelated struct constructors for a `bool` or enum field.
                if completions.is_empty() {
                    get_all_workspace_types(analyzer)
                } else {
                    completions
                }
            } else {
                get_all_workspace_types(analyzer)
            }
        }
        CompletionContext::StructType(field) => {
            // Find the field type and provide struct completions
            if let Some(field_name) = field.or_else(|| find_current_field(tree, content, position))
            {
                generate_type_completions_for_field(field_name, type_info, analyzer)
            } else {
                Vec::new()
            }
        }
    }
}

/// Get all types from the workspace as completion items
fn get_all_workspace_types(analyzer: Arc<RustAnalyzer>) -> Vec<CompletionItem> {
    analyzer
        .get_all_types()
        .into_iter()
        .map(create_type_completion)
        .collect()
}

/// Wrap Markdown text as LSP completion documentation.
fn markdown_docs(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

/// Build a completion item for a struct/variant field, labeled with the name
/// serde expects in the RON file.
fn field_completion(name: &str, field: &FieldInfo) -> CompletionItem {
    let signature = format!("```rust\n{}: {}\n```", name, field.type_name);
    let value = match &field.docs {
        Some(docs) => format!("{}\n\n{}", signature, docs),
        None => signature,
    };

    CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(field.type_name.clone()),
        documentation: Some(markdown_docs(value)),
        insert_text: Some(format!("{}: ", name)),
        ..Default::default()
    }
}

fn generate_field_completions(
    tree: &Tree,
    content: &str,
    position: Position,
    type_info: &TypeInfo,
    analyzer: &RustAnalyzer,
) -> Vec<CompletionItem> {
    // The fields serde accepts here, as `(serialized_name, field)` pairs: the
    // struct's own, or those of the enum variant the cursor is spelling out.
    let fields = match &type_info.kind {
        TypeKind::Struct(_) => analyzer.effective_fields(type_info),
        TypeKind::Enum(variants) => {
            match tree_sitter_parser::find_current_variant_context(tree, content, position)
                .and_then(|variant_name| type_info.find_variant(&variant_name))
            {
                Some(variant) => Cow::Owned(variant.effective_fields()),
                // Not inside a variant's fields: complete the variant names.
                None => return enum_variant_name_completions(type_info, variants),
            }
        }
    };

    // Offer only the fields not already written, under any name serde
    // accepts for them.
    let used_fields = tree_sitter_parser::extract_fields_from_ron(tree, content, position);
    fields
        .iter()
        .filter(|(name, field)| {
            !used_fields.contains(name)
                && !used_fields.contains(&field.name)
                && !field.aliases.iter().any(|a| used_fields.contains(a))
        })
        .map(|(name, field)| field_completion(name, field))
        .collect()
}

/// Completion items offering an enum's variant names where a variant is being
/// named, serialized (honoring rename/rename_all), with a snippet opening the
/// parens of a variant that carries data.
fn enum_variant_name_completions(
    type_info: &TypeInfo,
    variants: &[EnumVariant],
) -> Vec<CompletionItem> {
    let rename_all = type_info.rename_all.as_deref();
    variants
        .iter()
        .map(|variant| {
            let name = variant.serialized_name(rename_all);
            let value = match &variant.docs {
                Some(docs) => format!("```rust\n{}\n```\n\n{}", name, docs),
                None => format!("```rust\n{}\n```", name),
            };
            let insert_text = if variant.fields.is_empty() {
                name.to_string()
            } else {
                format!("{}($0)", name)
            };

            CompletionItem {
                label: name.into_owned(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(format!("Variant of {}", type_info.name)),
                documentation: Some(markdown_docs(value)),
                insert_text: Some(insert_text),
                ..Default::default()
            }
        })
        .collect()
}

/// Find the field name for the current cursor position using tree-sitter
fn find_current_field(tree: &Tree, content: &str, position: Position) -> Option<String> {
    tree_sitter_parser::get_field_at_position(tree, content, position)
}

/// Generate value completions for a specific field
fn generate_value_completions_for_field(
    field_name: String,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<CompletionItem> {
    // Find the field in the type info (by serialized or Rust name)
    if let Some(field) = type_info.find_field_serialized(&field_name) {
        return generate_value_completions_by_type(&field.type_name, analyzer);
    }

    Vec::new()
}

/// Generate type completions for a field that expects a custom type
fn generate_type_completions_for_field(
    field_name: String,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<CompletionItem> {
    // Find the field in the type info (by serialized or Rust name)
    if let Some(field) = type_info.find_field_serialized(&field_name) {
        // Get the inner type if it's a generic
        let inner_type = strip_outer_generic(&field.type_name);

        // Try to get type info for this type
        if let Some(nested_type) = analyzer.get_type_info(&inner_type) {
            // An enum field's value is a variant name, which the syntactic
            // context detection can't tell apart from a struct type name (both
            // are bare identifiers). Offer the variants rather than the enum
            // type itself, matching the `FieldValue` path.
            if let TypeKind::Enum(variants) = &nested_type.kind {
                return enum_variant_value_completions(nested_type, variants);
            }
            return vec![create_type_completion(nested_type)];
        }
    }

    Vec::new()
}

/// Completion items offering an enum's variants as RON values, using serialized
/// names (honoring `#[serde(rename)]` / `rename_all`).
fn enum_variant_value_completions(
    type_info: &TypeInfo,
    variants: &[EnumVariant],
) -> Vec<CompletionItem> {
    let rename_all = type_info.rename_all.as_deref();
    variants
        .iter()
        .map(|variant| {
            let name = variant.serialized_name(rename_all).into_owned();
            CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(format!("Variant of {}", type_info.name)),
                documentation: variant.docs.as_ref().map(|docs| markdown_docs(docs.clone())),
                insert_text: Some(name),
                ..Default::default()
            }
        })
        .collect()
}

/// Create a completion item for a type (struct or enum)
fn create_type_completion(type_info: &TypeInfo) -> CompletionItem {
    let type_name = short_name(&type_info.name);

    match &type_info.kind {
        TypeKind::Struct(fields) => {
            // Generate a snippet for the struct with all fields (serialized names)
            let rename_all = type_info.rename_all.as_deref();
            let field_snippets: Vec<String> = fields
                .iter()
                .filter(|f| !f.skip)
                .enumerate()
                .map(|(i, f)| format!("    {}: ${{{}}}", f.serialized_name(rename_all), i + 1))
                .collect();

            let snippet = if field_snippets.is_empty() {
                format!("{}()", type_name)
            } else {
                format!("{}(\n{},\n)", type_name, field_snippets.join(",\n"))
            };

            CompletionItem {
                label: type_name.to_string(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(format!("struct {}", type_info.name)),
                documentation: type_info.docs.as_ref().map(|docs| markdown_docs(docs.clone())),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        }
        TypeKind::Enum(_variants) => {
            // For enums, just provide the type name - variants will be suggested separately
            CompletionItem {
                label: type_name.to_string(),
                kind: Some(CompletionItemKind::ENUM),
                detail: Some(format!("enum {}", type_info.name)),
                documentation: type_info.docs.as_ref().map(|docs| markdown_docs(docs.clone())),
                insert_text: Some(format!("{}($0)", type_name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        }
    }
}

/// A completion offering a RON value, inserted exactly as written.
fn value_item(
    label: impl Into<String>,
    detail: impl Into<String>,
    insert_text: impl Into<String>,
) -> CompletionItem {
    CompletionItem {
        label: label.into(),
        kind: Some(CompletionItemKind::VALUE),
        detail: Some(detail.into()),
        insert_text: Some(insert_text.into()),
        ..Default::default()
    }
}

/// A [`value_item`] whose insert text is a snippet, with tab stops (`$0`,
/// `$1`, ...) for the editor to put the caret at.
fn snippet_item(
    label: impl Into<String>,
    detail: impl Into<String>,
    insert_text: impl Into<String>,
) -> CompletionItem {
    CompletionItem {
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..value_item(label, detail, insert_text)
    }
}

/// Generate value completions based on field type
fn generate_value_completions_by_type(
    field_type: &str,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<CompletionItem> {
    // Clean up the type string (remove spaces)
    let clean_type = normalize_type(field_type);

    // First check if this is a custom type (struct or enum) in the workspace:
    // an enum's values are its variants (serialized names), a struct's is the
    // struct itself, as a snippet.
    if let Some(type_info) = analyzer.get_type_info(field_type) {
        return match &type_info.kind {
            TypeKind::Enum(variants) => enum_variant_value_completions(type_info, variants),
            TypeKind::Struct(_) => vec![create_type_completion(type_info)],
        };
    }

    // A wrapper the document writes straight through — `Box<T>`, `Cow<T>`,
    // `NonZero<T>` — takes exactly the values its inner type does.
    if let Some(inner) = TRANSPARENT_WRAPPERS
        .iter()
        .find_map(|w| extract_inner_type(&clean_type, w))
    {
        return generate_value_completions_by_type(inner, analyzer);
    }

    // Check for generic types and try to provide completions for the inner type
    if clean_type.starts_with("Option<") {
        let inner = extract_inner_type(&clean_type, "Option<").unwrap_or(&clean_type);
        let some = match analyzer.get_type_info(inner) {
            Some(type_info) => {
                let name = short_name(&type_info.name);
                snippet_item(
                    format!("Some({name})"),
                    "Some variant with nested type",
                    format!("Some({name}($0))"),
                )
            }
            None => snippet_item("Some()", "Some variant", "Some($0)"),
        };
        return vec![some, value_item("None", "None variant", "None")];
    }

    // Handle primitive types
    if clean_type == "bool" {
        vec![
            value_item("true", "Boolean value", "true"),
            value_item("false", "Boolean value", "false"),
        ]
    } else if clean_type == "char" {
        vec![snippet_item("''", "Character value", "'$0'")]
    } else if clean_type.starts_with(['(', '[']) {
        // A tuple and a fixed-size array are both written as a parenthesized
        // list of exactly as many values as the type declares — brackets
        // belong to the sequence types, and `ron` refuses them here, so the
        // `[]`/`[...]` these fields used to be offered could not be used.
        let arity = tuple_element_types(&clean_type).map(|elements| elements.len());
        vec![match arity {
            // The unit type `()` takes exactly one value, and it is its own
            // spelling.
            Some(0) => value_item("()", "Unit value", "()"),
            // One tab stop per position, so the value can be filled in
            // left to right without counting the commas out by hand.
            Some(count) => snippet_item(
                "(...)",
                format!("{field_type} as a tuple of {count}"),
                format!(
                    "({})",
                    (1..=count)
                        .map(|stop| format!("${stop}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            // An arity the type name doesn't spell out — a slice, or an array
            // whose length is a const generic. Still a tuple in RON.
            None => snippet_item("(...)", format!("{field_type} as a tuple"), "($0)"),
        }]
    } else if SEQUENCE_WRAPPERS
        .iter()
        .chain(["HashSet<", "BTreeSet<"].iter())
        .any(|prefix| clean_type.starts_with(prefix))
    {
        // A set is written as a sequence too, so it takes the same values.
        vec![
            value_item("[]", "Empty vector/array", "[]"),
            snippet_item("[...]", "Vector/array with elements", "[$0]"),
        ]
    } else if clean_type.starts_with("HashMap<") || clean_type.starts_with("BTreeMap<") {
        vec![
            value_item("{}", "Empty map", "{}"),
            snippet_item("{...}", "Map with entries", "{$0}"),
        ]
    } else if let Some(std) = std_type(&clean_type) {
        // A standard-library type the analyzer never registers. There is no
        // definition to offer a snippet from, but its RON spelling is known,
        // and offering it is what keeps a `PathBuf` field from falling through
        // to "every type in the workspace".
        vec![match std.shape {
            StdShape::Text => {
                // A type whose placeholder spells out a value — an `IpAddr`'s
                // `"0.0.0.0"` — offers it pre-selected, as a starting point to
                // edit rather than a value anybody wants as is. One with
                // nothing to seed (`PathBuf`) just puts the caret in the
                // quotes, exactly as a `String` field does.
                let seed = std.placeholder.trim_matches('"');
                snippet_item(
                    std.placeholder,
                    format!("{field_type} as a string"),
                    match seed.is_empty() {
                        true => "\"$0\"".to_string(),
                        false => format!("\"${{1:{seed}}}\""),
                    },
                )
            }
            StdShape::Integer => value_item(
                std.placeholder,
                format!("{field_type} value"),
                std.placeholder,
            ),
            StdShape::Record => value_item(
                std.placeholder,
                format!("{field_type} as serde writes it"),
                std.placeholder,
            ),
        }]
    } else if clean_type == "String" || clean_type == "&str" || clean_type == "str" {
        // `str` reaches here through a wrapper: a `Cow<'a, str>` field is
        // written as the string it borrows. Leaving it out sent those fields to
        // the "every type in the workspace" fallback.
        vec![snippet_item("\"\"", "String value", "\"$0\"")]
    } else if clean_type.starts_with("i")
        || clean_type.starts_with("u")
        || clean_type.starts_with("f")
    {
        // Numeric types (i8, i16, i32, i64, u8, u16, u32, u64, f32, f64)
        vec![value_item("0", format!("{field_type} value"), "0")]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_analyzer::{EnumVariant, FieldInfo, TypeInfo, TypeKind};

    #[tokio::test]
    async fn test_enum_variant_field_completion() {
        // Create a mock enum with a struct variant
        let variant = EnumVariant {
            name: "StructVariant".to_string(),
            fields: vec![
                FieldInfo {
                    name: "field_a".to_string(),
                    type_name: "String".to_string(),
                    docs: Some("Field A documentation".to_string()),
                    line: Some(10),
                    column: Some(8),
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "field_b".to_string(),
                    type_name: "i32".to_string(),
                    docs: None,
                    line: Some(11),
                    column: Some(8),
                    has_default: false,
                    ..Default::default()
                },
            ],
            docs: Some("A struct variant".to_string()),
            line: Some(9),
            column: Some(4),
            ..Default::default()
        };

        let type_info = TypeInfo {
            name: "MyEnum".to_string(),
            kind: TypeKind::Enum(vec![variant]),
            docs: None,
            source_file: None,
            line: Some(8),
            column: Some(0),
            has_default: false,
            ..Default::default()
        };

        // Test content with a struct variant (RON uses parentheses)
        let content = "MyEnum::StructVariant(\n    \n)";
        let position = Position::new(1, 4); // Inside the parens

        let analyzer = std::sync::Arc::new(crate::rust_analyzer::RustAnalyzer::new());
        let tree = crate::lsp::ts_utils::RonParser::new().parse(content).unwrap();
        let completions =
            generate_completions_for_type(&tree, content, position, &type_info, analyzer);

        // Should complete with variant fields
        assert!(!completions.is_empty());
        let field_labels: Vec<String> = completions.iter().map(|c| c.label.clone()).collect();
        assert!(field_labels.contains(&"field_a".to_string()));
        assert!(field_labels.contains(&"field_b".to_string()));

        // Check that field_a has documentation
        let field_a = completions.iter().find(|c| c.label == "field_a").unwrap();
        assert!(field_a.documentation.is_some());
        if let Some(Documentation::MarkupContent(content)) = &field_a.documentation {
            assert!(content.value.contains("Field A documentation"));
        }
    }

    /// A field declared as a standard-library type the analyzer never
    /// registers used to yield no value completion at all, so the caller fell
    /// back to offering every type in the workspace — a list of structs where
    /// a path or an address belongs. Its RON spelling is known even though its
    /// definition is not, and that is what gets offered.
    #[test]
    fn test_std_type_value_completions() {
        let analyzer = std::sync::Arc::new(crate::rust_analyzer::RustAnalyzer::new());
        let labels = |type_name: &str| -> Vec<String> {
            generate_value_completions_by_type(type_name, analyzer.clone())
                .into_iter()
                .map(|c| c.label)
                .collect()
        };

        assert_eq!(labels("PathBuf"), vec!["\"\""]);
        assert_eq!(labels("std::path::PathBuf"), vec!["\"\""]);
        assert_eq!(labels("IpAddr"), vec!["\"0.0.0.0\""]);
        assert_eq!(labels("SocketAddr"), vec!["\"0.0.0.0:0\""]);
        assert_eq!(labels("Ipv6Addr"), vec!["\"::\""]);
        assert_eq!(labels("NonZeroU32"), vec!["1"]);
        assert_eq!(labels("Duration"), vec!["(secs: 0, nanos: 0)"]);

        // A wrapper the document writes straight through offers what the type
        // it wraps does — including `str`, which only ever arrives that way and
        // which the string branch used to miss.
        assert_eq!(labels("Cow<'static, str>"), vec!["\"\""]);
        assert_eq!(labels("Box<PathBuf>"), vec!["\"\""]);
        assert_eq!(labels("Arc<RwLock<Duration>>"), vec!["(secs: 0, nanos: 0)"]);

        // `NonZero<u32>` is `NonZeroU32` under its other spelling, and must not
        // be offered the `0` a bare `u32` would be: `ron` refuses it.
        assert_eq!(labels("NonZero<u32>"), vec!["1"]);
        assert_eq!(labels("std::num::NonZero<u32>"), vec!["1"]);

        // The sequences that were missing from the wrapper list take the array
        // spelling, like a `Vec`.
        assert_eq!(labels("VecDeque<String>"), vec!["[]", "[...]"]);
        assert_eq!(labels("BinaryHeap<u8>"), vec!["[]", "[...]"]);

        // The snippet puts the caret where the value goes: inside the quotes
        // for a path, over the seed value for an address.
        let inserted = |type_name: &str| -> Option<String> {
            generate_value_completions_by_type(type_name, analyzer.clone())
                .first()?
                .insert_text
                .clone()
        };
        assert_eq!(inserted("PathBuf").as_deref(), Some("\"$0\""));
        assert_eq!(inserted("IpAddr").as_deref(), Some("\"${1:0.0.0.0}\""));
    }

    #[tokio::test]
    async fn test_enum_variant_completion() {
        // Create a mock enum with multiple variants
        let variant1 = EnumVariant {
            name: "UnitVariant".to_string(),
            fields: vec![],
            docs: Some("A unit variant".to_string()),
            line: Some(9),
            column: Some(4),
            ..Default::default()
        };

        let variant2 = EnumVariant {
            name: "TupleVariant".to_string(),
            fields: vec![FieldInfo {
                name: "0".to_string(),
                type_name: "i32".to_string(),
                docs: None,
                line: None,
                column: None,
                has_default: false,
                ..Default::default()
            }],
            docs: Some("A tuple variant".to_string()),
            line: Some(10),
            column: Some(4),
            ..Default::default()
        };

        let type_info = TypeInfo {
            name: "MyEnum".to_string(),
            kind: TypeKind::Enum(vec![variant1, variant2]),
            docs: None,
            source_file: None,
            line: Some(8),
            column: Some(0),
            has_default: false,
            ..Default::default()
        };

        // Test in FieldName context - should get variant completions
        let content = "";
        let position = Position::new(0, 0);

        let analyzer = std::sync::Arc::new(crate::rust_analyzer::RustAnalyzer::new());
        let tree = crate::lsp::ts_utils::RonParser::new().parse(content).unwrap();
        let completions =
            generate_completions_for_type(&tree, content, position, &type_info, analyzer);

        // Should complete with variant names
        assert!(!completions.is_empty());
        let variant_labels: Vec<String> = completions.iter().map(|c| c.label.clone()).collect();
        assert!(variant_labels.contains(&"UnitVariant".to_string()));
        assert!(variant_labels.contains(&"TupleVariant".to_string()));
    }

    #[test]
    fn test_enum_field_value_completes_variants_not_type_name() {
        // A struct field whose type is an enum. When the user starts typing the
        // value (e.g. `mode: P`), the syntactic context is `StructType`, but the
        // value they want is an enum variant, not the enum's type name.
        let parent = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "mode".to_string(),
                type_name: "ServerMode".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let enum_type = TypeInfo {
            name: "ServerMode".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Development".to_string(),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Production".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let mut analyzer = crate::rust_analyzer::RustAnalyzer::new();
        analyzer.add_type(parent.clone());
        analyzer.add_type(enum_type);
        let analyzer = std::sync::Arc::new(analyzer);

        let completions = generate_type_completions_for_field("mode".to_string(), &parent, analyzer);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

        assert!(
            labels.contains(&"Development") && labels.contains(&"Production"),
            "enum variants should be offered as values: {:?}",
            labels
        );
        assert!(
            !labels.contains(&"ServerMode"),
            "the enum type name is not a valid value and should not be offered: {:?}",
            labels
        );
    }

    #[test]
    fn test_field_value_completion_excludes_unrelated_types() {
        // Completing an enum-typed field's value should offer that enum's
        // variants only — not every other struct/enum registered in the
        // workspace. Regression test: the FieldValue arm used to append
        // `get_all_workspace_types` even when the field type resolved.
        let parent = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "mode".to_string(),
                type_name: "ServerMode".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let enum_type = TypeInfo {
            name: "ServerMode".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Development".to_string(),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Production".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        // An unrelated type that must not leak into the suggestions.
        let unrelated = TypeInfo {
            name: "OtherConfig".to_string(),
            kind: TypeKind::Struct(vec![]),
            ..Default::default()
        };

        let mut analyzer = crate::rust_analyzer::RustAnalyzer::new();
        analyzer.add_type(parent.clone());
        analyzer.add_type(enum_type);
        analyzer.add_type(unrelated);
        let analyzer = std::sync::Arc::new(analyzer);

        // A quoted value puts the cursor in `FieldValue` (not `StructType`)
        // context, since the value text isn't a bare identifier.
        let content = "Config(\n    mode: \"\"\n)";
        let position = Position::new(1, 11); // inside the quotes, after `mode: `
        let tree = crate::lsp::ts_utils::RonParser::new().parse(content).unwrap();
        let completions =
            generate_completions_for_type(&tree, content, position, &parent, analyzer);
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();

        assert!(
            labels.contains(&"Development") && labels.contains(&"Production"),
            "enum variants should be offered: {:?}",
            labels
        );
        assert!(
            !labels.contains(&"OtherConfig") && !labels.contains(&"Config"),
            "unrelated workspace types should not be appended: {:?}",
            labels
        );
    }

    /// Value completion keys off the field's declared type, which a crate that
    /// doesn't import its standard-library types spells with the module path.
    /// Matching the bare name only meant none of these fields got the values
    /// they take; they fell through to the "every registered type" fallback.
    #[test]
    fn test_value_completion_for_qualified_std_field_types() {
        let analyzer = std::sync::Arc::new(crate::rust_analyzer::RustAnalyzer::new());
        let labels = |type_name: &str| -> Vec<String> {
            generate_value_completions_by_type(type_name, analyzer.clone())
                .iter()
                .map(|c| c.label.clone())
                .collect()
        };

        for (type_name, expected) in [
            (
                "std::collections::HashMap<String, u32>",
                vec!["{}", "{...}"],
            ),
            (
                "std::collections::BTreeMap<String, u32>",
                vec!["{}", "{...}"],
            ),
            // A set is a sequence, so it takes the same values as a `Vec`.
            ("std::collections::HashSet<String>", vec!["[]", "[...]"]),
            ("HashSet<String>", vec!["[]", "[...]"]),
            ("std::vec::Vec<String>", vec!["[]", "[...]"]),
            ("std::option::Option<u64>", vec!["Some()", "None"]),
            ("std::string::String", vec!["\"\""]),
        ] {
            assert_eq!(labels(type_name), expected, "type: {type_name}");
        }
    }

    /// A tuple field and a fixed-size array field are both written as a
    /// parenthesized tuple in RON — brackets belong to the sequence types, and
    /// `ron` refuses them here. A tuple field used to yield no values at all
    /// and fall through to the "every registered type" fallback, and an array
    /// field was offered the `[]`/`[...]` that cannot be used.
    #[test]
    fn test_value_completion_for_tuples_arrays_and_chars() {
        let mut analyzer = crate::rust_analyzer::RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Server".to_string(),
            kind: TypeKind::Struct(Vec::new()),
            ..Default::default()
        });
        let analyzer = std::sync::Arc::new(analyzer);

        let completions = |type_name: &str| -> Vec<(String, String)> {
            generate_value_completions_by_type(type_name, analyzer.clone())
                .iter()
                .map(|c| {
                    (
                        c.label.clone(),
                        c.insert_text.clone().unwrap_or_else(|| c.label.clone()),
                    )
                })
                .collect()
        };

        // One tab stop per position, so the value can be filled in left to
        // right without counting the commas out by hand.
        assert_eq!(
            completions("(String, u16)"),
            vec![("(...)".to_string(), "($1, $2)".to_string())]
        );
        assert_eq!(
            completions("[u32; 3]"),
            vec![("(...)".to_string(), "($1, $2, $3)".to_string())]
        );
        // The unit type takes the one value it has.
        assert_eq!(
            completions("()"),
            vec![("()".to_string(), "()".to_string())]
        );
        // An arity the type name doesn't spell out is still a tuple.
        assert_eq!(
            completions("[u8; LEN]"),
            vec![("(...)".to_string(), "($0)".to_string())]
        );
        assert_eq!(
            completions("char"),
            vec![("''".to_string(), "'$0'".to_string())]
        );
        // A sequence type keeps the bracket spelling.
        assert_eq!(
            completions("Vec<u8>")
                .into_iter()
                .map(|(label, _)| label)
                .collect::<Vec<_>>(),
            vec!["[]", "[...]"]
        );
    }

    #[test]
    fn test_serde_renamed_field_completion() {
        let type_info = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "max_connections".to_string(),
                    type_name: "u32".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "config_kind".to_string(),
                    type_name: "String".to_string(),
                    rename: Some("kind".to_string()),
                    ..Default::default()
                },
                FieldInfo {
                    name: "runtime_state".to_string(),
                    type_name: "String".to_string(),
                    skip: true,
                    ..Default::default()
                },
            ]),
            rename_all: Some("camelCase".to_string()),
            ..Default::default()
        };

        let content = "(\n    \n)";
        let position = Position::new(1, 4);
        let analyzer = std::sync::Arc::new(crate::rust_analyzer::RustAnalyzer::new());
        let tree = crate::lsp::ts_utils::RonParser::new().parse(content).unwrap();
        let completions =
            generate_completions_for_type(&tree, content, position, &type_info, analyzer);

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"maxConnections"), "got: {:?}", labels);
        assert!(labels.contains(&"kind"), "got: {:?}", labels);
        assert!(
            !labels.contains(&"max_connections") && !labels.contains(&"config_kind"),
            "Rust names should not be suggested: {:?}",
            labels
        );
        assert!(
            !labels.contains(&"runtimeState") && !labels.contains(&"runtime_state"),
            "Skipped fields should not be suggested: {:?}",
            labels
        );
    }

    #[test]
    fn test_serde_renamed_variant_completion() {
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
            rename_all: Some("kebab-case".to_string()),
            ..Default::default()
        };

        let content = "";
        let position = Position::new(0, 0);
        let analyzer = std::sync::Arc::new(crate::rust_analyzer::RustAnalyzer::new());
        let tree = crate::lsp::ts_utils::RonParser::new().parse(content).unwrap();
        let completions =
            generate_completions_for_type(&tree, content, position, &type_info, analyzer);

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"fast-mode"), "got: {:?}", labels);
        assert!(labels.contains(&"legacy"), "got: {:?}", labels);
    }

    /// `AppConfig { server: ServerConfig { host, mode: ServerMode } }`, which is
    /// enough nesting to exercise a cursor inside a struct that is itself a
    /// field value.
    fn nested_fixture() -> (TypeInfo, Arc<RustAnalyzer>) {
        let server = TypeInfo {
            name: "ServerConfig".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "host".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "mode".to_string(),
                    type_name: "ServerMode".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let mode = TypeInfo {
            name: "ServerMode".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Development".to_string(),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Production".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let root = TypeInfo {
            name: "AppConfig".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "debug".to_string(),
                    type_name: "bool".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "server".to_string(),
                    type_name: "ServerConfig".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(root);
        analyzer.add_type(server.clone());
        analyzer.add_type(mode);
        (server, Arc::new(analyzer))
    }

    fn labels_at(
        content: &str,
        position: Position,
        type_info: &TypeInfo,
        analyzer: Arc<RustAnalyzer>,
    ) -> Vec<String> {
        let tree = crate::lsp::ts_utils::RonParser::new().parse(content).unwrap();
        generate_completions_for_type(&tree, content, position, type_info, analyzer)
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    #[test]
    fn test_field_names_completed_inside_nested_struct() {
        // Regression test: a nested struct is the value of the field that holds
        // it, so a cursor inside its parentheses used to be read as that outer
        // field's value position. Field name completion then fell through to
        // offering every type in the workspace instead of the nested struct's
        // own fields.
        let (server, analyzer) = nested_fixture();
        let content =
            "AppConfig(\n    server: ServerConfig(\n        host: \"x\",\n        \n    ),\n)\n";

        let labels = labels_at(content, Position::new(3, 8), &server, analyzer);

        assert!(
            labels.contains(&"mode".to_string()),
            "the nested struct's own fields should be offered: {:?}",
            labels
        );
        assert!(
            !labels.contains(&"ServerConfig".to_string())
                && !labels.contains(&"AppConfig".to_string()),
            "workspace types are not field names: {:?}",
            labels
        );
    }

    #[test]
    fn test_value_still_completed_inside_nested_struct() {
        // The flip side of the test above: inside the same nested struct, a
        // cursor past a field's colon is a value position, and the enum field's
        // variants are what belongs there.
        let (server, analyzer) = nested_fixture();
        let content = "AppConfig(\n    server: ServerConfig(\n        host: \"x\",\n        mode: \n    ),\n)\n";

        let labels = labels_at(content, Position::new(3, 14), &server, analyzer);

        assert!(
            labels.contains(&"Production".to_string()),
            "the enum field's variants should be offered: {:?}",
            labels
        );
        assert!(
            !labels.contains(&"host".to_string()),
            "a value position is not a field name position: {:?}",
            labels
        );
    }

    #[test]
    fn test_struct_type_completed_at_end_of_line() {
        // The cursor an editor sends sits at the end of what has been typed,
        // where the deepest tree node is the enclosing struct rather than the
        // value. The field being given a value has to come from the struct's
        // children, or the nested type never gets offered.
        let (_, analyzer) = nested_fixture();
        let root = analyzer.get_type_info("AppConfig").unwrap().clone();

        for content in [
            "AppConfig(\n    debug: true,\n    server: Serv\n)\n",
            "AppConfig(\n    debug: true,\n    server: Serv,\n)\n",
        ] {
            let labels = labels_at(content, Position::new(2, 16), &root, analyzer.clone());
            assert!(
                labels.contains(&"ServerConfig".to_string()),
                "the field's declared type should be offered for {:?}: {:?}",
                content,
                labels
            );
        }
    }

    /// The "already written" filter has to be read off the same value the
    /// offered fields come from: the innermost one. Reading it off the root
    /// value hid a nested field whose name the root also uses and re-offered
    /// one the nested value had already written; see
    /// <https://git.cilki.net/cilki/roniker/issues/101>.
    #[test]
    fn test_nested_field_filter_uses_the_nested_values_field_names() {
        let database = TypeInfo {
            name: "DatabaseConfig".to_string(),
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
                FieldInfo {
                    name: "username".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        // The root value has a `port` of its own, as config roots routinely do.
        let root = TypeInfo {
            name: "Config".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "port".to_string(),
                    type_name: "u16".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "database".to_string(),
                    type_name: "DatabaseConfig".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(root);
        analyzer.add_type(database.clone());
        let analyzer = Arc::new(analyzer);

        let content = "Config(\n    port: 8080,\n    database: DatabaseConfig(\n        host: \"localhost\",\n        \n    ),\n)\n";
        // The blank line inside DatabaseConfig's parentheses.
        let labels = labels_at(content, Position::new(4, 8), &database, analyzer);

        assert!(
            labels.contains(&"username".to_string()),
            "a field neither value has written should be offered: {:?}",
            labels
        );
        assert!(
            labels.contains(&"port".to_string()),
            "the root value's `port` must not mask the nested one: {:?}",
            labels
        );
        assert!(
            !labels.contains(&"host".to_string()),
            "a field this value has already written must not be offered again: {:?}",
            labels
        );
    }

    /// The same filter inside an enum variant's fields: the variant value is
    /// the innermost struct, so what it has written is what gets filtered, not
    /// what the root value has.
    #[test]
    fn test_variant_field_filter_uses_the_variants_field_names() {
        let kind = TypeInfo {
            name: "Kind".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "Detailed".to_string(),
                fields: vec![
                    FieldInfo {
                        name: "name".to_string(),
                        type_name: "String".to_string(),
                        ..Default::default()
                    },
                    FieldInfo {
                        name: "length".to_string(),
                        type_name: "u32".to_string(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let root = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "kind".to_string(),
                    type_name: "Kind".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(root);
        analyzer.add_type(kind.clone());
        let analyzer = Arc::new(analyzer);

        let content =
            "Post(\n    name: \"x\",\n    kind: Detailed(\n        length: 1,\n        \n    ),\n)\n";
        let labels = labels_at(content, Position::new(4, 8), &kind, analyzer);

        assert_eq!(labels, vec!["name".to_string()]);
    }

    #[test]
    fn test_field_names_still_completed_at_top_level() {
        let (_, analyzer) = nested_fixture();
        let root = analyzer.get_type_info("AppConfig").unwrap().clone();
        let content = "AppConfig(\n    debug: true,\n    \n)\n";

        let labels = labels_at(content, Position::new(2, 4), &root, analyzer);

        assert!(
            labels.contains(&"server".to_string()),
            "unused top-level fields should be offered: {:?}",
            labels
        );
        assert!(
            !labels.contains(&"debug".to_string()),
            "fields already present should not be offered: {:?}",
            labels
        );
    }
}
