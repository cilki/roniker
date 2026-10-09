use super::ts_utils::{
    self, ancestors, child_by_kind, field_name, node_at_position, node_text,
    position_to_byte_offset, struct_name,
};
use tower_lsp::lsp_types::Position;
use tree_sitter::{Node, Tree};

/// One struct value the cursor is inside, i.e. one step of the nesting path
/// from the document's root value down to the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeContext {
    /// The struct name written in the document, when the value has one
    /// (e.g. `User(...)`). RON lets the name be omitted (`(...)`), in which
    /// case this is `None` and only `field_name` says what the type is.
    pub type_name: Option<String>,
    /// The name of the field this value is the value of (`server` for the
    /// `(...)` in `server: (...)`), or `None` when the value belongs to no
    /// field — which is the case for the document's root value.
    pub field_name: Option<String>,
}

/// Find the nested type context at a cursor position using tree-sitter.
/// Returns a stack of type contexts from outermost to innermost, with one
/// entry per enclosing struct value whether or not it names its type. The
/// first entry is therefore always the document's root value, which lets
/// [`navigate_type_contexts`] walk the stack without having to guess how the
/// document was written.
///
/// [`navigate_type_contexts`]: super::navigation::navigate_type_contexts
pub fn find_type_context_at_position(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Vec<TypeContext> {
    let mut contexts = Vec::new();
    if let Some(current) = node_at_position(tree, content, position) {
        // Walk up the tree to collect all struct contexts
        for node in ancestors(current) {
            if node.kind() == "struct" {
                contexts.push(TypeContext {
                    type_name: struct_name(&node, content).map(str::to_string),
                    field_name: owning_field_name(&node, content).map(str::to_string),
                });
            }
        }
    }

    // Reverse to get outermost to innermost
    contexts.reverse();
    contexts
}

/// The name of the field whose value `node` is, if any.
///
/// The walk stops at the first enclosing `struct`, because a value nested
/// directly inside another struct value — the `Inner(..)` of `Some(Inner(..))`
/// — is that struct's payload rather than the value of any field. Wrappers
/// that aren't structs are walked through, so the element of an array still
/// finds the field the array belongs to.
fn owning_field_name<'a>(node: &Node, content: &'a str) -> Option<&'a str> {
    ancestors(*node)
        .skip(1)
        .take_while(|ancestor| ancestor.kind() != "struct")
        .find_map(|ancestor| field_name(&ancestor, content))
}

/// Get the field name at a specific position in RON content using tree-sitter
pub fn get_field_at_position(tree: &Tree, content: &str, position: Position) -> Option<String> {
    let current = node_at_position(tree, content, position)?;

    // While a value is still being typed with no terminator (e.g. `mode: ` with
    // no trailing comma yet), tree-sitter can't build a `field` node: the field
    // name parses as a bare `identifier` and the dangling `:` becomes an `ERROR`
    // sibling. The ancestor walk below would then skip past the real field and
    // report the enclosing field instead (or nothing at all), so value
    // completion falls back to offering every workspace type. Recover the field
    // name from that `identifier ERROR(":")` pair first.
    if let Some(field) = field_of_unterminated_value(current, content, position) {
        return Some(field);
    }

    // Walk up to find a field node
    ancestors(current).find_map(|node| field_name(&node, content).map(str::to_string))
}

/// Recover the field name for a value that is mid-edit and has no terminator.
///
/// Such a value defeats tree-sitter's `field` production, leaving the field name
/// as a bare `identifier` immediately followed by an `ERROR` node holding the
/// `:`. When the cursor sits at or past that identifier within the same struct,
/// treat it as the field being edited. The cursor check keeps a well-formed
/// earlier field on the same struct from being misattributed to the dangling one.
fn field_of_unterminated_value(node: Node, content: &str, position: Position) -> Option<String> {
    let (name, _) = unterminated_field_pair(node, content, position)?;
    node_text(&name, content).map(str::to_string)
}

