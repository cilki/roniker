use super::tree_sitter_parser;
use crate::rust_analyzer::{FieldInfo, RustAnalyzer, TypeInfo, TypeKind};
use std::sync::Arc;
use tower_lsp::lsp_types::*;
use tree_sitter::Tree;

/// Generate all code actions for a RON document
pub fn generate_code_actions(
    tree: &Tree,
    content: &str,
    type_info: &TypeInfo,
    url: &Url,
    analyzer: Arc<RustAnalyzer>,
    context_diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();

    // Quick-fixes for diagnostics in the requested range (remove unknown/duplicate fields)
    actions.extend(generate_remove_field_actions(
        tree,
        content,
        context_diagnostics,
        url,
    ));

    // Add actions for making struct names explicit
    actions.extend(generate_explicit_type_actions(tree, content, type_info, url));

    // Add actions for missing fields (handles both structs and enum variants)
    actions.extend(generate_missing_field_actions(
        tree, content, type_info, url, &analyzer,
    ));

    // Also check for nested enum variant fields
    actions.extend(generate_missing_variant_field_actions(
        tree, content, type_info, url, &analyzer,
    ));

    actions
}

/// Quick-fixes that delete fields flagged as `unknown-field` or
/// `duplicate-field` by the published diagnostics.
pub fn generate_remove_field_actions(
    tree: &Tree,
    content: &str,
    diagnostics: &[Diagnostic],
    url: &Url,
) -> Vec<CodeActionOrCommand> {
    use super::diagnostics::codes;
    use super::ts_utils;

    let mut actions = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for diag in diagnostics {
        let removable = matches!(
            &diag.code,
            Some(NumberOrString::String(c))
                if c == codes::UNKNOWN_FIELD || c == codes::DUPLICATE_FIELD
        );
        if !removable {
            continue;
        }

        let offset = ts_utils::position_to_byte_offset(content, diag.range.start);
        let Some(node) = tree.root_node().descendant_for_byte_range(offset, offset) else {
            continue;
        };
        let Some(field_node) = (if node.kind() == "field" {
            Some(node)
        } else {
            ts_utils::find_ancestor_by_kind(node, "field")
        }) else {
            continue;
        };
        let Some(name) = ts_utils::field_name(&field_node, content) else {
            continue;
        };

        // Delete through the trailing comma if present
        let start = field_node.start_position();
        let start_byte = field_node.start_byte();
        let mut end = field_node.end_position();
        let mut end_byte = field_node.end_byte();
        if let Some(next) = field_node.next_sibling()
            && next.kind() == ","
        {
            end = next.end_position();
            end_byte = next.end_byte();
        }

        // If the field occupies its line(s) alone, remove the whole lines
        let line_prefix = lines
            .get(start.row)
            .map(|l| &l[..start.column.min(l.len())])
            .unwrap_or("");
        let line_suffix = lines
            .get(end.row)
            .map(|l| &l[end.column.min(l.len())..])
            .unwrap_or("");
        let range = if line_prefix.trim().is_empty() && line_suffix.trim().is_empty() {
            Range::new(
                Position::new(start.row as u32, 0),
                Position::new(end.row as u32 + 1, 0),
            )
        } else {
            Range::new(
                ts_utils::byte_offset_to_position(content, start_byte, start.row),
                ts_utils::byte_offset_to_position(content, end_byte, end.row),
            )
        };

        actions.push(single_file_action(
            url,
            format!("Remove field '{}'", name),
            CodeActionKind::QUICKFIX,
            vec![TextEdit {
                range,
                new_text: String::new(),
            }],
            Some(vec![diag.clone()]),
        ));
    }

    actions
}

/// Generate code actions for adding missing fields in nested enum variants
fn generate_missing_variant_field_actions(
    tree: &Tree,
    content: &str,
    type_info: &TypeInfo,
    url: &Url,
    analyzer: &RustAnalyzer,
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();
    let variant_locations = tree_sitter_parser::find_all_variant_field_locations(tree, content);

    // Group locations by (containing_field_name, variant_name) to find which fields are used
    let mut variant_fields_map: std::collections::HashMap<
        (String, String),
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();

    // Collect all fields used for each variant
    for location in &variant_locations {
        if let Some(ref field_at_pos) = location.field_at_position {
            let key = (
                location.containing_field_name.clone(),
                location.variant_name.clone(),
            );
            variant_fields_map
                .entry(key)
                .or_default()
                .insert(field_at_pos.clone());
        }
    }

    // Now generate actions for each unique variant
    let mut seen_variants = std::collections::HashSet::new();
    for location in variant_locations {
        let key = (
            location.containing_field_name.clone(),
            location.variant_name.clone(),
        );
        if seen_variants.contains(&key) {
            continue;
        }
        seen_variants.insert(key.clone());

        // Resolve the containing field directly on the root type; deeper nesting
        // is not currently handled here
        let Some(field) = type_info.find_field_serialized(&location.containing_field_name) else {
            continue;
        };

        // Get the enum type for this field and find the variant definition
        let Some(field_type_info) = analyzer.get_type_info(&field.type_name) else {
            continue;
        };
        let Some(variant) = field_type_info.find_variant(&location.variant_name) else {
            continue;
        };

        // Get fields used in this variant from our map
        let used_fields = variant_fields_map.get(&key).cloned().unwrap_or_default();

        let required_missing = if field_type_info.has_default {
            Vec::new()
        } else {
            super::type_utils::missing_required_fields(&variant.effective_fields(), |name| {
                used_fields.contains(name)
            })
        };

        if !required_missing.is_empty()
            && let Some(edits) = generate_field_insertions(tree, content, &required_missing)
        {
            actions.push(single_file_action(
                url,
                format!(
                    "Add {} required field{} to {}::{}",
                    required_missing.len(),
                    plural(required_missing.len()),
                    location.containing_field_name,
                    location.variant_name
                ),
                CodeActionKind::QUICKFIX,
                edits,
                None,
            ));
        }
    }

    actions
}

