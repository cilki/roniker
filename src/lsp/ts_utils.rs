/// Tree-sitter utilities for RON LSP
/// This module provides high-level helpers for working with tree-sitter AST nodes
use std::cell::RefCell;

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Node, Parser, Tree};

/// Wrapper around tree-sitter parser for RON files
pub struct RonParser {
    parser: Parser,
}

impl RonParser {
    pub fn new() -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&ron_lsp_tree_sitter::language())
            .expect("Error loading RON language");
        Self { parser }
    }

    /// Parse RON content and return the tree
    pub fn parse(&mut self, content: &str) -> Option<Tree> {
        self.parser.parse(content, None)
    }

    /// Re-parse content reusing a previous (edited) tree for incremental parsing
    pub fn parse_with(&mut self, content: &str, old_tree: Option<&Tree>) -> Option<Tree> {
        self.parser.parse(content, old_tree)
    }
}

impl Default for RonParser {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    // Building a `Parser` allocates and loads the RON grammar via `set_language`,
    // which is wasteful to repeat on every parse (e.g. once per keystroke through
    // `Document::apply_change`). Keep one parser per thread and reuse it.
    static PARSER: RefCell<RonParser> = RefCell::new(RonParser::new());
}

/// Parse RON content using the thread-local reusable parser.
pub fn parse(content: &str) -> Option<Tree> {
    PARSER.with(|parser| parser.borrow_mut().parse(content))
}

/// Re-parse content reusing a previous (edited) tree for incremental parsing,
/// backed by the thread-local reusable parser.
pub fn parse_with(content: &str, old_tree: Option<&Tree>) -> Option<Tree> {
    PARSER.with(|parser| parser.borrow_mut().parse_with(content, old_tree))
}

/// Convert LSP Position to byte offset in content
pub fn position_to_byte_offset(content: &str, position: Position) -> usize {
    let mut offset = 0;
    let mut current_line = 0;
    let mut current_col = 0;

    for ch in content.chars() {
        if current_line == position.line as usize && current_col == position.character as usize {
            return offset;
        }

        if ch == '\n' {
            current_line += 1;
            current_col = 0;
        } else {
            current_col += 1;
        }

        offset += ch.len_utf8();
    }

    offset
}

/// Convert tree-sitter byte range to LSP Range
pub fn node_to_lsp_range(node: &Node) -> Range {
    let start_pos = node.start_position();
    let end_pos = node.end_position();

    Range {
        start: Position {
            line: start_pos.row as u32,
            character: start_pos.column as u32,
        },
        end: Position {
            line: end_pos.row as u32,
            character: end_pos.column as u32,
        },
    }
}

/// Get the text content of a node
pub fn node_text<'a>(node: &Node, content: &'a str) -> Option<&'a str> {
    node.utf8_text(content.as_bytes()).ok()
}

/// Whether a node is a RON comment (`// ...` line or `/* ... */` block).
pub fn is_comment(node: &Node) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

/// Find the deepest node at a given position
pub fn node_at_position<'a>(tree: &'a Tree, content: &str, position: Position) -> Option<Node<'a>> {
    let byte_offset = position_to_byte_offset(content, position);
    let root = tree.root_node();
    root.descendant_for_byte_range(byte_offset, byte_offset)
}

/// Iterate over `node` and each of its ancestors, innermost first (the node
/// itself comes first, then its parent, and so on up to the root).
pub fn ancestors<'a>(node: Node<'a>) -> impl Iterator<Item = Node<'a>> {
    let mut next = Some(node);
    std::iter::from_fn(move || {
        let current = next?;
        next = current.parent();
        Some(current)
    })
}

/// Find the first ancestor of a node with a given kind
pub fn find_ancestor_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    ancestors(node).skip(1).find(|n| n.kind() == kind)
}

/// Collect all descendant nodes of a given kind, depth-first
pub fn descendants_by_kind<'a>(tree: &'a Tree, kind: &str) -> Vec<Node<'a>> {
    let mut results = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == kind {
            results.push(node);
        }
        for i in (0..node.child_count()).rev() {
            if let Some(child) = node.child(i) {
                stack.push(child);
            }
        }
    }
    results
}

/// Get all children of a node with a specific kind
pub fn children_by_kind<'a>(node: &Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut results = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            results.push(child);
        }
    }
    results
}