/// The field whose value the cursor is positioned to type, when that value is
/// unterminated and so has no `field` node of its own: the cursor must be at or
/// past the dangling `:`. Callers choosing between completing a field name and
/// completing a value have no `field` node to go on in that case.
pub fn unterminated_value_field(tree: &Tree, content: &str, position: Position) -> Option<String> {
    let node = node_at_position(tree, content, position)?;
    let (name, colon) = unterminated_field_pair(node, content, position)?;
    if position_to_byte_offset(content, position) < colon.end_byte() {
        return None;
    }
    node_text(&name, content).map(str::to_string)
}

/// Locate the `identifier` / `ERROR(":")` child pair that an unterminated field
/// leaves behind in the struct containing `node`, if the cursor is at or past it.
fn unterminated_field_pair<'a>(
    node: Node<'a>,
    content: &str,
    position: Position,
) -> Option<(Node<'a>, Node<'a>)> {
    let cursor_byte = position_to_byte_offset(content, position);
    let struct_node = ancestors(node).find(|n| n.kind() == "struct")?;

    let mut walk = struct_node.walk();
    let children: Vec<Node> = struct_node.children(&mut walk).collect();
    children
        .windows(2)
        .filter(|pair| pair[0].kind() == "identifier" && pair[1].kind() == "ERROR")
        .filter(|pair| node_text(&pair[1], content).map(str::trim) == Some(":"))
        .rfind(|pair| cursor_byte >= pair[0].start_byte())
        .map(|pair| (pair[0], pair[1]))
}

/// Find the current variant context (enum variant name) at a position
pub fn find_current_variant_context(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Option<String> {
    // Walk up to find the innermost struct node with a name
    let current = node_at_position(tree, content, position)?;
    ancestors(current).find_map(|node| {
        if node.kind() != "struct" {
            return None;
        }
        // A struct is a variant when it has a name that starts uppercase.
        let name = struct_name(&node, content)?;
        name.chars()
            .next()
            .filter(|c| c.is_uppercase())
            .map(|_| name.to_string())
    })
}

/// Get the containing field context by finding the parent field
/// For example: "post_type: Detailed(\n    length: 1" - when on "length" line, returns "post_type"
pub fn get_containing_field_context(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Option<String> {
    // Walk up the tree to find the parent field that contains a struct which
    // contains our current field. The first field ancestor is the one we're in;
    // the next named field ancestor is the containing field.
    let current = node_at_position(tree, content, position)?;
    ancestors(current)
        .filter(|node| node.kind() == "field")
        .skip(1)
        .find_map(|node| field_name(&node, content).map(str::to_string))
}

/// Information about a variant field location in RON content
#[derive(Debug, Clone)]
pub struct VariantFieldLocation {
    pub variant_name: String,
    pub containing_field_name: String,
    pub field_at_position: Option<String>,
}

/// Scan through content and find all variant field locations
pub fn find_all_variant_field_locations(tree: &Tree, content: &str) -> Vec<VariantFieldLocation> {
    let mut locations = Vec::new();
    let root = tree.root_node();

    // `visit_fields` recurses, so a tree too deep to walk yields nothing.
    if ts_utils::exceeds_max_depth(&root) {
        return locations;
    }

    // Walk the tree to find all field nodes that are inside struct variants
    visit_fields(&root, content, &mut locations);

    locations
}

/// Recursively visit nodes to find field locations inside variants
fn visit_fields(node: &Node, content: &str, locations: &mut Vec<VariantFieldLocation>) {
    // Check if this is a struct (potential variant)
    if node.kind() == "struct"
        && let Some(variant_name) = struct_name(node, content)
    {
        // This is a named struct, check if it's a variant (uppercase start)
        if variant_name
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase())
        {
            // Look for the containing field by checking parent
            let containing_field_name = find_parent_field_name(node, content);

            // Now collect all fields inside this variant
            collect_fields_in_node(
                node,
                content,
                variant_name,
                &containing_field_name,
                locations,
            );
        }
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_fields(&child, content, locations);
    }
}

/// Find the parent field name of a node
fn find_parent_field_name(node: &Node, content: &str) -> Option<String> {
    ancestors(*node)
        .skip(1)
        .find_map(|n| field_name(&n, content).map(str::to_string))
}

/// Collect all fields in a node
fn collect_fields_in_node(
    node: &Node,
    content: &str,
    variant_name: &str,
    containing_field_name: &Option<String>,
    locations: &mut Vec<VariantFieldLocation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "field" {
            // Extract field name
            let field_at_position = field_name(&child, content).map(|s| s.to_string());

            if let Some(containing_field) = containing_field_name {
                locations.push(VariantFieldLocation {
                    variant_name: variant_name.to_string(),
                    containing_field_name: containing_field.clone(),
                    field_at_position,
                });
            }
        }

        // Recurse for nested structures
        collect_fields_in_node(
            &child,
            content,
            variant_name,
            containing_field_name,
            locations,
        );
    }
}

