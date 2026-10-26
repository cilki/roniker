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

    // Same again for every value nested below the root
    actions.extend(generate_nested_missing_field_actions(
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
            ts_utils::find_ancestor_by_kind(tree, node, "field")
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

/// Code actions for the required fields missing from any value nested *below*
/// the document root — the `ServerConfig(...)` of
/// `AppConfig(server: ServerConfig(...))` as much as the `Prod(...)` of
/// `Cfg(mode: Prod(...))`.
///
/// The root itself is [`generate_missing_field_actions`]'s job; this walks down
/// from it, resolving every value against the type the field it belongs to
/// declares, the way the validator does. The validator reports
/// `missing-required-field` at every depth, so anything this walk doesn't reach
/// is a diagnostic naming fields the user then has to type out by hand.
fn generate_nested_missing_field_actions(
    tree: &Tree,
    content: &str,
    type_info: &TypeInfo,
    url: &Url,
    analyzer: &RustAnalyzer,
) -> Vec<CodeActionOrCommand> {
    let Some(root) = root_struct_node(tree) else {
        return Vec::new();
    };
    let Some((root_fields, _)) = expected_root_fields(tree, content, type_info, analyzer) else {
        return Vec::new();
    };

    let mut actions = Vec::new();
    let walk = NestedWalk {
        content,
        url,
        analyzer,
    };
    walk.walk_struct(&root, &root_fields, "", MAX_NESTING, &mut actions);
    actions
}

/// How many levels below the root the nested walk descends. The walk is
/// recursive, and [`super::ts_utils::MAX_TREE_DEPTH`] alone is too generous a
/// bound to lean on here; nothing a configuration file legitimately contains
/// comes anywhere near this.
const MAX_NESTING: usize = 32;

/// The nested walk's immutable context: everything it needs that doesn't change
/// as it descends.
struct NestedWalk<'a> {
    content: &'a str,
    url: &'a Url,
    analyzer: &'a RustAnalyzer,
}

/// What a nested value is expected to contain, once the type its field declares
/// has been resolved.
struct NestedTarget {
    /// The names serde accepts, as `(serialized_name, field)` pairs — the same
    /// shape the root is compared against.
    fields: Vec<(String, FieldInfo)>,
    /// The container carries `#[serde(default)]`, so nothing is required of it.
    has_default: bool,
    /// How an action title names this value: a dotted field path, with `::` in
    /// front of an enum variant (`server.tls`, `mode::Prod`).
    path: String,
}

impl NestedWalk<'_> {
    /// Visit each named field of `node` — a value expected to contain
    /// `expected` — and descend into the field's value. `path` names `node`
    /// itself, and is empty for the document root.
    fn walk_struct(
        &self,
        node: &tree_sitter::Node,
        expected: &[(String, FieldInfo)],
        path: &str,
        depth: usize,
        actions: &mut Vec<CodeActionOrCommand>,
    ) {
        use super::ts_utils;

        if depth == 0 {
            return;
        }

        for field_node in ts_utils::struct_fields(node) {
            let Some(field_name) = ts_utils::field_name(&field_node, self.content) else {
                continue;
            };
            let Some(value) = ts_utils::field_value(&field_node) else {
                continue;
            };
            // A field the containing type doesn't have is already reported as
            // unknown; there is no type to validate its value against.
            let Some(field) = matching_field(expected, field_name) else {
                continue;
            };

            let child_path = match path.is_empty() {
                true => field_name.to_string(),
                false => format!("{path}.{field_name}"),
            };
            self.walk_value(&value, &field.type_name, &child_path, depth - 1, actions);
        }
    }

    /// Offer the required fields `value` is missing, then descend into it.
    ///
    /// `declared_type` is the type the field holding `value` declares. Array
    /// elements and the contents of a `Some(...)` are visited as values in their
    /// own right, so `Vec<T>` and `Option<T>` are walked the same as a plain
    /// `T`.
    fn walk_value(
        &self,
        value: &tree_sitter::Node,
        declared_type: &str,
        path: &str,
        depth: usize,
        actions: &mut Vec<CodeActionOrCommand>,
    ) {
        use super::ts_utils;

        if depth == 0 {
            return;
        }

        if value.kind() == "array" {
            let element_type = super::type_utils::strip_outer_generic(declared_type);
            for (index, element) in ts_utils::array_elements(value).iter().enumerate() {
                self.walk_value(
                    element,
                    &element_type,
                    &format!("{path}[{index}]"),
                    depth - 1,
                    actions,
                );
            }
            return;
        }

        // RON spells an inhabited `Option` as `Some(value)`. The field's type
        // describes the value inside, not the `Some` wrapper.
        if let Some(inner) = ts_utils::unwrap_some(value, self.content) {
            self.walk_value(&inner, declared_type, path, depth - 1, actions);
            return;
        }

        // `unit` is the empty unnamed struct `()`: a value whose fields are all
        // missing, and which has parens of its own to insert them into.
        if !matches!(value.kind(), "struct" | "unit") {
            return;
        }
        // A bare `None`, or any other unit variant, parses as a `struct` node
        // with nothing but its name. It supplies no fields and has nowhere to
        // insert any, so the type its field declares says nothing about it.
        if value.kind() == "struct" && ts_utils::child_by_kind(value, "(").is_none() {
            return;
        }
        let Some(target) = self.resolve(value, declared_type, path) else {
            return;
        };
        // Tuple structs and tuple variants have positional fields ("0", "1",
        // ...), which have no name that could be inserted.
        if target.fields.iter().any(|(_, field)| field.is_positional()) {
            return;
        }

        // Only the value's own fields count as supplied. Descending into them
        // would credit it with field names belonging to a nested type, which is
        // how a required field could be considered present because something
        // nested inside it happened to share its name.
        let present: std::collections::HashSet<&str> = ts_utils::struct_fields(value)
            .iter()
            .filter_map(|field| ts_utils::field_name(field, self.content))
            .collect();

        if !target.has_default {
            let missing = super::type_utils::missing_required_fields(&target.fields, |name| {
                present.contains(name)
            });
            if !missing.is_empty()
                // Into this value's own parens, not the document root's.
                && let Some(edits) =
                    generate_field_insertions(value, self.content, &missing, self.analyzer)
            {
                actions.push(single_file_action(
                    self.url,
                    format!(
                        "Add {} required field{} to {}",
                        missing.len(),
                        plural(missing.len()),
                        target.path
                    ),
                    CodeActionKind::QUICKFIX,
                    edits,
                    None,
                ));
            }
        }

        self.walk_struct(value, &target.fields, &target.path, depth - 1, actions);
    }

    /// What `value` is expected to contain, given the type its field declares.
    ///
    /// `None` when there is nothing to compare it against: a type the analyzer
    /// doesn't know (a `HashMap`, say), or an enum value that names no known
    /// variant — an unknown-variant error rather than a set of absent fields.
    fn resolve(
        &self,
        value: &tree_sitter::Node,
        declared_type: &str,
        path: &str,
    ) -> Option<NestedTarget> {
        // One generic layer is stripped so `Option<Server>` and `Vec<Server>`
        // resolve to `Server`, matching how the validator recurses.
        let info = self.analyzer.get_type_info(declared_type).or_else(|| {
            self.analyzer
                .get_type_info(&super::type_utils::strip_outer_generic(declared_type))
        })?;

        match &info.kind {
            TypeKind::Struct(_) => Some(NestedTarget {
                fields: info.effective_fields(self.analyzer),
                has_default: info.has_default,
                path: path.to_string(),
            }),
            TypeKind::Enum(_) => {
                // A variant with fields is spelled `Variant(...)`, which parses
                // as a `struct` node named after the variant.
                let variant_name = super::ts_utils::struct_name(value, self.content)?;
                let variant = info.find_variant(variant_name)?;
                Some(NestedTarget {
                    fields: variant.effective_fields(),
                    has_default: info.has_default,
                    path: format!("{path}::{variant_name}"),
                })
            }
        }
    }
}