/// Get the first child of a node with a specific kind
pub fn child_by_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();

    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// Get all named children of a node
pub fn named_children<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// Get the text of a node's leading `identifier` child, if the node has the
/// expected kind. Both `struct` and `field` nodes store their name this way.
fn leading_identifier<'a>(node: &Node, content: &'a str, expected_kind: &str) -> Option<&'a str> {
    if node.kind() != expected_kind {
        return None;
    }

    let first_child = node.child(0)?;
    if first_child.kind() == "identifier" {
        node_text(&first_child, content)
    } else {
        None
    }
}

/// Get the struct/variant name from a struct node
/// Returns None if it's an anonymous struct (no identifier child)
pub fn struct_name<'a>(node: &Node, content: &'a str) -> Option<&'a str> {
    leading_identifier(node, content, "struct")
}

/// Get the field name from a field node
pub fn field_name<'a>(node: &Node, content: &'a str) -> Option<&'a str> {
    leading_identifier(node, content, "field")
}

/// Get the value node from a field node
pub fn field_value<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    if node.kind() != "field" {
        return None;
    }

    // Field structure: identifier ":" value
    // So value is typically the 3rd child (after identifier and colon)
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();

    // Find the colon, then return the next node
    for (i, child) in children.iter().enumerate() {
        if child.kind() == ":" && i + 1 < children.len() {
            return Some(children[i + 1]);
        }
    }

    // Fallback: last child that's not a comma
    children.iter().rev().find(|c| c.is_named()).copied()
}

/// Find all fields in a struct node (non-recursive, only direct children)
pub fn struct_fields<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    if node.kind() != "struct" {
        return Vec::new();
    }

    children_by_kind(node, "field")
}

/// Get all value children of a struct (for tuple-style structs), excluding the struct name identifier
pub fn struct_values<'a>(node: &Node<'a>, content: &str) -> Vec<Node<'a>> {
    if node.kind() != "struct" {
        return Vec::new();
    }

    let mut values = Vec::new();
    let mut cursor = node.walk();
    let has_name = struct_name(node, content).is_some();
    let mut skip_first_identifier = has_name;

    for child in node.children(&mut cursor) {
        // Skip the first identifier (struct name) and punctuation
        if child.is_named() && child.kind() != "field" {
            if skip_first_identifier && child.kind() == "identifier" {
                skip_first_identifier = false;
                continue;
            }
            values.push(child);
        }
    }

    values
}

/// Find the main value node (the actual RON data, skipping annotation and comments)
pub fn find_main_value(tree: &Tree) -> Option<Node<'_>> {
    let root = tree.root_node();
    let mut cursor = root.walk();

    // Skip type_annotation, extensions, and comments - find the first actual value node
    for child in root.children(&mut cursor) {
        if child.is_named() {
            let kind = child.kind();
            if kind != "type_annotation"
                && kind != "extensions"
                && kind != "extension"
                && !is_comment(&child)
            {
                return Some(child);
            }
        }
    }

    None
}

/// Check if a node represents an empty structure (), [], or {}
pub fn is_empty_structure(node: &Node) -> bool {
    match node.kind() {
        "struct" | "array" | "map" | "tuple" => {
            // Check if it has any named children (fields, values, etc.)
            let mut cursor = node.walk();
            let has_children = node.named_children(&mut cursor).next().is_some();
            !has_children
        }
        _ => false,
    }
}

/// The enum variant a value names, and where that name is written.
#[derive(Debug, Clone)]
pub struct ParsedEnumVariant {
    pub name: String,
    pub range: Range,
}

