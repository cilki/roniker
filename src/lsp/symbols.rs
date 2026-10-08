//! Document symbols: a nested outline of the RON document built from the
//! cached tree-sitter parse tree.
use super::ts_utils;
use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};
use tree_sitter::{Node, Tree};

/// Build the document outline for a RON file.
pub fn document_symbols(tree: &Tree, content: &str) -> Vec<DocumentSymbol> {
    let Some(main_value) = ts_utils::find_main_value(tree) else {
        return Vec::new();
    };
    value_symbols(&main_value, content)
}

/// Symbols for a value node: named structs become container symbols,
/// anonymous structs contribute their fields directly, arrays recurse.
fn value_symbols(node: &Node, content: &str) -> Vec<DocumentSymbol> {
    match node.kind() {
        "struct" => {
            let children: Vec<DocumentSymbol> = ts_utils::struct_fields(node)
                .iter()
                .filter_map(|f| field_symbol(f, content))
                .collect();

            match ts_utils::struct_name(node, content) {
                Some(name) => vec![symbol(
                    name.to_string(),
                    SymbolKind::STRUCT,
                    node,
                    node.child(0).as_ref().unwrap_or(node),
                    children,
                    content,
                )],
                // Anonymous struct: hoist its fields to the parent level
                None => children,
            }
        }
        "array" | "tuple" => ts_utils::named_children(node)
            .iter()
            .flat_map(|child| value_symbols(child, content))
            .collect(),
        _ => Vec::new(),
    }
}

/// Symbol for one `name: value` field, with nested symbols from its value.
fn field_symbol(field_node: &Node, content: &str) -> Option<DocumentSymbol> {
    let name = ts_utils::field_name(field_node, content)?;
    let name_node = field_node.child(0)?;

    let children = ts_utils::field_value(field_node)
        .map(|value| value_symbols(&value, content))
        .unwrap_or_default();

    Some(symbol(
        name.to_string(),
        SymbolKind::FIELD,
        field_node,
        &name_node,
        children,
        content,
    ))
}

#[allow(deprecated)] // DocumentSymbol::deprecated must still be initialized
fn symbol(
    name: String,
    kind: SymbolKind,
    node: &Node,
    selection_node: &Node,
    children: Vec<DocumentSymbol>,
    content: &str,
) -> DocumentSymbol {
    DocumentSymbol {
        name,
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range: ts_utils::node_to_lsp_range(node, content),
        selection_range: ts_utils::node_to_lsp_range(selection_node, content),
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
}