/// Generate code actions for making implicit struct names explicit
fn generate_explicit_type_actions(
    tree: &Tree,
    content: &str,
    type_info: &TypeInfo,
    url: &Url,
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();

    // Check if the root level uses unnamed struct syntax
    if let Some(action) = create_explicit_root_type_action(tree, content, type_info, url) {
        actions.push(action);
    }

    // Check for nested unnamed structs in field values
    if let Some(fields) = type_info.fields() {
        let rename_all = type_info.rename_all.as_deref();
        for field in fields {
            if let Some(action) =
                create_explicit_field_type_action(tree, content, field, rename_all, url)
            {
                actions.push(action);
            }
        }
    }

    actions
}

/// Generate code actions for adding the fields missing from the document root,
/// for both a struct root and an enum root — see [`expected_root_fields`], which
/// is the only thing that differs between them.
fn generate_missing_field_actions(
    tree: &Tree,
    content: &str,
    type_info: &TypeInfo,
    url: &Url,
    analyzer: &RustAnalyzer,
) -> Vec<CodeActionOrCommand> {
    let Some((effective_fields, target)) = expected_root_fields(tree, content, type_info, analyzer)
    else {
        return Vec::new();
    };

    let ron_fields = tree_sitter_parser::extract_fields_from_ron(tree, content);
    let present = |name: &str| ron_fields.iter().any(|f| f == name);

    let all_missing: Vec<_> = effective_fields
        .iter()
        .filter(|(name, field)| !present(name) && !present(&field.name))
        .cloned()
        .collect();
    // A type that derives Default needs none of its fields spelled out.
    let required_missing = if type_info.has_default {
        Vec::new()
    } else {
        super::type_utils::missing_required_fields(&effective_fields, present)
    };

    [
        ("Add", "required", required_missing),
        ("Add all", "missing", all_missing),
    ]
    .into_iter()
    .filter(|(_, _, fields)| !fields.is_empty())
    .filter_map(|(lead, kind, fields)| {
        let edits = generate_field_insertions(tree, content, &fields)?;
        Some(single_file_action(
            url,
            format!(
                "{lead} {} {kind} field{}{target}",
                fields.len(),
                plural(fields.len())
            ),
            CodeActionKind::QUICKFIX,
            edits,
            None,
        ))
    })
    .collect()
}

/// The fields the document root is expected to carry, paired with the suffix
/// that names them in a code action's title.
///
/// A struct root is compared against its own fields, an enum root against the
/// fields of whichever variant the document names; that is the whole difference
/// between the two, so everything downstream can ignore it.
///
/// `None` when there is nothing to compare against: an enum root that names no
/// known variant — an unknown-variant error rather than a set of absent fields —
/// or a tuple/newtype, whose fields are positional (`"0"`, `"1"`, ...) and so
/// have no name that could be inserted.
fn expected_root_fields(
    tree: &Tree,
    content: &str,
    type_info: &TypeInfo,
    analyzer: &RustAnalyzer,
) -> Option<(Vec<(String, FieldInfo)>, String)> {
    let (fields, target) = match &type_info.kind {
        TypeKind::Enum(_) => {
            let name = root_variant_name(tree, content)?;
            let variant = type_info.find_variant(&name)?;
            (variant.effective_fields(), format!(" to {}", name))
        }
        TypeKind::Struct(_) => (type_info.effective_fields(analyzer), String::new()),
    };

    match fields.iter().any(|(_, field)| field.is_positional()) {
        true => None,
        false => Some((fields, target)),
    }
}

/// The variant name a root enum value spells out — `Prod` for both the unit
/// `Prod` and `Prod(...)`. Read off the parse tree, the same way the diagnostics
/// for that value read it.
fn root_variant_name(tree: &Tree, content: &str) -> Option<String> {
    let main_value = super::ts_utils::find_main_value(tree)?;
    super::ts_utils::extract_enum_variant(&main_value, content).map(|variant| variant.name)
}

