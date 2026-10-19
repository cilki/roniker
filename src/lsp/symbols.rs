//! Document symbols: a nested outline of the RON document built from the
//! cached tree-sitter parse tree.
use super::ts_utils;
use super::ts_utils::LineIndex;
use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};
use tree_sitter::{Node, Tree};

/// Build the document outline for a RON file.
pub fn document_symbols(tree: &Tree, content: &str) -> Vec<DocumentSymbol> {
    let Some(main_value) = ts_utils::find_main_value(tree) else {
        return Vec::new();
    };
    // The outline is built recursively, so a tree too deep to walk gets none.
    if ts_utils::exceeds_max_depth(&main_value) {
        return Vec::new();
    }
    // Every value and every field contributes a symbol, and every symbol
    // carries two ranges, so the outline converts byte offsets to positions a
    // fixed number of times per node. Index the long rows once rather than
    // scanning the row each conversion lands on, which on a document written as
    // one line is the whole document, every time.
    let lines = LineIndex::new(content);
    value_symbols(&main_value, content, &lines)
}

/// Symbols for a value node: named structs become container symbols,
/// anonymous structs contribute their fields directly, arrays recurse.
fn value_symbols(node: &Node, content: &str, lines: &LineIndex) -> Vec<DocumentSymbol> {
    match node.kind() {
        "struct" => {
            let children: Vec<DocumentSymbol> = ts_utils::struct_fields(node)
                .iter()
                .filter_map(|f| field_symbol(f, content, lines))
                .collect();

            match ts_utils::struct_name(node, content) {
                Some(name) => vec![symbol(
                    name.to_string(),
                    SymbolKind::STRUCT,
                    node,
                    node.child(0).as_ref().unwrap_or(node),
                    children,
                    content,
                    lines,
                )],
                // Anonymous struct: hoist its fields to the parent level
                None => children,
            }
        }
        "array" | "tuple" => ts_utils::named_children(node)
            .iter()
            .flat_map(|child| value_symbols(child, content, lines))
            .collect(),
        _ => Vec::new(),
    }
}

/// Symbol for one `name: value` field, with nested symbols from its value.
fn field_symbol(field_node: &Node, content: &str, lines: &LineIndex) -> Option<DocumentSymbol> {
    let name = ts_utils::field_name(field_node, content)?;
    let name_node = field_node.child(0)?;

    let children = ts_utils::field_value(field_node)
        .map(|value| value_symbols(&value, content, lines))
        .unwrap_or_default();

    Some(symbol(
        name.to_string(),
        SymbolKind::FIELD,
        field_node,
        &name_node,
        children,
        content,
        lines,
    ))
}

#[allow(deprecated)] // DocumentSymbol::deprecated must still be initialized
#[allow(clippy::too_many_arguments)]
fn symbol(
    name: String,
    kind: SymbolKind,
    node: &Node,
    selection_node: &Node,
    children: Vec<DocumentSymbol>,
    content: &str,
    lines: &LineIndex,
) -> DocumentSymbol {
    DocumentSymbol {
        name,
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range: lines.node_range(node, content),
        selection_range: lines.node_range(selection_node, content),
        children: if children.is_empty() {
            None
        } else {
            Some(children)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Tree {
        ts_utils::RonParser::new().parse(content).unwrap()
    }

    #[test]
    fn test_named_struct_outline() {
        let content = "Config(\n    debug: true,\n    server: Server(\n        port: 80,\n    ),\n)";
        let symbols = document_symbols(&parse(content), content);

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Config");
        assert_eq!(symbols[0].kind, SymbolKind::STRUCT);

        let fields = symbols[0].children.as_ref().unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "debug");
        assert_eq!(fields[0].kind, SymbolKind::FIELD);
        assert_eq!(fields[1].name, "server");

        let server = fields[1].children.as_ref().unwrap();
        assert_eq!(server.len(), 1);
        assert_eq!(server[0].name, "Server");
        let server_fields = server[0].children.as_ref().unwrap();
        assert_eq!(server_fields[0].name, "port");
    }

    #[test]
    fn test_anonymous_struct_hoists_fields() {
        let content = "(\n    debug: true,\n)";
        let symbols = document_symbols(&parse(content), content);

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "debug");
        assert_eq!(symbols[0].kind, SymbolKind::FIELD);
    }

    /// A symbol's ranges are columns in UTF-16 code units: an editor uses them
    /// to highlight and reveal the symbol, so a non-ASCII value earlier on the
    /// line must not shift them.
    #[test]
    fn test_selection_range_columns_are_utf16() {
        let content = "Config(host: \"münchen\", port: 80)";
        let symbols = document_symbols(&parse(content), content);

        let fields = symbols[0].children.as_ref().unwrap();
        let port = fields.iter().find(|s| s.name == "port").unwrap();

        let units: Vec<u16> = content.encode_utf16().collect();
        let selected = String::from_utf16(
            &units[port.selection_range.start.character as usize
                ..port.selection_range.end.character as usize],
        )
        .unwrap();
        assert_eq!(selected, "port");
    }

    #[test]
    fn test_array_elements() {
        let content = "(\n    users: [\n        User(id: 1),\n        User(id: 2),\n    ],\n)";
        let symbols = document_symbols(&parse(content), content);

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "users");
        let users = symbols[0].children.as_ref().unwrap();
        assert_eq!(users.len(), 2);
        assert!(users.iter().all(|s| s.name == "User"));
    }

    /// The outline is built recursively, so a document nested far enough used
    /// to exhaust the stack and abort the process. Editors ask for the outline
    /// on their own, without the user requesting anything, so merely opening
    /// such a file was enough to take the server down.
    ///
    /// If the depth guard regresses this test does not fail, it kills the test
    /// binary with a stack overflow, which is the point.
    #[test]
    fn test_deeply_nested_document_yields_no_symbols() {
        let depth = ts_utils::MAX_TREE_DEPTH * 20;
        let content = format!("{}1{}", "Outer(inner: ".repeat(depth), ")".repeat(depth));
        assert!(document_symbols(&parse(&content), &content).is_empty());
    }

    /// Nothing stops a RON document from being written on a single line, and a
    /// generated one usually is. Deriving each symbol's range by scanning the
    /// row it sits on then costs the length of the whole document per symbol,
    /// and the outline has a symbol per value and per field: at 16000 values on
    /// one line a release build took 4.5 seconds, against 61ms for the same
    /// values one per line, and doubling the document quadrupled it. Editors ask
    /// for the outline themselves after every change, so that is the server
    /// unusable on a file it can otherwise handle.
    ///
    /// The bound below is deliberately loose. What it tests is the complexity
    /// class, not the constant.
    #[test]
    fn test_outline_cost_does_not_grow_with_the_square_of_the_row() {
        let values = 4000;
        let content = format!("Config(items: [{}])", "Item(host: \"x\"), ".repeat(values));

        let started = std::time::Instant::now();
        let symbols = document_symbols(&parse(&content), &content);
        let elapsed = started.elapsed();

        let items = symbols[0].children.as_ref().expect("Config has fields");
        let entries = items[0].children.as_ref().expect("items has entries");
        assert_eq!(entries.len(), values, "every entry of the list is outlined");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "outlining {values} values on one line took {elapsed:?}"
        );
    }
}