/// The field of `expected` that serde would fill from a RON field named
/// `field_name`: the one whose serialized name — as the *containing* type spells
/// it — matches, or that accepts the name itself (its Rust name, an alias).
fn matching_field<'a>(
    expected: &'a [(String, FieldInfo)],
    field_name: &str,
) -> Option<&'a FieldInfo> {
    expected
        .iter()
        .find(|(name, field)| name == field_name || field.accepts_name(field_name, None))
        .map(|(_, field)| field)
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
    let Some(root) = root_struct_node(tree) else {
        return Vec::new();
    };

    let ron_fields = tree_sitter_parser::extract_fields_from_ron(tree, content);
    let present = |name: &str| ron_fields.iter().any(|f| f == name);

    let all_missing: Vec<_> = effective_fields
        .iter()
        .filter(|(name, field)| !present(name) && !present(&field.name))
        .cloned()
        .collect();
    // A container `#[serde(default)]` fills in whatever is absent, so none of
    // its fields need spelling out.
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
        let edits = generate_field_insertions(&root, content, &fields, analyzer)?;
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

/// The `struct` node holding the document's root value, i.e. the one whose
/// fields are compared against the root type.
///
/// `find_main_value` returns the first named, non-annotation child of the root,
/// which already includes any `struct` or `ERROR` node. A half-typed document
/// parses as an `ERROR` with the struct as its *sibling* rather than its child,
/// so that case looks one level up. A document that is just `()` parses as a
/// `unit`: an unnamed struct value with no field written, which is where the
/// fields the root type requires most need adding. `None` when the document's
/// root value is not a struct at all.
fn root_struct_node(tree: &Tree) -> Option<tree_sitter::Node<'_>> {
    use super::ts_utils;

    match ts_utils::find_main_value(tree)? {
        main_value if matches!(main_value.kind(), "struct" | "unit") => Some(main_value),
        main_value if main_value.kind() == "ERROR" => {
            ts_utils::child_by_kind(&tree.root_node(), "struct")
        }
        _ => None,
    }
}