/// Create action to make root-level struct name explicit using tree-sitter
/// Converts: `(field: value)` → `StructName(field: value)`
fn create_explicit_root_type_action(
    tree: &Tree,
    content: &str,
    type_info: &TypeInfo,
    url: &Url,
) -> Option<CodeActionOrCommand> {
    use super::ts_utils;

    let main_value = ts_utils::find_main_value(tree)?;

    if main_value.kind() == "struct" && ts_utils::struct_name(&main_value, content).is_none() {
        let type_name = super::type_utils::short_name(&type_info.name);
        let pos = ts_utils::node_start_position(&main_value, content);

        return Some(single_file_action(
            url,
            format!("Make struct name explicit: {}", type_name),
            CodeActionKind::REFACTOR,
            vec![TextEdit {
                range: Range::new(pos, pos),
                new_text: type_name.to_string(),
            }],
            None,
        ));
    }

    None
}

/// Create action to make nested field type explicit using tree-sitter
/// Converts: `foo: (value)` → `foo: TypeName(value)`
fn create_explicit_field_type_action(
    tree: &Tree,
    content: &str,
    field: &FieldInfo,
    rename_all: Option<&str>,
    url: &Url,
) -> Option<CodeActionOrCommand> {
    use super::ts_utils;

    let main_value = ts_utils::find_main_value(tree)?;

    if main_value.kind() == "struct" {
        let field_nodes = ts_utils::struct_fields(&main_value);
        let serialized = field.serialized_name(rename_all);

        for field_node in field_nodes {
            if let Some(field_name) = ts_utils::field_name(&field_node, content)
                && (field_name == serialized || field_name == field.name)
                && let Some(value_node) = ts_utils::field_value(&field_node)
                && value_node.kind() == "struct"
                && ts_utils::struct_name(&value_node, content).is_none()
            {
                let short = super::type_utils::short_name(&field.type_name);
                let type_name = super::type_utils::normalize_type(short);
                let clean_type = super::type_utils::extract_inner_type(&type_name, "Option<")
                    .unwrap_or(&type_name);

                let pos = ts_utils::node_start_position(&value_node, content);
                return Some(single_file_action(
                    url,
                    format!("Make field type explicit: {} {}", field.name, clean_type),
                    CodeActionKind::REFACTOR,
                    vec![TextEdit {
                        range: Range::new(pos, pos),
                        new_text: clean_type.to_string(),
                    }],
                    None,
                ));
            }
        }
    }

    None
}

/// Generate text edits to insert missing fields using tree-sitter.
/// Takes `(serialized_name, field)` pairs so inserted names match what serde expects.
///
/// The edits are anchored on the last thing inside the struct's parens rather
/// than on the closing paren itself, so a separator is only emitted when the
/// previous field doesn't already end in one. Anchoring on the closing paren
/// instead produced `a: 1,\n,\n    b: 0` for any document whose last field
/// carries a trailing comma — which is every document this server's own
/// formatter emits.
fn generate_field_insertions(
    tree: &Tree,
    content: &str,
    missing_fields: &[(String, FieldInfo)],
) -> Option<Vec<TextEdit>> {
    use super::ts_utils;

    let root = tree.root_node();

    // `find_main_value` returns the first named, non-annotation child of the
    // root, which already includes any `struct` or `ERROR` node. If it finds
    // nothing, there is no struct to insert into, so bail out.
    let main_value = ts_utils::find_main_value(tree)?;

    if main_value.kind() != "struct" && main_value.kind() != "ERROR" {
        return None;
    }

    // For ERROR nodes, the struct is likely a SIBLING, not a child
    let struct_node = if main_value.kind() == "ERROR" {
        // Look for struct node among root's children
        ts_utils::child_by_kind(&root, "struct")?
    } else {
        main_value
    };

    let open = ts_utils::child_by_kind(&struct_node, "(")?;
    // Tree-sitter supplies a zero-width `MISSING ")"` node while the struct is
    // still unterminated, so this also resolves for a half-typed document.
    let close = ts_utils::child_by_kind(&struct_node, ")")?;

    // Anchor on the last node inside the parens, ignoring comments, so the edit
    // lands after the final field instead of on the closing paren's line.
    let mut cursor = struct_node.walk();
    let anchor = struct_node
        .children(&mut cursor)
        .take_while(|child| child.id() != close.id())
        .filter(|child| !ts_utils::is_comment(child))
        .last()
        .unwrap_or(open);

    // A separator is needed unless the struct is empty or already ends in one.
    let needs_separator = !matches!(anchor.kind(), "(" | ",");

    // Keep a struct that already fits on one line on one line.
    let on_one_line = !content
        .get(open.end_byte()..close.start_byte())
        .unwrap_or_default()
        .contains('\n');

    let anchor_pos = anchor.end_position();
    let separator_pos = ts_utils::node_end_position(&anchor, content);

    let mut body = String::new();
    if on_one_line {
        if needs_separator {
            body.push(' ');
        }
        let rendered: Vec<String> = missing_fields
            .iter()
            .map(|(name, field)| format!("{}: {}", name, generate_default_value(&field.type_name)))
            .collect();
        body.push_str(&rendered.join(", "));
    } else {
        let indent = field_indent(content, &struct_node);
        for (name, field) in missing_fields {
            body.push('\n');
            body.push_str(&indent);
            body.push_str(name);
            body.push_str(": ");
            body.push_str(&generate_default_value(&field.type_name));
            body.push(',');
        }
    }

    // Step over whatever is left on the anchor's line — a trailing comment, say
    // — so the new fields start on a line of their own instead of landing
    // inside it. The separator still has to go right after the anchor, which is
    // why this can need two edits.
    let bytes = content.as_bytes();
    let anchor_end = anchor.end_byte();
    let mut body_byte = anchor_end;
    if !on_one_line {
        let limit = close.start_byte().max(anchor_end);
        while body_byte < limit && bytes[body_byte] != b'\n' {
            body_byte += 1;
        }
    }
    // `body_byte` never crosses a newline, so it is still on the anchor's row.
    let body_pos = ts_utils::byte_offset_to_position(content, body_byte, anchor_pos.row);

    let separator = if needs_separator { "," } else { "" };
    if body_pos == separator_pos {
        return Some(vec![TextEdit {
            range: Range::new(body_pos, body_pos),
            new_text: format!("{separator}{body}"),
        }]);
    }

    let mut edits = Vec::new();
    if needs_separator {
        edits.push(TextEdit {
            range: Range::new(separator_pos, separator_pos),
            new_text: separator.to_string(),
        });
    }
    edits.push(TextEdit {
        range: Range::new(body_pos, body_pos),
        new_text: body,
    });
    Some(edits)
}