/// The enum variant a value names, if it names one.
///
/// RON spells a unit variant as a bare name (`Prod`), and tuple and struct
/// variants as `Prod(30)` / `Prod(retries: 3)` — the latter two both parsing as
/// a `struct` node whose leading identifier is the variant name. `None` for
/// anything that names no variant, including a struct written with RON's
/// unnamed syntax (`(retries: 3)`).
pub fn extract_enum_variant(node: &Node, content: &str) -> Option<ParsedEnumVariant> {
    let name_node = match node.kind() {
        "struct" => node.child(0).filter(|c| c.kind() == "identifier")?,
        "identifier" => *node,
        _ => return None,
    };

    let name = node_text(&name_node, content)?.trim();
    if name.is_empty() {
        return None;
    }

    Some(ParsedEnumVariant {
        name: name.to_string(),
        range: node_to_lsp_range(&name_node),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_struct() {
        let mut parser = RonParser::new();
        let tree = parser.parse("User(id: 1, name: \"test\")");
        assert!(tree.is_some());
    }

    #[test]
    fn test_struct_name() {
        let mut parser = RonParser::new();
        let tree = parser.parse("User(id: 1)").unwrap();
        let root = tree.root_node();

        if let Some(struct_node) = child_by_kind(&root, "struct") {
            let name = struct_name(&struct_node, "User(id: 1)");
            assert_eq!(name, Some("User"));
        } else {
            panic!("No struct node found");
        }
    }

    #[test]
    fn test_struct_fields() {
        let mut parser = RonParser::new();
        let tree = parser.parse("User(id: 1, name: \"test\")").unwrap();
        let root = tree.root_node();

        if let Some(struct_node) = child_by_kind(&root, "struct") {
            let fields = struct_fields(&struct_node);
            assert_eq!(fields.len(), 2);
        }
    }

    #[test]
    fn test_field_name() {
        let content = "User(id: 1)";
        let mut parser = RonParser::new();
        let tree = parser.parse(content).unwrap();
        let root = tree.root_node();

        if let Some(struct_node) = child_by_kind(&root, "struct") {
            let fields = struct_fields(&struct_node);
            if let Some(field) = fields.first() {
                let name = field_name(field, content);
                assert_eq!(name, Some("id"));
            }
        }
    }

    #[test]
    fn test_struct_name_extraction() {
        let content = "User(id: 1)";
        let mut parser = RonParser::new();
        let tree = parser.parse(content).unwrap();

        if let Some(main_value) = find_main_value(&tree) {
            assert_eq!(main_value.kind(), "struct");
            if let Some(name) = struct_name(&main_value, content) {
                assert_eq!(name, "User");
            }
        } else {
            panic!("No main value found");
        }
    }

    #[test]
    fn test_empty_structure() {
        let mut parser = RonParser::new();
        let content = "Unit()";
        let tree = parser.parse(content).unwrap();

        if let Some(value) = find_main_value(&tree) {
            assert_eq!(value.kind(), "struct");
            let values = struct_values(&value, content);
            let fields = struct_fields(&value);
            assert_eq!(values.len(), 0);
            assert_eq!(fields.len(), 0);
        }
    }

    #[test]
    fn test_position_to_byte_offset() {
        let content = "abc\ndef";
        let offset = position_to_byte_offset(content, Position::new(1, 1));
        assert_eq!(offset, 5); // After "abc\nd"
    }

    #[test]
    fn test_field_value_extraction_nested() {
        let content = "Post(author: User(id: 1, name: \"Alice\"))";
        let mut parser = RonParser::new();
        let tree = parser.parse(content).unwrap();

        if let Some(main) = find_main_value(&tree) {
            let fields = struct_fields(&main);
            if let Some(author_field) = fields.first()
                && let Some(value) = field_value(author_field)
            {
                let value_text = node_text(&value, content).unwrap();
                println!("Author field value: {}", value_text);
                assert!(value_text.contains("User("));
                assert!(value_text.contains("id: 1"));
                assert!(value_text.contains("name: \"Alice\""));
            }
        }
    }

    #[test]
    fn test_parse_double_colon_syntax() {
        let content = "MyEnum::StructVariant(\n    field_a: \"test\"\n)";
        let mut parser = RonParser::new();
        let tree = parser.parse(content);

        if let Some(tree) = tree {
            println!("Parsed successfully");
            let root = tree.root_node();
            println!("Root sexp: {}", root.to_sexp());

            if let Some(main) = find_main_value(&tree) {
                println!("Main value kind: {}", main.kind());
                println!("Main value child count: {}", main.child_count());

                let mut cursor = main.walk();
                for child in main.children(&mut cursor) {
                    println!(
                        "  Child: kind='{}' is_named={}",
                        child.kind(),
                        child.is_named()
                    );
                }

                if let Some(name) = struct_name(&main, content) {
                    println!("Struct name: {}", name);
                }
            }
        } else {
            panic!("Failed to parse");
        }
    }
}