/// Parse RON structure to get all field names present at the top level
pub fn extract_fields_from_ron(tree: &Tree, content: &str) -> Vec<String> {
    let mut fields = std::collections::HashSet::new();
    let root = tree.root_node();

    // Find the first struct node (the top-level value)
    if let Some(struct_node) = child_by_kind(&root, "struct") {
        // Collect only direct child fields of this struct
        collect_direct_field_names(&struct_node, content, &mut fields);
    }

    fields.into_iter().collect()
}

/// Collect only direct child field names from a struct node (not recursively)
fn collect_direct_field_names(
    node: &Node,
    content: &str,
    fields: &mut std::collections::HashSet<String>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(name) = field_name(&child, content) {
            fields.insert(name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ts_utils::RonParser;
    use super::*;

    fn parse(content: &str) -> Tree {
        RonParser::new().parse(content).unwrap()
    }

    #[test]
    fn test_parse_simple_struct() {
        let content = r#"MyStruct(
    name: "test",
    age: 30,
)"#;
        let mut parser = RonParser::new();
        let tree = parser.parse(content);
        assert!(tree.is_some());
    }

    #[test]
    fn test_find_type_context_nested() {
        let content = r#"PostReference(Post(
    id: 42,
    author: User(
        name: "Alice",
    ),
))"#;
        // Position inside User
        let contexts =
            find_type_context_at_position(&parse(content), content, Position::new(3, 20));
        assert_eq!(contexts.len(), 3);
        assert_eq!(contexts[0].type_name.as_deref(), Some("PostReference"));
        assert_eq!(contexts[1].type_name.as_deref(), Some("Post"));
        assert_eq!(contexts[2].type_name.as_deref(), Some("User"));
        // Only `User` is the value of a field; the other two are the root value
        // and a tuple-struct payload.
        assert_eq!(contexts[0].field_name, None);
        assert_eq!(contexts[1].field_name, None);
        assert_eq!(contexts[2].field_name.as_deref(), Some("author"));
    }

    #[test]
    fn test_find_type_context_unnamed_structs() {
        // RON lets a struct value omit its name. Every enclosing struct still
        // gets a context, so the first entry is always the root value and the
        // field name is there to resolve the type from.
        let content = "(\n    server: (\n        host: \"x\",\n    ),\n)";
        let contexts =
            find_type_context_at_position(&parse(content), content, Position::new(2, 10));
        assert_eq!(contexts.len(), 2);
        assert_eq!(
            contexts[0],
            TypeContext {
                type_name: None,
                field_name: None,
            }
        );
        assert_eq!(
            contexts[1],
            TypeContext {
                type_name: None,
                field_name: Some("server".to_string()),
            }
        );
    }

    #[test]
    fn test_find_type_context_unnamed_struct_in_array() {
        // An array is not a struct, so the element's owning field is still
        // found through it.
        let content = "Config(\n    servers: [\n        (host: \"x\"),\n    ],\n)";
        let contexts =
            find_type_context_at_position(&parse(content), content, Position::new(2, 12));
        assert_eq!(contexts.len(), 2);
        assert_eq!(contexts[0].type_name.as_deref(), Some("Config"));
        assert_eq!(
            contexts[1],
            TypeContext {
                type_name: None,
                field_name: Some("servers".to_string()),
            }
        );
    }

    #[test]
    fn test_get_field_at_position() {
        let content = r#"MyStruct(
    name: "test",
    age: 30,
)"#;
        // Position on "name" field
        let field = get_field_at_position(&parse(content), content, Position::new(1, 8));
        assert_eq!(field, Some("name".to_string()));
    }

    #[test]
    fn test_get_field_at_position_unterminated_value() {
        // A value being typed with no trailing comma: the field name parses as a
        // bare identifier and the `:` becomes an ERROR node, so the plain
        // ancestor walk can't see the `mode` field. The recovery should still
        // report it.
        let content = "AppConfig(\n    server: ServerConfig(\n        mode: \n    ),\n)";
        // Cursor just after `mode: `.
        let field = get_field_at_position(&parse(content), content, Position::new(2, 14));
        assert_eq!(field, Some("mode".to_string()));
    }

    #[test]
    fn test_get_field_at_position_unterminated_toplevel() {
        // Same situation at the top level: the walk finds no enclosing field, so
        // without recovery the field would come back as None.
        let content = "Config(\n    mode: \n)";
        let field = get_field_at_position(&parse(content), content, Position::new(1, 10));
        assert_eq!(field, Some("mode".to_string()));
    }

    #[test]
    fn test_get_field_at_position_unterminated_after_prior_field() {
        // A well-formed earlier field must not be misattributed to the dangling
        // one. Cursor on the earlier field's value still resolves to that field.
        let content = "S(\n    name: \"x\",\n    mode: \n)";
        let on_name = get_field_at_position(&parse(content), content, Position::new(1, 12));
        assert_eq!(on_name, Some("name".to_string()));
        // Cursor on the unterminated field resolves to it.
        let on_mode = get_field_at_position(&parse(content), content, Position::new(2, 10));
        assert_eq!(on_mode, Some("mode".to_string()));
    }

    #[test]
    fn test_find_current_variant_context() {
        let content = r#"Detailed(
    length: 1,
)"#;
        let variant = find_current_variant_context(&parse(content), content, Position::new(1, 12));
        assert_eq!(variant, Some("Detailed".to_string()));
    }

    #[test]
    fn test_extract_fields_from_ron() {
        let content = r#"MyStruct(
    name: "test",
    age: 30,
    items: [],
)"#;
        let fields = extract_fields_from_ron(&parse(content), content);
        assert!(fields.contains(&"name".to_string()));
        assert!(fields.contains(&"age".to_string()));
        assert!(fields.contains(&"items".to_string()));
    }

    #[test]
    fn test_enum_variant_field_detection() {
        let content = r#"Post(
    id: 42,
    post_type: Detailed(
        length: 1,
    ),
)"#;
        let position = Position::new(3, 16);

        let tree = parse(content);
        let variant = find_current_variant_context(&tree, content, position);
        assert_eq!(variant, Some("Detailed".to_string()));

        let containing_field = get_containing_field_context(&tree, content, position);
        assert_eq!(containing_field, Some("post_type".to_string()));
    }

    /// `visit_fields` recurses, so a document nested far enough used to
    /// exhaust the stack and abort the process.
    ///
    /// If the depth guard regresses this test does not fail, it kills the test
    /// binary with a stack overflow, which is the point.
    #[test]
    fn test_deeply_nested_document_yields_no_variant_locations() {
        let depth = ts_utils::MAX_TREE_DEPTH * 20;
        let content = format!("{}1{}", "Outer(inner: ".repeat(depth), ")".repeat(depth));
        assert!(find_all_variant_field_locations(&parse(&content), &content).is_empty());
    }
}