/// The indentation to put in front of a field inserted into `struct_node`:
/// copied from the struct's own fields when it has any on a line of their own,
/// and otherwise one level deeper than the line the struct starts on.
fn field_indent(content: &str, struct_node: &tree_sitter::Node) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let leading = |row: usize| -> String {
        lines
            .get(row)
            .map(|line| {
                line.chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect()
            })
            .unwrap_or_default()
    };

    let own_row = struct_node.start_position().row;
    match super::ts_utils::struct_fields(struct_node)
        .iter()
        .find(|field| field.start_position().row != own_row)
    {
        Some(field) => leading(field.start_position().row),
        None => leading(own_row) + "    ",
    }
}

/// Generate a default value for a given Rust type
fn generate_default_value(type_name: &str) -> String {
    let clean = super::type_utils::normalize_type(type_name);

    if clean.starts_with("Option") {
        "None".to_string()
    } else if clean == "bool" {
        "false".to_string()
    } else if clean.starts_with("Vec") || clean.starts_with("[") {
        "[]".to_string()
    } else if clean.starts_with("HashMap") || clean.starts_with("BTreeMap") {
        "{}".to_string()
    } else if clean == "String" || clean == "&str" || clean == "str" {
        "\"\"".to_string()
    } else if is_numeric_type(&clean) {
        // Numeric types default to zero
        "0".to_string()
    } else {
        // Custom type - use constructor notation with placeholder
        format!("{}()", clean)
    }
}

/// Whether a cleaned type name is a Rust numeric primitive.
fn is_numeric_type(clean: &str) -> bool {
    matches!(
        clean,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
    )
}

