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

/// The text of row `row`, counting rows the way tree-sitter does: one per `\n`,
/// with any `\r` left in place. Empty when the row is past the end of `content`.
pub fn line_at(content: &str, row: usize) -> &str {
    content.split('\n').nth(row).unwrap_or("")
}

/// The length of `s` in UTF-16 code units, which is what LSP counts columns in.
fn utf16_len(s: &str) -> u32 {
    if s.is_ascii() {
        return s.len() as u32;
    }
    s.encode_utf16().count() as u32
}

/// The UTF-16 column that a byte column on `line` corresponds to.
///
/// LSP counts `Position::character` in UTF-16 code units, while tree-sitter
/// counts `Point::column` in bytes. The two only agree on an all-ASCII line, so
/// everything the server hands back to the editor has to go through here or the
/// ranges land one position too far right for every extra byte earlier on the
/// line.
pub fn utf16_column(line: &str, byte_column: usize) -> u32 {
    utf16_len(&line[..floor_char_boundary(line, byte_column)])
}

/// `offset` clamped into `s` and back to the nearest character boundary at or
/// before it, so that slicing `s` there cannot panic.
fn floor_char_boundary(s: &str, offset: usize) -> usize {
    let mut offset = offset.min(s.len());
    while offset > 0 && !s.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The byte offset at which the row containing `byte_offset` begins.
///
/// Rows are counted the way tree-sitter counts them: one per `\n`. Finding the
/// row's start by scanning *backwards* from the offset costs one line, where
/// counting rows forwards from the start of the document costs everything that
/// precedes it — the difference between answering a request in time
/// proportional to the node and in time proportional to the whole file.
fn line_start(content: &str, byte_offset: usize) -> usize {
    content[..byte_offset]
        .rfind('\n')
        .map(|newline| newline + 1)
        .unwrap_or(0)
}

/// The byte column on `line` that a UTF-16 column corresponds to, clamped to the
/// end of the line. The inverse of [`utf16_column`].
pub fn utf16_column_to_byte(line: &str, utf16_column: usize) -> usize {
    if line.is_ascii() {
        return utf16_column.min(line.len());
    }
    let mut units = 0;
    for (byte, ch) in line.char_indices() {
        if units >= utf16_column {
            return byte;
        }
        units += ch.len_utf16();
    }
    line.len()
}

/// The width of row `row` in UTF-16 code units, i.e. the column just past its
/// last character. Counts rows from the start of `content`; prefer
/// [`line_end_column`] where a byte offset on the row is already to hand.
pub fn line_width(content: &str, row: usize) -> u32 {
    let line = line_at(content, row);
    utf16_column(line, line.len())
}

/// The column just past the last character of the row that `byte_offset` falls
/// on, in UTF-16 code units.
pub fn line_end_column(content: &str, byte_offset: usize) -> u32 {
    let start = line_start(content, floor_char_boundary(content, byte_offset));
    let end = content[start..]
        .find('\n')
        .map(|newline| start + newline)
        .unwrap_or(content.len());
    utf16_len(&content[start..end])
}

/// Convert a byte offset into `content` to an LSP `Position` (line + UTF-16
/// column). `row` is the row the offset falls on, which every caller already
/// knows: tree-sitter reports it alongside the byte offset, and deriving it
/// here instead would mean counting newlines from the start of the document.
pub fn byte_offset_to_position(content: &str, byte_offset: usize, row: usize) -> Position {
    let offset = floor_char_boundary(content, byte_offset);
    Position {
        line: row as u32,
        character: utf16_len(&content[line_start(content, offset)..offset]),
    }
}

/// The LSP position of a node's first byte.
pub fn node_start_position(node: &Node, content: &str) -> Position {
    byte_offset_to_position(content, node.start_byte(), node.start_position().row)
}

/// The LSP position just past a node's last byte.
pub fn node_end_position(node: &Node, content: &str) -> Position {
    byte_offset_to_position(content, node.end_byte(), node.end_position().row)
}

/// Convert LSP Position to byte offset in content
pub fn position_to_byte_offset(content: &str, position: Position) -> usize {
    let mut offset = 0;

    for (row, line) in content.split('\n').enumerate() {
        if row == position.line as usize {
            return offset + utf16_column_to_byte(line, position.character as usize);
        }
        offset += line.len() + 1; // + the '\n' that split consumed
    }

    content.len()
}

/// Convert a node's byte range to an LSP Range
pub fn node_to_lsp_range(node: &Node, content: &str) -> Range {
    Range {
        start: node_start_position(node, content),
        end: node_end_position(node, content),
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
        range: node_to_lsp_range(&name_node, content),
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

    /// LSP columns count UTF-16 code units and tree-sitter columns count bytes.
    /// Converting between them has to account for both multibyte characters
    /// (`ü` is 2 bytes, 1 code unit) and surrogate pairs (`🚀` is 4 bytes, 2
    /// code units).
    #[test]
    fn test_utf16_column_conversions() {
        let line = "a(\"ü🚀\", b: 1)";

        // byte column -> UTF-16 column, at every character boundary
        let boundaries: [(usize, u32); 6] = [
            (0, 0),   // start
            (3, 3),   // after `a("`
            (5, 4),   // after `ü` (2 bytes, 1 unit)
            (9, 6),   // after `🚀` (4 bytes, 2 units)
            (12, 9),  // after `", `
            (13, 10), // after `b`
        ];
        for (byte, utf16) in boundaries {
            assert_eq!(
                utf16_column(line, byte),
                utf16,
                "byte column {byte} is UTF-16 column {utf16}"
            );
            assert_eq!(
                utf16_column_to_byte(line, utf16 as usize),
                byte,
                "UTF-16 column {utf16} is byte column {byte}"
            );
        }

        // Both directions clamp past the end of the line rather than panicking,
        // and a byte column in the middle of a character snaps back to its start.
        assert_eq!(utf16_column(line, line.len() + 10), 14);
        assert_eq!(utf16_column_to_byte(line, 999), line.len());
        assert_eq!(utf16_column(line, 4), 3, "mid-`ü` snaps back to its start");
        assert_eq!(utf16_column(line, 7), 4, "mid-`🚀` snaps back to its start");
    }

    /// A node's reported range is in UTF-16 columns, so an editor underlines
    /// the node itself and not whatever sits that many *bytes* along the line.
    #[test]
    fn test_node_range_is_utf16_not_bytes() {
        let content = "Config(host: \"münchen\", port: 80)";
        let tree = parse(content).unwrap();

        let port = descendants_by_kind(&tree, "field")
            .into_iter()
            .find(|f| field_name(f, content) == Some("port"))
            .expect("the `port` field should parse");
        let range = node_to_lsp_range(&port.child(0).unwrap(), content);

        let expected = content.char_indices().position(|(_, c)| c == 'p').unwrap();
        assert_eq!(range.start.character as usize, expected);
        assert_eq!(range.end.character as usize, expected + "port".len());
    }

    /// Round-tripping a position through a byte offset has to be lossless, or
    /// requests land on the wrong node once a line holds anything non-ASCII.
    #[test]
    fn test_position_byte_offset_round_trip() {
        // The field names sit *after* the surrogate pair on the same line, so
        // a column counted in anything but UTF-16 code units resolves to the
        // wrong node.
        let content = "Config(\n    host: \"🚀 prod\", port: 80, debug: true,\n)";
        let tree = parse(content).unwrap();

        for field in descendants_by_kind(&tree, "field") {
            let name = field.child(0).unwrap();
            let position = node_start_position(&name, content);
            assert_eq!(
                position_to_byte_offset(content, position),
                name.start_byte(),
                "position {position:?} should map back to the node it came from"
            );
            assert_eq!(
                node_at_position(&tree, content, position).map(|n| n.id()),
                Some(name.id()),
                "position {position:?} should resolve to the field name node"
            );
        }
    }

    #[test]
    fn test_line_at_and_line_width() {
        let content = "ü\nab🚀\n";
        assert_eq!(line_at(content, 0), "ü");
        assert_eq!(line_at(content, 1), "ab🚀");
        assert_eq!(line_at(content, 2), "");
        assert_eq!(line_at(content, 9), "");

        assert_eq!(line_width(content, 0), 1);
        assert_eq!(line_width(content, 1), 4, "`ab` plus a surrogate pair");
        assert_eq!(line_width(content, 2), 0);
    }

    /// `line_end_column` has to answer for whichever row the byte offset lands
    /// on, and agree with `line_width`'s count of that row.
    #[test]
    fn test_line_end_column_matches_line_width() {
        let content = "ü\nab🚀\nxy";

        // An offset anywhere on a row, including in the middle of it, answers
        // for that row.
        assert_eq!(line_end_column(content, 0), line_width(content, 0));
        assert_eq!(line_end_column(content, 3), line_width(content, 1));
        assert_eq!(
            line_end_column(content, content.len()),
            line_width(content, 2)
        );

        assert_eq!(line_end_column(content, 3), 4, "`ab` plus a surrogate pair");
        assert_eq!(line_end_column(content, content.len()), 2);
    }

    /// Node positions are derived from the node's byte offset, scanning back to
    /// the start of its own line, rather than by counting rows forward from the
    /// start of the document. The two have to answer identically for every node
    /// — including on lines carrying multi-byte and astral characters, where a
    /// byte offset and a UTF-16 column come apart.
    #[test]
    fn test_node_ranges_agree_with_row_scan() {
        let content =
            "Config(\n    city: \"münchen\",\n    rockets: \"🚀🚀\",\n    tag: \"ü🚀x\",\n)";
        let tree = parse(content).unwrap();

        let mut walked = 0;
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            let range = node_to_lsp_range(&node, content);
            let (start, end) = (node.start_position(), node.end_position());

            assert_eq!(
                range.start,
                Position::new(
                    start.row as u32,
                    utf16_column(line_at(content, start.row), start.column)
                ),
                "start of `{}`",
                node.kind()
            );
            assert_eq!(
                range.end,
                Position::new(
                    end.row as u32,
                    utf16_column(line_at(content, end.row), end.column)
                ),
                "end of `{}`",
                node.kind()
            );

            walked += 1;
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        assert!(walked > 10, "expected a non-trivial tree, walked {walked}");
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