/// Generate text edits to insert missing fields into `struct_node`'s parens.
/// Takes `(serialized_name, field)` pairs so inserted names match what serde expects.
///
/// The edits are anchored on the last thing inside the struct's parens rather
/// than on the closing paren itself, so a separator is only emitted when the
/// previous field doesn't already end in one. Anchoring on the closing paren
/// instead produced `a: 1,\n,\n    b: 0` for any document whose last field
/// carries a trailing comma — which is every document this server's own
/// formatter emits.
fn generate_field_insertions(
    struct_node: &tree_sitter::Node,
    content: &str,
    missing_fields: &[(String, FieldInfo)],
    analyzer: &RustAnalyzer,
) -> Option<Vec<TextEdit>> {
    use super::ts_utils;

    // `()` — an unnamed struct with no field written — is a single leaf token,
    // so there are no `(` and `)` children to anchor on. There is also nothing
    // inside it to separate the new fields from, which leaves one insertion
    // between its two characters, on the line it already occupies.
    if struct_node.kind() == "unit" {
        let inside = ts_utils::byte_offset_to_position(
            content,
            struct_node.start_byte() + 1,
            struct_node.start_position().row,
        );
        return Some(vec![TextEdit {
            range: Range::new(inside, inside),
            new_text: one_line_fields(missing_fields, analyzer),
        }]);
    }

    let open = ts_utils::child_by_kind(struct_node, "(")?;
    // Tree-sitter supplies a zero-width `MISSING ")"` node while the struct is
    // still unterminated, so this also resolves for a half-typed document.
    let close = ts_utils::child_by_kind(struct_node, ")")?;

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
        body.push_str(&one_line_fields(missing_fields, analyzer));
    } else {
        let indent = field_indent(content, struct_node);
        for (name, field) in missing_fields {
            body.push('\n');
            body.push_str(&indent);
            body.push_str(name);
            body.push_str(": ");
            body.push_str(&generate_default_value(&field.type_name, analyzer));
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

/// The missing fields written out for a struct that is staying on one line:
/// `name: value, name: value`.
fn one_line_fields(missing_fields: &[(String, FieldInfo)], analyzer: &RustAnalyzer) -> String {
    missing_fields
        .iter()
        .map(|(name, field)| {
            format!(
                "{}: {}",
                name,
                generate_default_value(&field.type_name, analyzer)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
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

/// How many layers of user-defined type the placeholder generator descends
/// through. A newtype wrapping a newtype is ordinary; a type that contains
/// itself (`struct Node(Box<Node>)` with the `Box` written out) is not, and the
/// recursion must not chase it forever.
const MAX_DEFAULT_DEPTH: usize = 4;

/// Generate a placeholder value for a field of the given Rust type.
///
/// The placeholder has to be something RON will actually parse, because the
/// whole point of the action is to save the user from typing it. For a named
/// struct `Server()` is enough — its own required fields come back as a
/// `missing-required-field` diagnostic with a nested "add fields" action to
/// finish the job. An enum and a tuple struct have no such follow-up, so they
/// have to be emitted complete:
///
/// - an enum names one of its variants, preferring a unit variant since that is
///   a whole value on its own. `Mode()` names no variant at all and came back as
///   an `unknown-variant` error with no fix offered.
/// - a tuple or newtype struct supplies its elements. `UserId()` is a `UserId`
///   missing its one element, which no diagnostic covers — serde rejects the
///   file the server called clean.
fn generate_default_value(type_name: &str, analyzer: &RustAnalyzer) -> String {
    generate_default_value_depth(type_name, analyzer, MAX_DEFAULT_DEPTH)
}

fn generate_default_value_depth(type_name: &str, analyzer: &RustAnalyzer, depth: usize) -> String {
    let clean = super::type_utils::normalize_type(type_name);

    if clean.starts_with("Option") {
        return "None".to_string();
    }
    if clean == "bool" {
        return "false".to_string();
    }
    // A set serializes as a sequence, the same as a `Vec`. A fixed-size array
    // does not, despite the brackets in its name: see the tuple branch below.
    if [
        "Vec",
        "VecDeque",
        "LinkedList",
        "BinaryHeap",
        "HashSet",
        "BTreeSet",
    ]
    .iter()
    .any(|prefix| clean.starts_with(prefix))
    {
        return "[]".to_string();
    }
    if clean.starts_with("HashMap") || clean.starts_with("BTreeMap") {
        return "{}".to_string();
    }
    if clean == "String" || clean == "&str" || clean == "str" {
        return "\"\"".to_string();
    }
    if clean == "char" {
        // A `char` is a quoted character, and the one thing it is not is
        // `char()` — which is what the opaque fallback below used to insert.
        return "'a'".to_string();
    }
    if is_numeric_type(&clean) {
        // Numeric types default to zero
        return "0".to_string();
    }
    // RON writes a tuple and a fixed-size array alike as a parenthesized list
    // of exactly as many values as the type declares, each filled in from the
    // type of its own position. Neither has a follow-up diagnostic to finish
    // the job from, so both have to be emitted complete — `(String,u16)()` is
    // not RON at all, and the `[]` a `[u32; 3]` used to get is RON that `ron`
    // then refuses to read.
    if let Some(element_types) = super::type_utils::tuple_element_types(&clean) {
        let next = depth.saturating_sub(1);
        let elements: Vec<String> = element_types
            .iter()
            .map(|element| generate_default_value_depth(element, analyzer, next))
            .collect();
        return format!("({})", elements.join(", "));
    }

    // A custom type is spelled by its last path segment: a field declared
    // `config::Server` is written `Server(...)` in RON.
    let short = super::type_utils::short_name(&clean).to_string();
    // Constructor notation with no contents, for a named struct whose own
    // required fields come back as a `missing-required-field` diagnostic with a
    // nested action to finish the job.
    let opaque = || format!("{short}()");
    // For a name the analyzer has no definition of at all: the value `ron`
    // reads a standard-library type from, since the analyzer was never going to
    // have one of those. `PathBuf()` is not a `PathBuf` — the very thing this
    // generator exists to avoid — and nor are `SocketAddr()` or `Duration()`.
    // Asked only once the analyzer has been, so a workspace type sharing a name
    // with one of them keeps its own constructor.
    let unresolved = || match super::type_utils::std_type(&clean) {
        Some(std) => std.placeholder.to_string(),
        None => opaque(),
    };

    if depth == 0 {
        return unresolved();
    }

    // Serde sees straight through a smart pointer, and so does RON: a
    // `Box<Server>` field is written exactly like a `Server` one.
    for wrapper in super::type_utils::TRANSPARENT_WRAPPERS {
        if let Some(inner) = super::type_utils::extract_inner_type(&clean, wrapper) {
            return generate_default_value_depth(inner, analyzer, depth - 1);
        }
    }

    let Some(info) = analyzer.get_type_info(&clean) else {
        return unresolved();
    };

    match &info.kind {
        TypeKind::Enum(variants) => {
            // A unit variant is a complete value by itself, so prefer one;
            // failing that, the first variant with its own elements supplied.
            let Some(variant) = variants
                .iter()
                .find(|variant| variant.fields.is_empty())
                .or_else(|| variants.first())
            else {
                // An enum with no variants has no inhabitant to name.
                return opaque();
            };
            let name = variant.serialized_name(info.rename_all.as_deref());
            match positional_elements(&variant.fields, analyzer, depth) {
                Some(elements) => format!("{name}({elements})"),
                None if variant.fields.is_empty() => name.into_owned(),
                // A struct variant's named fields are reported as missing and
                // can be added from there, as for a named struct.
                None => format!("{name}()"),
            }
        }
        TypeKind::Struct(fields) => match positional_elements(fields, analyzer, depth) {
            Some(elements) => format!("{short}({elements})"),
            None => opaque(),
        },
    }
}

/// The comma-separated placeholders for a tuple or newtype's elements, or `None`
/// when `fields` are named (or absent) and so aren't written positionally.
fn positional_elements(
    fields: &[FieldInfo],
    analyzer: &RustAnalyzer,
    depth: usize,
) -> Option<String> {
    if fields.is_empty() || !fields.iter().all(|field| field.is_positional()) {
        return None;
    }
    Some(
        fields
            .iter()
            .map(|field| generate_default_value_depth(&field.type_name, analyzer, depth - 1))
            .collect::<Vec<_>>()
            .join(", "),
    )
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
    use crate::rust_analyzer::{EnumVariant, TypeKind};

    #[test]
    fn test_generate_default_value() {
        let analyzer = RustAnalyzer::new();
        let default_value = |type_name: &str| generate_default_value(type_name, &analyzer);

        assert_eq!(default_value("bool"), "false");
        assert_eq!(default_value("String"), "\"\"");
        assert_eq!(default_value("Option<u32>"), "None");
        assert_eq!(default_value("Vec<u8>"), "[]");
        assert_eq!(default_value("HashMap<String, u32>"), "{}");
        // A set is written as a sequence, not as a `HashSet(...)` call.
        assert_eq!(default_value("HashSet<String>"), "[]");
        assert_eq!(default_value("BTreeSet<u8>"), "[]");

        // Every numeric primitive defaults to 0, including the pointer-sized
        // integers whose names contain non-`iuf` letters.
        for numeric in [
            "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
            "f32", "f64",
        ] {
            assert_eq!(default_value(numeric), "0", "type: {numeric}");
        }

        // Unknown/custom types fall back to constructor notation, spelled the
        // way RON spells a type: by its last path segment.
        assert_eq!(default_value("Server"), "Server()");
        assert_eq!(default_value("config::Server"), "Server()");
    }

    /// A field that names a std type through its module path used to miss
    /// every prefix check here and fall through to constructor notation, so
    /// the quick-fix inserted `HashMap<String,u32>()` — which RON cannot
    /// parse. Each of these has to produce the same placeholder as the bare
    /// spelling does.
    #[test]
    fn test_generate_default_value_for_qualified_std_types() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "crate::config::Server".to_string(),
            kind: TypeKind::Struct(vec![]),
            ..Default::default()
        });
        let default_value = |type_name: &str| generate_default_value(type_name, &analyzer);

        assert_eq!(
            default_value("std::collections::HashMap<String, u32>"),
            "{}"
        );
        assert_eq!(
            default_value("std::collections::BTreeMap<String, u32>"),
            "{}"
        );
        assert_eq!(default_value("std::collections::HashSet<String>"), "[]");
        assert_eq!(default_value("std::vec::Vec<String>"), "[]");
        assert_eq!(default_value("std::option::Option<u64>"), "None");
        assert_eq!(default_value("std::string::String"), "\"\"");
        // A smart pointer is seen through to the type it wraps, qualified or
        // not.
        assert_eq!(default_value("std::boxed::Box<Server>"), "Server()");

        // Each one has to be something RON will actually parse.
        for type_name in [
            "std::collections::HashMap<String, u32>",
            "std::collections::HashSet<String>",
            "std::vec::Vec<String>",
            "std::option::Option<u64>",
            "std::string::String",
        ] {
            let document = format!("Cfg(field: {})", default_value(type_name));
            ron::from_str::<ron::Value>(&document).unwrap_or_else(|e| {
                panic!("placeholder for {type_name} is not valid RON ({e}): {document}")
            });
        }
    }

    /// An enum field's placeholder has to name a variant. `Mode()` names none of
    /// them, so the quick-fix used to hand back a document carrying an
    /// `unknown-variant` error with no further fix on offer.
    #[test]
    fn test_generate_default_value_for_enum_names_a_variant() {
        let variant = |name: &str, fields: Vec<FieldInfo>| EnumVariant {
            name: name.to_string(),
            fields,
            ..Default::default()
        };
        let positional = |type_name: &str| FieldInfo {
            name: "0".to_string(),
            type_name: type_name.to_string(),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        // A unit variant is a whole value on its own, so it wins even though a
        // tuple variant comes first.
        analyzer.add_type(TypeInfo {
            name: "Mode".to_string(),
            kind: TypeKind::Enum(vec![
                variant("Wait", vec![positional("u64")]),
                variant("Prod", vec![]),
            ]),
            ..Default::default()
        });
        // With no unit variant to pick, the first one is supplied in full.
        analyzer.add_type(TypeInfo {
            name: "Delay".to_string(),
            kind: TypeKind::Enum(vec![variant("Wait", vec![positional("u64")])]),
            ..Default::default()
        });
        // A struct variant's named fields come back as missing-required-field,
        // with a nested action to add them — as for a named struct.
        analyzer.add_type(TypeInfo {
            name: "Send".to_string(),
            kind: TypeKind::Enum(vec![variant(
                "To",
                vec![FieldInfo {
                    name: "addr".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                }],
            )]),
            ..Default::default()
        });
        // `rename_all` decides the name serde expects, so it decides the name
        // the placeholder spells.
        analyzer.add_type(TypeInfo {
            name: "Level".to_string(),
            kind: TypeKind::Enum(vec![variant("VeryLoud", vec![])]),
            rename_all: Some("snake_case".to_string()),
            ..Default::default()
        });

        assert_eq!(generate_default_value("Mode", &analyzer), "Prod");
        assert_eq!(generate_default_value("Delay", &analyzer), "Wait(0)");
        assert_eq!(generate_default_value("Send", &analyzer), "To()");
        assert_eq!(generate_default_value("Level", &analyzer), "very_loud");
        // Still reachable through the wrappers the validator strips.
        assert_eq!(generate_default_value("Option<Mode>", &analyzer), "None");
    }

    /// A tuple or newtype struct has to be given its elements: `UserId()` is a
    /// `UserId` short one element, which no diagnostic reports — the server
    /// called the document clean and serde then refused to read it.
    #[test]
    fn test_generate_default_value_for_tuple_struct_supplies_elements() {
        let positional = |index: usize, type_name: &str| FieldInfo {
            name: index.to_string(),
            type_name: type_name.to_string(),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "UserId".to_string(),
            kind: TypeKind::Struct(vec![positional(0, "u32")]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "Pair".to_string(),
            kind: TypeKind::Struct(vec![positional(0, "String"), positional(1, "UserId")]),
            ..Default::default()
        });
        // A named struct keeps the bare constructor: its own required fields are
        // reported and can be added from there.
        analyzer.add_type(TypeInfo {
            name: "Server".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "host".to_string(),
                type_name: "String".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        // A type that contains itself must not send the recursion spinning.
        analyzer.add_type(TypeInfo {
            name: "Node".to_string(),
            kind: TypeKind::Struct(vec![positional(0, "Box<Node>")]),
            ..Default::default()
        });

        assert_eq!(generate_default_value("UserId", &analyzer), "UserId(0)");
        assert_eq!(
            generate_default_value("Pair", &analyzer),
            "Pair(\"\", UserId(0))"
        );
        assert_eq!(generate_default_value("Server", &analyzer), "Server()");
        // A smart pointer is transparent to serde and to RON alike, so it is
        // written as whatever it points at.
        assert_eq!(
            generate_default_value("Box<UserId>", &analyzer),
            "UserId(0)"
        );
        assert_eq!(generate_default_value("Arc<Server>", &analyzer), "Server()");
        assert_eq!(
            generate_default_value("Node", &analyzer),
            "Node(Node(Node()))",
            "the recursion is cut off, not followed"
        );
    }

    /// A tuple field, a fixed-size array field and a `char` field all used to
    /// be given a placeholder `ron` cannot read: a tuple got the type name as
    /// a constructor (`(String,u16)()`, which is not RON at all), a `char` got
    /// `char()`, and an array got the `[]` its brackets suggest — RON that
    /// `ron` then refuses to deserialize, since a fixed-size array is written
    /// as a tuple.
    #[test]
    fn test_generate_default_value_for_std_composites() {
        let analyzer = RustAnalyzer::new();

        assert_eq!(
            generate_default_value("(String, u16)", &analyzer),
            "(\"\", 0)"
        );
        assert_eq!(generate_default_value("[u32; 3]", &analyzer), "(0, 0, 0)");
        assert_eq!(generate_default_value("()", &analyzer), "()");
        assert_eq!(generate_default_value("char", &analyzer), "'a'");
        assert_eq!(
            generate_default_value("(bool, Vec<u8>, Option<u8>)", &analyzer),
            "(false, [], None)"
        );
        // A sequence whose length the value decides keeps the brackets.
        assert_eq!(generate_default_value("Vec<u8>", &analyzer), "[]");
        assert_eq!(generate_default_value("HashSet<u8>", &analyzer), "[]");
    }

    /// The point of the placeholders above: the document the quick-fix leaves
    /// behind has to be one the configured application can actually load.
    #[test]
    fn test_missing_field_action_inserts_a_deserializable_document() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Cfg {
            pair: (String, u16),
            triple: [u32; 3],
            initial: char,
            tags: Vec<String>,
        }

        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            ..Default::default()
        };
        let type_info = TypeInfo {
            name: "Cfg".to_string(),
            kind: TypeKind::Struct(vec![
                field("pair", "(String, u16)"),
                field("triple", "[u32; 3]"),
                field("initial", "char"),
                field("tags", "Vec<String>"),
            ]),
            ..Default::default()
        };

        let actions = missing_field_actions("Cfg()", &type_info, Arc::new(RustAnalyzer::new()));
        assert!(!actions.is_empty(), "the fields should be offered");
        for (title, document) in actions {
            ron::from_str::<Cfg>(&document).unwrap_or_else(|e| {
                panic!("{title:?} produced {document:?}, which ron rejects: {e}")
            });
        }
    }

    /// Same again for the standard-library types, which the analyzer never
    /// registers and which the opaque fallback therefore reached: the quick-fix
    /// used to fill a required `PathBuf` in as `PathBuf()`, a required
    /// `SocketAddr` as `SocketAddr()` and a required `Duration` as
    /// `Duration()`, none of which `ron` will read.
    #[test]
    fn test_missing_std_field_action_inserts_a_deserializable_document() {
        use std::net::{IpAddr, Ipv6Addr, SocketAddr};
        use std::num::NonZeroU32;
        use std::path::PathBuf;
        use std::time::Duration;

        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Cfg {
            log_file: PathBuf,
            addr: SocketAddr,
            advertise: IpAddr,
            v6: Ipv6Addr,
            workers: NonZeroU32,
            timeout: Duration,
            queue: std::collections::VecDeque<String>,
            boxed: Box<PathBuf>,
        }

        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            ..Default::default()
        };
        let type_info = TypeInfo {
            name: "Cfg".to_string(),
            kind: TypeKind::Struct(vec![
                field("log_file", "PathBuf"),
                field("addr", "SocketAddr"),
                field("advertise", "IpAddr"),
                field("v6", "Ipv6Addr"),
                field("workers", "NonZeroU32"),
                field("timeout", "std::time::Duration"),
                field("queue", "VecDeque<String>"),
                field("boxed", "Box<PathBuf>"),
            ]),
            ..Default::default()
        };

        let actions = missing_field_actions("Cfg()", &type_info, Arc::new(RustAnalyzer::new()));
        assert!(!actions.is_empty(), "the fields should be offered");
        for (title, document) in actions {
            ron::from_str::<Cfg>(&document).unwrap_or_else(|e| {
                panic!("{title:?} produced {document:?}, which ron rejects: {e}")
            });
        }
    }

    /// A type the workspace defines is spelled by its own constructor even when
    /// it shares a name with one of the standard-library types, since its
    /// missing fields come back as a diagnostic with a nested action to finish
    /// the job.
    #[test]
    fn test_std_placeholder_does_not_shadow_a_workspace_type() {
        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "crate::config::Duration".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "millis".to_string(),
                type_name: "u64".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });

        assert_eq!(generate_default_value("Duration", &analyzer), "Duration()");
        // Nothing registered under that name: serde's spelling, not `Duration()`.
        assert_eq!(
            generate_default_value("Duration", &RustAnalyzer::new()),
            "(secs: 0, nanos: 0)"
        );
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

    /// Adding the root's missing fields has to leave a document the validator
    /// then accepts — that is the whole value of the action. An enum field used
    /// to be filled in as `Mode()`, which names no variant and so came back as
    /// `unknown-variant`, and a newtype field as `UserId()`, which serde
    /// refuses to read while no diagnostic says so.
    #[tokio::test]
    async fn test_added_fields_leave_a_document_the_validator_accepts() {
        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Mode".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Dev".to_string(),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Prod".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "UserId".to_string(),
            kind: TypeKind::Struct(vec![field("0", "u32")]),
            ..Default::default()
        });
        let root = TypeInfo {
            name: "Cfg".to_string(),
            kind: TypeKind::Struct(vec![field("mode", "Mode"), field("owner", "UserId")]),
            ..Default::default()
        };
        analyzer.add_type(root.clone());
        let analyzer = Arc::new(analyzer);

        let content = "Cfg(\n)";
        let actions = missing_field_actions(content, &root, analyzer.clone());
        assert_eq!(
            actions,
            vec![
                (
                    "Add 2 required fields".to_string(),
                    "Cfg(\n    mode: Dev,\n    owner: UserId(0),\n)".to_string(),
                ),
                (
                    "Add all 2 missing fields".to_string(),
                    "Cfg(\n    mode: Dev,\n    owner: UserId(0),\n)".to_string(),
                ),
            ]
        );

        for (title, fixed) in actions {
            let errors: Vec<_> = super::super::diagnostics::validate_ron_with_analyzer(
                &fixed,
                None,
                &root,
                analyzer.clone(),
            )
            .into_iter()
            .filter(|d| d.severity != Some(DiagnosticSeverity::INFORMATION))
            .map(|d| d.message)
            .collect();
            assert!(
                errors.is_empty(),
                "'{title}' left problems in:\n{fixed}\n{errors:?}"
            );
            assert!(
                ron::from_str::<ron::Value>(&fixed).is_ok(),
                "'{title}' left unparseable RON:\n{fixed}"
            );
        }
    }

    /// A root type carrying one enum-typed field `mode: Mode`, where `Mode`'s
    /// `Prod` variant requires both `a` and `b`.
    fn nested_variant_fixture() -> (TypeInfo, Arc<RustAnalyzer>) {
        use crate::rust_analyzer::EnumVariant;

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Mode".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "Prod".to_string(),
                fields: vec![
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
                ],
                ..Default::default()
            }]),
            ..Default::default()
        });

        let root = TypeInfo {
            name: "Cfg".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "mode".to_string(),
                type_name: "Mode".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        (root, Arc::new(analyzer))
    }

    /// The titles and resulting documents of every missing-field action offered
    /// for `content`.
    fn missing_field_actions(
        content: &str,
        type_info: &TypeInfo,
        analyzer: Arc<RustAnalyzer>,
    ) -> Vec<(String, String)> {
        let url = Url::parse("file:///test.ron").unwrap();
        generate_code_actions(&parse(content), content, type_info, &url, analyzer, &[])
            .iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) if a.title.contains("field") => {
                    let edits = a.edit.as_ref()?.changes.as_ref()?.get(&url)?;
                    Some((a.title.clone(), apply(content, edits)))
                }
                _ => None,
            })
            .collect()
    }

    /// Fields missing from an enum variant nested under a root field have to be
    /// inserted into that variant's own parens. They used to go through the
    /// root-only insertion path, which appended them to the document root — so
    /// the quick-fix left the variant still incomplete and added fields the root
    /// type doesn't have.
    #[test]
    fn test_nested_variant_fields_are_inserted_into_the_variant() {
        let (root, analyzer) = nested_variant_fixture();
        let content = "Cfg(\n    mode: Prod(\n        a: 1,\n    ),\n)";

        assert_eq!(
            missing_field_actions(content, &root, analyzer),
            vec![(
                "Add 1 required field to mode::Prod".to_string(),
                "Cfg(\n    mode: Prod(\n        a: 1,\n        b: \"\",\n    ),\n)".to_string(),
            )]
        );
    }

    /// Only a variant's own fields count as supplied. The walk used to descend
    /// through them, so a field of some nested type that happened to share a
    /// name with a required one made the variant look complete.
    #[test]
    fn test_nested_variant_ignores_field_names_of_nested_values() {
        let (root, analyzer) = nested_variant_fixture();
        // `b` here belongs to `Inner`, not to `Prod`.
        let content = "Cfg(\n    mode: Prod(\n        a: 1,\n        junk: Inner(b: 2),\n    ),\n)";

        let actions = missing_field_actions(content, &root, analyzer);
        assert_eq!(
            actions.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["Add 1 required field to mode::Prod"],
            "`b` is still missing from `Prod`"
        );
    }

    /// A root type `Cfg(server, certs, extra)` over nested *structs*:
    /// `Server` requires `host` and `port` and carries an optional
    /// `tls: Option<Tls>`, `Tls` requires `cert`, `certs` is a `Vec<Tls>` with a
    /// default, and `extra: Option<Defaults>` resolves to a type carrying a
    /// container-level `#[serde(default)]`.
    ///
    /// Every test below spells out all three root fields, so the only actions
    /// left are the nested ones under test.
    fn nested_struct_fixture() -> (TypeInfo, Arc<RustAnalyzer>) {
        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::new();
        analyzer.add_type(TypeInfo {
            name: "Server".to_string(),
            kind: TypeKind::Struct(vec![
                field("host", "String"),
                field("port", "u16"),
                field("tls", "Option<Tls>"),
            ]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "Tls".to_string(),
            kind: TypeKind::Struct(vec![field("cert", "String")]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "Defaults".to_string(),
            kind: TypeKind::Struct(vec![field("anything", "String")]),
            has_default: true,
            ..Default::default()
        });

        let root = TypeInfo {
            name: "Cfg".to_string(),
            kind: TypeKind::Struct(vec![
                field("server", "Server"),
                FieldInfo {
                    has_default: true,
                    ..field("certs", "Vec<Tls>")
                },
                field("extra", "Option<Defaults>"),
            ]),
            ..Default::default()
        };
        (root, Arc::new(analyzer))
    }

    /// The fields a nested *struct* value is missing used to get no quick-fix at
    /// all: only the document root and an enum variant one level under it were
    /// walked, so the `missing-required-field` diagnostic the validator reports
    /// for `server` named a field the user then had to type out by hand.
    #[test]
    fn test_nested_struct_fields_are_inserted_into_the_struct() {
        let (root, analyzer) = nested_struct_fixture();
        let content = "Cfg(\n    server: Server(\n        port: 1,\n    ),\n    certs: [],\n    extra: None,\n)";

        assert_eq!(
            missing_field_actions(content, &root, analyzer),
            vec![(
                "Add 1 required field to server".to_string(),
                "Cfg(\n    server: Server(\n        port: 1,\n        host: \"\",\n    ),\n    certs: [],\n    extra: None,\n)".to_string(),
            )]
        );
    }

    /// The walk keeps going below the first nested value, and sees through the
    /// `Some(...)` that RON spells an inhabited `Option` with — so `Tls` is
    /// recognized as the value of `tls`, rather than `Some` being mistaken for
    /// it.
    #[test]
    fn test_nested_struct_walk_descends_through_option() {
        let (root, analyzer) = nested_struct_fixture();
        let content = "Cfg(\n    server: Server(host: \"h\", port: 1, tls: Some(Tls())),\n    certs: [],\n    extra: None,\n)";

        assert_eq!(
            missing_field_actions(content, &root, analyzer),
            vec![(
                "Add 1 required field to server.tls".to_string(),
                "Cfg(\n    server: Server(host: \"h\", port: 1, tls: Some(Tls(cert: \"\"))),\n    certs: [],\n    extra: None,\n)".to_string(),
            )]
        );
    }

    /// Each element of a `Vec<T>` is a value of `T` in its own right, and the
    /// action has to name which one it fills in.
    #[test]
    fn test_nested_struct_walk_visits_array_elements() {
        let (root, analyzer) = nested_struct_fixture();
        let content = "Cfg(\n    server: Server(host: \"h\", port: 1),\n    certs: [Tls(cert: \"a\"), Tls()],\n    extra: None,\n)";

        assert_eq!(
            missing_field_actions(content, &root, analyzer),
            vec![(
                "Add 1 required field to certs[1]".to_string(),
                "Cfg(\n    server: Server(host: \"h\", port: 1),\n    certs: [Tls(cert: \"a\"), Tls(cert: \"\")],\n    extra: None,\n)".to_string(),
            )]
        );
    }

    /// An absent `Option` is written as the bare `None`, which parses as a
    /// `struct` node named `None`. Taking it for a value of the field's own type
    /// would offer to fill in fields of a value that isn't there.
    #[test]
    fn test_no_nested_action_for_absent_option() {
        let (root, analyzer) = nested_struct_fixture();
        let content = "Cfg(\n    server: Server(host: \"h\", port: 1, tls: None),\n    certs: [],\n    extra: None,\n)";

        assert_eq!(missing_field_actions(content, &root, analyzer), vec![]);
    }

    /// A container-level `#[serde(default)]` fills in whatever is absent, so
    /// nothing nested under it is required.
    #[test]
    fn test_no_nested_action_for_container_default() {
        let (root, analyzer) = nested_struct_fixture();
        let content = "Cfg(\n    server: Server(host: \"h\", port: 1),\n    certs: [],\n    extra: Defaults(),\n)";

        assert_eq!(missing_field_actions(content, &root, analyzer), vec![]);
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

    /// The edits that insert `fields` into the root struct of `content`.
    fn root_insertions(tree: &Tree, content: &str, fields: &[(&str, &str)]) -> Vec<TextEdit> {
        let root = root_struct_node(tree).expect("expected a root struct");
        generate_field_insertions(&root, content, &missing(fields), &RustAnalyzer::new())
            .expect("expected an insertion edit")
    }

    /// Insert `fields` into the root struct of `content` and return the
    /// resulting document, asserting it still parses as RON.
    fn insert(content: &str, fields: &[(&str, &str)]) -> String {
        let tree = parse(content);
        let result = apply(content, &root_insertions(&tree, content, fields));
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

    /// `()` is a struct value with nothing in it, but the grammar hands it back
    /// as one indivisible `unit` token — no `(` and `)` children to anchor an
    /// insertion between. Without a path of its own the missing-field quick-fix
    /// had nothing to offer on the very document that is missing everything.
    #[test]
    fn test_insert_into_empty_unnamed_struct() {
        assert_eq!(insert("()", &[("b", "u32")]), "(b: 0)");
        assert_eq!(
            insert("()", &[("b", "u32"), ("c", "String")]),
            "(b: 0, c: \"\")"
        );
        // Not on the first line, and not at column 0: the insertion point is
        // one character past the `(` wherever that happens to be.
        assert_eq!(
            insert("// a comment\n    ()", &[("b", "u32")]),
            "// a comment\n    (b: 0)"
        );
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
        let tree = parse(content);
        let edits = root_insertions(&tree, content, &[("b", "u32")]);
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

    /// The validator reports `missing-required-field` on a nested `()`, so the
    /// quick-fix that fills it in has to reach it too — otherwise the error
    /// names fields the user is left to type out by hand.
    #[test]
    fn test_nested_empty_unnamed_struct_gets_its_own_action() {
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
            name: "Cfg".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "srv".to_string(),
                type_name: "Server".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let url = Url::parse("file:///test.ron").unwrap();

        let content = "Cfg(\n    srv: (),\n)";
        let actions = generate_code_actions(
            &parse(content),
            content,
            &config,
            &url,
            Arc::new(analyzer),
            &[],
        );
        let nested: Vec<(&String, &Vec<TextEdit>)> = actions
            .iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) if a.title.ends_with("to srv") => {
                    Some((&a.title, a.edit.as_ref()?.changes.as_ref()?.get(&url)?))
                }
                _ => None,
            })
            .collect();

        assert_eq!(
            nested.len(),
            1,
            "expected one action for the nested value. Got: {actions:?}"
        );
        assert_eq!(nested[0].0, "Add 1 required field to srv");
        assert_eq!(
            apply(content, nested[0].1),
            "Cfg(\n    srv: (host: \"\"),\n)",
            "the field goes into the empty value's own parens"
        );
    }
}