/// Plural suffix for a count: `""` for exactly one, `"s"` otherwise.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Build a `CodeAction` that applies `edits` to a single document (`url`).
///
/// Every code action in this module targets one file, so they all wrap their
/// edits in the same single-entry `changes` map and `WorkspaceEdit`; this
/// centralizes that boilerplate.
fn single_file_action(
    url: &Url,
    title: String,
    kind: CodeActionKind,
    edits: Vec<TextEdit>,
    diagnostics: Option<Vec<Diagnostic>>,
) -> CodeActionOrCommand {
    let mut changes = std::collections::HashMap::new();
    changes.insert(url.clone(), edits);

    CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(kind),
        diagnostics,
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Tree {
        super::super::ts_utils::RonParser::new()
            .parse(content)
            .unwrap()
    }
    use crate::rust_analyzer::TypeKind;

    #[test]
    fn test_generate_default_value() {
        assert_eq!(generate_default_value("bool"), "false");
        assert_eq!(generate_default_value("String"), "\"\"");
        assert_eq!(generate_default_value("Option<u32>"), "None");
        assert_eq!(generate_default_value("Vec<u8>"), "[]");
        assert_eq!(generate_default_value("HashMap<String, u32>"), "{}");

        // Every numeric primitive defaults to 0, including the pointer-sized
        // integers whose names contain non-`iuf` letters.
        for numeric in [
            "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
            "f32", "f64",
        ] {
            assert_eq!(generate_default_value(numeric), "0", "type: {numeric}");
        }

        // Unknown/custom types fall back to constructor notation.
        assert_eq!(generate_default_value("Server"), "Server()");
    }

    #[test]
    fn test_explicit_root_type_same_line() {
        let content = "(id: 1, name: \"test\")";
        let type_info = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        let action = create_explicit_root_type_action(&parse(content), content, &type_info, &url);
        assert!(action.is_some());

        if let Some(CodeActionOrCommand::CodeAction(action)) = action {
            assert_eq!(action.title, "Make struct name explicit: User");
            let edit = action.edit.unwrap();
            let changes = edit.changes.unwrap();
            let text_edits = changes.values().next().unwrap();
            assert_eq!(text_edits.len(), 1);
            assert_eq!(text_edits[0].new_text, "User");
            assert_eq!(text_edits[0].range.start.line, 0);
            assert_eq!(text_edits[0].range.start.character, 0);
        }
    }

    #[test]
    fn test_explicit_root_type_next_line() {
        let content = "(\n    id: 1,\n    name: \"test\"\n)";
        let type_info = TypeInfo {
            name: "example::User".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        let action = create_explicit_root_type_action(&parse(content), content, &type_info, &url);
        assert!(action.is_some());

        if let Some(CodeActionOrCommand::CodeAction(action)) = action {
            assert_eq!(action.title, "Make struct name explicit: User");
            let edit = action.edit.unwrap();
            let changes = edit.changes.unwrap();
            let text_edits = changes.values().next().unwrap();
            assert_eq!(text_edits.len(), 1);
            assert_eq!(text_edits[0].new_text, "User");
            assert_eq!(text_edits[0].range.start.line, 0);
            assert_eq!(text_edits[0].range.start.character, 0);
        }
    }

    #[test]
    fn test_explicit_field_type_same_line() {
        let content = "User(author: (id: 1, name: \"test\"))";
        let field = FieldInfo {
            name: "author".to_string(),
            type_name: "Author".to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        let action =
            create_explicit_field_type_action(&parse(content), content, &field, None, &url);
        assert!(action.is_some());

        if let Some(CodeActionOrCommand::CodeAction(action)) = action {
            assert_eq!(action.title, "Make field type explicit: author Author");
            let edit = action.edit.unwrap();
            let changes = edit.changes.unwrap();
            let text_edits = changes.values().next().unwrap();
            assert_eq!(text_edits.len(), 1);
            assert_eq!(text_edits[0].new_text, "Author");
            // Should insert before the opening paren after "author: "
            assert_eq!(text_edits[0].range.start.line, 0);
            assert_eq!(text_edits[0].range.start.character, 13); // position of '(' after "author: "
        }
    }

    #[test]
    fn test_explicit_field_type_next_line() {
        let content = r#"Post(
    author: (
        id: 5,
        name: "Charlie",
        email: "charlie@example.com"
    )
)"#;
        let field = FieldInfo {
            name: "author".to_string(),
            type_name: "User".to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        let action =
            create_explicit_field_type_action(&parse(content), content, &field, None, &url);
        assert!(action.is_some());

        if let Some(CodeActionOrCommand::CodeAction(action)) = action {
            assert_eq!(action.title, "Make field type explicit: author User");
            let edit = action.edit.unwrap();
            let changes = edit.changes.unwrap();
            let text_edits = changes.values().next().unwrap();
            assert_eq!(text_edits.len(), 1);
            assert_eq!(text_edits[0].new_text, "User");
            // Should insert at the opening paren on line 1
            assert_eq!(text_edits[0].range.start.line, 1);
            assert_eq!(text_edits[0].range.start.character, 12); // position of '(' on second line
        }
    }

    #[test]
    fn test_explicit_field_type_with_option_wrapper() {
        let content = "User(author: (id: 1, name: \"test\"))";
        let field = FieldInfo {
            name: "author".to_string(),
            type_name: "Option<Author>".to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        let action =
            create_explicit_field_type_action(&parse(content), content, &field, None, &url);
        assert!(action.is_some());

        if let Some(CodeActionOrCommand::CodeAction(action)) = action {
            assert_eq!(action.title, "Make field type explicit: author Author");
            let edit = action.edit.unwrap();
            let changes = edit.changes.unwrap();
            let text_edits = changes.values().next().unwrap();
            assert_eq!(text_edits[0].new_text, "Author"); // Should strip Option wrapper
        }
    }

    #[test]
    fn test_no_action_for_explicit_type() {
        let content = "User(id: 1, name: \"test\")";
        let type_info = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        // Should not offer action since type is already explicit
        let action = create_explicit_root_type_action(&parse(content), content, &type_info, &url);
        assert!(action.is_none());
    }

    #[test]
    fn test_no_action_for_explicit_field_type() {
        let content = "User(author: Author(id: 1, name: \"test\"))";
        let field = FieldInfo {
            name: "author".to_string(),
            type_name: "Author".to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        // Should not offer action since field type is already explicit
        let action =
            create_explicit_field_type_action(&parse(content), content, &field, None, &url);
        assert!(action.is_none());
    }

    #[test]
    fn test_real_world_with_comments_and_spacing() {
        let content = r#"Post(
    id: 123,
    title: "Mixed Syntax Example",
    content: "Demonstrating both explicit and unnamed struct syntax",

    // Explicit type name for author
    author: (
        id: 5,
        name: "Charlie",
        email: "charlie@example.com",
        age: 28,
        bio: None,
        is_active: true,
        roles: ["editor"],
    ),

    likes: 50,
    tags: ["example", "syntax"],
    published: true,
    post_type: Short,
)"#;
        let field = FieldInfo {
            name: "author".to_string(),
            type_name: "User".to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        let action =
            create_explicit_field_type_action(&parse(content), content, &field, None, &url);
        assert!(action.is_some());

        if let Some(CodeActionOrCommand::CodeAction(action)) = action {
            assert_eq!(action.title, "Make field type explicit: author User");
            let edit = action.edit.unwrap();
            let changes = edit.changes.unwrap();
            let text_edits = changes.values().next().unwrap();
            assert_eq!(text_edits.len(), 1);
            assert_eq!(text_edits[0].new_text, "User");
            // The opening paren is on line 6 (0-indexed), after the comment
            assert_eq!(text_edits[0].range.start.line, 6);
            // The opening paren is at column 12 (after "    author: ")
            assert_eq!(text_edits[0].range.start.character, 12);

            println!(
                "Line: {}, Character: {}",
                text_edits[0].range.start.line, text_edits[0].range.start.character
            );
        }
    }

    #[test]
    fn test_enum_variant_missing_fields() {
        use crate::rust_analyzer::EnumVariant;

        let variant = EnumVariant {
            name: "StructVariant".to_string(),
            fields: vec![
                FieldInfo {
                    name: "field_a".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
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
            docs: None,
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

        // How RON actually spells an enum value: the bare variant name. The
        // `Enum::Variant(...)` form this used to be written with is a RON syntax
        // error, so it never exercised the feature on a real document.
        let content = "StructVariant(\n    field_a: \"test\"\n)";
        let url = Url::parse("file:///test.ron").unwrap();

        // Create mock analyzer and client for the test
        use crate::rust_analyzer::RustAnalyzer;
        use std::sync::Arc;

        let analyzer = Arc::new(RustAnalyzer::new());

        let actions =
            generate_code_actions(&parse(content), content, &type_info, &url, analyzer, &[]);

        // Should suggest adding missing field_b
        assert!(!actions.is_empty());

        let titles: Vec<String> = actions
            .iter()
            .filter_map(|a| {
                if let CodeActionOrCommand::CodeAction(action) = a {
                    Some(action.title.clone())
                } else {
                    None
                }
            })
            .collect();

        // Should have action mentioning the variant name
        assert!(
            titles.iter().any(|t| t.contains("StructVariant")),
            "got: {titles:?}"
        );
        assert!(
            titles.iter().any(|t| t.contains("field")),
            "got: {titles:?}"
        );
    }

    /// The struct and enum roots share one code path, so the fields a root enum
    /// variant is missing must be inserted exactly as a struct's would be.
    #[test]
    fn test_enum_variant_missing_field_insertion_matches_struct() {
        use crate::rust_analyzer::EnumVariant;

        let fields = vec![
            FieldInfo {
                name: "a".to_string(),
                type_name: "u32".to_string(),
                ..Default::default()
            },
            FieldInfo {
                name: "b".to_string(),
                type_name: "String".to_string(),
                ..Default::default()
            },
        ];
        let as_enum = TypeInfo {
            name: "Mode".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "Prod".to_string(),
                fields: fields.clone(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let as_struct = TypeInfo {
            name: "Prod".to_string(),
            kind: TypeKind::Struct(fields),
            ..Default::default()
        };

        let content = "Prod(\n    a: 1,\n)";
        let url = Url::parse("file:///test.ron").unwrap();
        let analyzer = Arc::new(RustAnalyzer::new());

        let edit_sets = |type_info: &TypeInfo| -> Vec<Vec<TextEdit>> {
            generate_code_actions(
                &parse(content),
                content,
                type_info,
                &url,
                analyzer.clone(),
                &[],
            )
            .iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) if a.title.contains("field") => {
                    Some(a.edit.as_ref()?.changes.as_ref()?.get(&url)?.clone())
                }
                _ => None,
            })
            .collect()
        };

        let from_enum = edit_sets(&as_enum);
        assert!(
            !from_enum.is_empty(),
            "a root enum variant's missing fields should be offered"
        );
        assert_eq!(
            from_enum,
            edit_sets(&as_struct),
            "the enum root's edits diverged from the struct root's"
        );
        for edits in &from_enum {
            assert_eq!(apply(content, edits), "Prod(\n    a: 1,\n    b: \"\",\n)");
        }
    }

    /// A tuple variant's fields are positional, so there is no name to insert and
    /// the action must not be offered at all.
    #[test]
    fn test_no_missing_field_action_for_tuple_variant() {
        use crate::rust_analyzer::EnumVariant;

        let type_info = TypeInfo {
            name: "Mode".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "Port".to_string(),
                fields: vec![
                    FieldInfo {
                        name: "0".to_string(),
                        type_name: "u16".to_string(),
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

        let content = "Port(80)";
        let url = Url::parse("file:///test.ron").unwrap();
        let analyzer = Arc::new(RustAnalyzer::new());

        let actions =
            generate_code_actions(&parse(content), content, &type_info, &url, analyzer, &[]);
        let titles: Vec<&str> = actions
            .iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) => Some(a.title.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !titles.iter().any(|t| t.contains("field")),
            "positional fields have no name to insert: {titles:?}"
        );
    }

    #[test]
    fn test_remove_unknown_field_action() {
        let content = "(\n    name: \"a\",\n    bogus: 1,\n)";
        let url = Url::parse("file:///test.ron").unwrap();
        let diagnostic = Diagnostic {
            range: Range::new(Position::new(2, 4), Position::new(2, 9)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "Unknown field 'bogus'".to_string(),
            code: Some(NumberOrString::String("unknown-field".to_string())),
            ..Default::default()
        };

        let actions = generate_remove_field_actions(&parse(content), content, &[diagnostic], &url);
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected code action");
        };
        assert_eq!(action.title, "Remove field 'bogus'");

        let edits = &action.edit.as_ref().unwrap().changes.as_ref().unwrap()[&url];
        assert_eq!(edits.len(), 1);
        // The field is alone on its line: the whole line is removed
        assert_eq!(edits[0].new_text, "");
        assert_eq!(edits[0].range.start, Position::new(2, 0));
        assert_eq!(edits[0].range.end, Position::new(3, 0));
    }

    /// Edit ranges are columns in UTF-16 code units, not bytes. A non-ASCII
    /// string value earlier on the line used to shift every edit on that line
    /// one column right per extra byte, so applying the quick-fix ate the
    /// wrong text instead of the field it named.
    #[test]
    fn test_remove_field_action_columns_are_utf16() {
        let content = "(host: \"münchen\", bogus: 1, port: 80)";
        let url = Url::parse("file:///test.ron").unwrap();
        let start = content.chars().position(|c| c == 'b').unwrap() as u32;
        let diagnostic = Diagnostic {
            range: Range::new(Position::new(0, start), Position::new(0, start + 5)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "Unknown field 'bogus'".to_string(),
            code: Some(NumberOrString::String("unknown-field".to_string())),
            ..Default::default()
        };

        let actions = generate_remove_field_actions(&parse(content), content, &[diagnostic], &url);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected code action, got {actions:?}");
        };
        assert_eq!(action.title, "Remove field 'bogus'");

        let edits = &action.edit.as_ref().unwrap().changes.as_ref().unwrap()[&url];
        assert_eq!(
            apply(content, edits),
            // The separating space is left behind, as it is for an ASCII-only
            // document; what matters is that `bogus: 1,` is what went away.
            "(host: \"münchen\",  port: 80)",
            "the quick-fix must remove the field it named and nothing else"
        );
    }

    /// Insertions anchor on the last thing inside the parens, whose column is
    /// likewise reported in UTF-16 code units.
    #[test]
    fn test_field_insertion_columns_are_utf16() {
        assert_eq!(
            insert("(host: \"münchen\")", &[("port", "u16")]),
            "(host: \"münchen\", port: 0)"
        );
        assert_eq!(
            insert(
                "Config(\n    host: \"münchen\", // the office\n)",
                &[("port", "u16")]
            ),
            "Config(\n    host: \"münchen\", // the office\n    port: 0,\n)"
        );
    }

    #[test]
    fn test_remove_field_action_ignores_other_codes() {
        let content = "(name: \"a\")";
        let url = Url::parse("file:///test.ron").unwrap();
        let diagnostic = Diagnostic {
            range: Range::new(Position::new(0, 1), Position::new(0, 5)),
            message: "Type mismatch: expected u32".to_string(),
            code: Some(NumberOrString::String("type-mismatch".to_string())),
            ..Default::default()
        };

        let actions = generate_remove_field_actions(&parse(content), content, &[diagnostic], &url);
        assert!(actions.is_empty());
    }

    /// Apply single-document `TextEdit`s so tests can assert on the document a
    /// user would actually end up with. Edits are applied back to front, the
    /// way a client does it.
    fn apply(content: &str, edits: &[TextEdit]) -> String {
        let offset = |pos| super::super::ts_utils::position_to_byte_offset(content, pos);
        let mut ordered: Vec<&TextEdit> = edits.iter().collect();
        ordered.sort_by_key(|e| std::cmp::Reverse(offset(e.range.start)));

        let mut result = content.to_string();
        for edit in ordered {
            result.replace_range(
                offset(edit.range.start)..offset(edit.range.end),
                &edit.new_text,
            );
        }
        result
    }

    fn missing(names: &[(&str, &str)]) -> Vec<(String, FieldInfo)> {
        names
            .iter()
            .map(|(name, type_name)| {
                (
                    name.to_string(),
                    FieldInfo {
                        name: name.to_string(),
                        type_name: type_name.to_string(),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    /// Insert `fields` into `content` and return the resulting document,
    /// asserting it still parses as RON.
    fn insert(content: &str, fields: &[(&str, &str)]) -> String {
        let edits = generate_field_insertions(&parse(content), content, &missing(fields))
            .expect("expected an insertion edit");
        let result = apply(content, &edits);
        ron::from_str::<ron::Value>(&result)
            .unwrap_or_else(|e| panic!("insertion produced invalid RON ({e}):\n{result}"));
        result
    }

    #[test]
    fn test_insert_after_existing_trailing_comma() {
        // The formatter this server ships emits a trailing comma after the last
        // field, so this is the shape nearly every real document has.
        assert_eq!(
            insert("Cfg(\n    a: 1,\n)", &[("b", "u32")]),
            "Cfg(\n    a: 1,\n    b: 0,\n)"
        );
    }

    #[test]
    fn test_insert_adds_missing_separator() {
        assert_eq!(
            insert("Cfg(\n    a: 1\n)", &[("b", "u32")]),
            "Cfg(\n    a: 1,\n    b: 0,\n)"
        );
    }

    #[test]
    fn test_insert_multiple_fields() {
        assert_eq!(
            insert("Cfg(\n    a: 1,\n)", &[("b", "String"), ("c", "bool")]),
            "Cfg(\n    a: 1,\n    b: \"\",\n    c: false,\n)"
        );
    }

    #[test]
    fn test_insert_into_empty_struct() {
        assert_eq!(insert("Cfg(\n)", &[("b", "u32")]), "Cfg(\n    b: 0,\n)");
        assert_eq!(insert("Cfg()", &[("b", "u32")]), "Cfg(b: 0)");
    }

    #[test]
    fn test_insert_keeps_single_line_struct_on_one_line() {
        assert_eq!(insert("Cfg(a: 1)", &[("b", "u32")]), "Cfg(a: 1, b: 0)");
    }

    #[test]
    fn test_insert_after_trailing_comment() {
        // The comma must not land inside the comment.
        assert_eq!(
            insert("Cfg(\n    a: 1 // about a\n)", &[("b", "u32")]),
            "Cfg(\n    a: 1, // about a\n    b: 0,\n)"
        );
    }

    #[test]
    fn test_insert_matches_existing_indentation() {
        assert_eq!(
            insert("Cfg(\n\ta: 1,\n)", &[("b", "u32")]),
            "Cfg(\n\ta: 1,\n\tb: 0,\n)"
        );
        assert_eq!(
            insert("Cfg(\n        a: 1,\n)", &[("b", "u32")]),
            "Cfg(\n        a: 1,\n        b: 0,\n)"
        );
    }

    #[test]
    fn test_insert_into_unterminated_struct() {
        // Mid-edit documents have a zero-width MISSING ")" instead of a real
        // one; the insertion still belongs after the last field.
        let content = "Cfg(\n    a: 1,\n";
        let edits = generate_field_insertions(&parse(content), content, &missing(&[("b", "u32")]))
            .expect("expected an insertion edit");
        assert_eq!(apply(content, &edits), "Cfg(\n    a: 1,\n    b: 0,\n");
    }

    #[test]
    fn test_add_missing_fields_action_yields_valid_ron() {
        // End to end through the public entry point, on the document shape the
        // formatter produces.
        let content = "Cfg(\n    a: 1,\n)";
        let type_info = TypeInfo {
            name: "Cfg".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "a".to_string(),
                    type_name: "u32".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "b".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();
        let analyzer = Arc::new(RustAnalyzer::new());

        let actions =
            generate_code_actions(&parse(content), content, &type_info, &url, analyzer, &[]);
        let edit_sets: Vec<&Vec<TextEdit>> = actions
            .iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) if a.title.contains("field") => {
                    a.edit.as_ref()?.changes.as_ref()?.get(&url)
                }
                _ => None,
            })
            .collect();
        assert!(!edit_sets.is_empty(), "expected a missing-field action");
        for edits in edit_sets {
            assert_eq!(apply(content, edits), "Cfg(\n    a: 1,\n    b: \"\",\n)");
        }
    }
}
