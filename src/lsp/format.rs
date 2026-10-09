/// Tree-sitter based RON formatter with comment preservation
/// This formatter uses the AST to properly handle formatting while preserving comments
use super::ts_utils;
use std::collections::HashSet;
use tree_sitter::Node;

/// Represents a comment found in the source
#[derive(Debug, Clone)]
struct Comment {
    text: String,
    /// The byte position where this comment starts
    start_byte: usize,
    /// The byte position where this comment ends
    end_byte: usize,
    /// True if this comment appears on the same line as code before it
    is_trailing: bool,
}

/// Collect comments that are direct children of a container node
/// Returns comments with their positions relative to sibling nodes
fn collect_inner_comments(node: &Node, content: &str) -> Vec<Comment> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();

    for (i, child) in children.iter().enumerate() {
        if ts_utils::is_comment(child)
            && let Some(text) = ts_utils::node_text(child, content)
        {
            let start_byte = child.start_byte();

            // Determine if this is a trailing comment by checking if there's
            // a non-comment, non-punctuation sibling before it on the same line
            let is_trailing = is_trailing_comment_in_context(content, start_byte, &children[..i]);

            result.push(Comment {
                text: text.to_string(),
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
                is_trailing,
            });
        }
    }

    result
}

/// Determine if a comment is trailing by checking if there's a significant sibling
/// (field, value, etc.) before it on the same line
fn is_trailing_comment_in_context(
    content: &str,
    comment_start: usize,
    siblings_before: &[Node],
) -> bool {
    // Find the line start for the comment
    let before = &content[..comment_start];
    let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);

    // Check if any significant sibling ends on this same line (after line_start)
    for sibling in siblings_before.iter().rev() {
        let kind = sibling.kind();
        // Skip punctuation like ( ) , :
        if !sibling.is_named() {
            continue;
        }
        // Skip other comments
        if ts_utils::is_comment(sibling) {
            continue;
        }
        // Skip identifiers that are struct names (first child of struct)
        // We only want to consider fields and values as trailing anchors
        if kind == "identifier" {
            // Check if this identifier is the first named child (struct name)
            // by seeing if there are any fields/values before it
            let has_field_before = siblings_before
                .iter()
                .any(|s| s.kind() == "field" || s.kind() == "map_entry");
            if !has_field_before {
                // This is likely the struct name, skip it
                continue;
            }
        }
        // This is a significant node - check if it ends on or after the line start
        if sibling.end_byte() > line_start {
            return true;
        }
        // If this sibling is on a previous line, stop looking
        break;
    }

    false
}

/// Collect top-level comments (direct children of source_file that come before the main value)
fn collect_top_level_comments(root: &Node, content: &str, main_value: &Node) -> Vec<Comment> {
    let mut comments = Vec::new();
    let mut cursor = root.walk();

    for child in root.children(&mut cursor) {
        if ts_utils::is_comment(&child)
            && child.end_byte() <= main_value.start_byte()
            && let Some(text) = ts_utils::node_text(&child, content)
        {
            comments.push(Comment {
                text: text.to_string(),
                start_byte: child.start_byte(),
                end_byte: child.end_byte(),
                is_trailing: false,
            });
        }
    }

    comments.sort_by_key(|c| c.start_byte);
    comments
}

/// Emit any not-yet-emitted leading (non-trailing) comments that fall before
/// `items[i]` and after the previous item. The struct, array, and map
/// formatters all lay leading comments out identically; this is that shared
/// logic. Comments are indented one level deeper than `indent_level`.
fn emit_leading_comments(
    output: &mut String,
    comments: &[Comment],
    emitted: &mut HashSet<usize>,
    items: &[Node<'_>],
    i: usize,
    indent_str: &str,
    indent_level: usize,
) {
    let item = &items[i];
    for comment in comments {
        if emitted.contains(&comment.start_byte) {
            continue;
        }
        if !comment.is_trailing
            && comment.end_byte <= item.start_byte()
            && (i == 0 || comment.start_byte > items[i - 1].end_byte())
        {
            output.push_str(&indent_str.repeat(indent_level + 1));
            output.push_str(&comment.text);
            output.push('\n');
            emitted.insert(comment.start_byte);
        }
    }
}

/// Emit the single trailing comment (if any) that belongs after `items[i]` and
/// before the next item. At most one comment is emitted.
fn emit_trailing_comment(
    output: &mut String,
    comments: &[Comment],
    emitted: &mut HashSet<usize>,
    items: &[Node<'_>],
    i: usize,
) {
    let item = &items[i];
    for comment in comments {
        if emitted.contains(&comment.start_byte) {
            continue;
        }
        if comment.is_trailing && comment.start_byte > item.end_byte() {
            let next_start = items.get(i + 1).map(|n| n.start_byte());
            let is_before_next = next_start.map(|s| comment.end_byte <= s).unwrap_or(true);
            if is_before_next {
                output.push(' ');
                output.push_str(&comment.text);
                emitted.insert(comment.start_byte);
                break;
            }
        }
    }
}

/// Emit any not-yet-emitted, non-trailing comments that follow the last item in
/// a container.
fn emit_remaining_comments(
    output: &mut String,
    comments: &[Comment],
    emitted: &mut HashSet<usize>,
    items: &[Node<'_>],
    indent_str: &str,
    indent_level: usize,
) {
    let Some(last) = items.last() else {
        return;
    };
    for comment in comments {
        if emitted.contains(&comment.start_byte) {
            continue;
        }
        if comment.start_byte > last.end_byte() && !comment.is_trailing {
            output.push_str(&indent_str.repeat(indent_level + 1));
            output.push_str(&comment.text);
            output.push('\n');
            emitted.insert(comment.start_byte);
        }
    }
}

/// The named children of a node that carry data, with comments filtered out.
fn value_children<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    ts_utils::named_children(node)
        .into_iter()
        .filter(|n| !ts_utils::is_comment(n))
        .collect()
}

/// Lay out the items of a container one per line, with their comments: leading
/// comments above the item, the item itself at one extra level of indentation,
/// a terminating comma, and any trailing comment after it. Comments that follow
/// the last item are emitted before the caller's closing delimiter.
///
/// This is the one layout rule shared by structs, arrays, maps and tuples —
/// they differ only in the delimiters, which the caller writes, and in what
/// their items are. Nothing at all is written for an empty container, so the
/// caller's delimiters stay adjacent.
fn format_items(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
    items: &[Node<'_>],
) {
    if items.is_empty() {
        return;
    }

    let comments = collect_inner_comments(node, content);

    output.push('\n');
    for (i, item) in items.iter().enumerate() {
        emit_leading_comments(
            output,
            &comments,
            emitted,
            items,
            i,
            indent_str,
            indent_level,
        );

        output.push_str(&indent_str.repeat(indent_level + 1));
        format_node(item, content, output, indent_level + 1, indent_str, emitted);
        output.push(',');

        emit_trailing_comment(output, &comments, emitted, items, i);
        output.push('\n');
    }

    emit_remaining_comments(output, &comments, emitted, items, indent_str, indent_level);
    output.push_str(&indent_str.repeat(indent_level));
}

/// Format RON content using tree-sitter AST, preserving comments
pub fn format_ron(content: &str) -> String {
    let indent_str = "    "; // 4 spaces

    let tree = match ts_utils::parse(content) {
        Some(t) => t,
        None => {
            // If parsing fails, return original content
            return content.to_string();
        }
    };

    // Laying the document out is recursive, so a tree too deep to walk is left
    // alone, exactly as an unparseable one is.
    if ts_utils::exceeds_max_depth(&tree.root_node()) {
        return content.to_string();
    }

    // Build the formatted output
    let mut result = String::new();

    // Format the main value
    if let Some(main_value) = ts_utils::find_main_value(&tree) {
        // Collect and emit top-level comments
        let top_comments = collect_top_level_comments(&tree.root_node(), content, &main_value);
        for comment in &top_comments {
            result.push_str(&comment.text);
            result.push('\n');
        }

        // Track which comments we've emitted to avoid duplicates
        let mut emitted_comments = HashSet::new();
        for comment in &top_comments {
            emitted_comments.insert(comment.start_byte);
        }

        format_node(
            &main_value,
            content,
            &mut result,
            0,
            indent_str,
            &mut emitted_comments,
        );
    }

    result.trim_end().to_string()
}

/// Format a single node recursively
fn format_node(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
) {
    match node.kind() {
        "struct" => format_struct(node, content, output, indent_level, indent_str, emitted),
        "array" => format_array(node, content, output, indent_level, indent_str, emitted),
        "map" => format_map(node, content, output, indent_level, indent_str, emitted),
        "tuple" => format_tuple(node, content, output, indent_level, indent_str, emitted),
        "field" => format_field(node, content, output, indent_level, indent_str, emitted),
        "map_entry" => format_map_entry(node, content, output, indent_level, indent_str, emitted),
        // Leaf nodes, and anything the grammar hands us that we don't lay out
        // ourselves, are reproduced verbatim
        _ => {
            if let Some(text) = ts_utils::node_text(node, content) {
                output.push_str(text);
            }
        }
    }
}

/// Format a struct node
fn format_struct(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
) {
    // Get struct name if it exists
    if let Some(name) = ts_utils::struct_name(node, content) {
        output.push_str(name);
    }

    if ts_utils::is_empty_structure(node) {
        output.push_str("()");
        return;
    }

    output.push('(');

    // A struct is either field-style — `User(id: 1)`, laid out like every other
    // container — or tuple-style, which has its own rules
    let fields = ts_utils::struct_fields(node);
    if fields.is_empty() {
        format_struct_values(node, content, output, indent_level, indent_str, emitted);
    } else {
        format_items(
            node,
            content,
            output,
            indent_level,
            indent_str,
            emitted,
            &fields,
        );
    }

    output.push(')');
}

/// Format the values of a tuple-style struct (`Some("value")`, `Point(1, 2)`).
/// These are the one container whose items are *separated* rather than
/// terminated by commas, and a lone value stays on the struct's own line, so
/// they don't go through `format_items`.
fn format_struct_values(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
) {
    let values = ts_utils::struct_values(node, content);
    if values.is_empty() {
        return;
    }

    let comments = collect_inner_comments(node, content);
    let inline = values.len() == 1;

    if !inline {
        output.push('\n');
    }

    for (i, value) in values.iter().enumerate() {
        if !inline {
            emit_leading_comments(
                output,
                &comments,
                emitted,
                &values,
                i,
                indent_str,
                indent_level,
            );
            output.push_str(&indent_str.repeat(indent_level + 1));
        }

        format_node(
            value,
            content,
            output,
            indent_level + 1,
            indent_str,
            emitted,
        );

        if i + 1 < values.len() {
            output.push(',');
            output.push(if inline { ' ' } else { '\n' });
        }
    }

    if !inline {
        output.push('\n');
        output.push_str(&indent_str.repeat(indent_level));
    }
}

/// Format a field node
fn format_field(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
) {
    // Get field name
    if let Some(name) = ts_utils::field_name(node, content) {
        output.push_str(name);
        output.push_str(": ");
    }

    // Get field value
    if let Some(value) = ts_utils::field_value(node) {
        format_node(&value, content, output, indent_level, indent_str, emitted);
    }
}

/// Format a map entry node (`key: value`)
fn format_map_entry(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
) {
    let children = value_children(node);
    if children.len() < 2 {
        return;
    }

    format_node(
        &children[0],
        content,
        output,
        indent_level,
        indent_str,
        emitted,
    );
    output.push_str(": ");
    format_node(
        &children[1],
        content,
        output,
        indent_level,
        indent_str,
        emitted,
    );
}

/// Format an array node
fn format_array(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
) {
    if ts_utils::is_empty_structure(node) {
        output.push_str("[]");
        return;
    }

    output.push('[');
    format_items(
        node,
        content,
        output,
        indent_level,
        indent_str,
        emitted,
        &value_children(node),
    );
    output.push(']');
}

/// Format a map node
fn format_map(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
) {
    if ts_utils::is_empty_structure(node) {
        output.push_str("{}");
        return;
    }

    output.push('{');
    format_items(
        node,
        content,
        output,
        indent_level,
        indent_str,
        emitted,
        &ts_utils::children_by_kind(node, "map_entry"),
    );
    output.push('}');
}

/// Format a tuple node
fn format_tuple(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    emitted: &mut HashSet<usize>,
) {
    output.push('(');
    format_items(
        node,
        content,
        output,
        indent_level,
        indent_str,
        emitted,
        &value_children(node),
    );
    output.push(')');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_struct() {
        let input = "User(id: 1, name: \"Alice\")";
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("User("));
        assert!(formatted.contains("    id: 1,"));
        assert!(formatted.contains("    name: \"Alice\","));
        assert!(formatted.contains(")"));
    }

    #[test]
    fn test_empty_parens() {
        let input = "Unit()";
        let formatted = format_ron(input);
        println!("Formatted: '{}'", formatted);
        assert_eq!(formatted, "Unit()");
    }

    #[test]
    fn test_nested_struct() {
        let input = "Post(author: User(id: 1))";
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("Post("));
        assert!(formatted.contains("    author: User("));
        assert!(formatted.contains("        id: 1,"));
        assert!(formatted.contains("    )"));
        assert!(formatted.trim().ends_with(")"));
    }

    #[test]
    fn test_array() {
        let input = r#"Config(roles: ["admin", "user"])"#;
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("roles: ["));
        assert!(formatted.contains(r#"        "admin","#));
        assert!(formatted.contains(r#"        "user","#));
        assert!(formatted.contains("    ],") || formatted.contains("    ]\n)"));
    }

    // Comment preservation tests

    #[test]
    fn test_top_level_line_comment() {
        let input = "// This is a config file\nUser(id: 1)";
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("// This is a config file"));
        assert!(formatted.contains("User("));
        assert!(formatted.contains("    id: 1,"));
    }

    #[test]
    fn test_top_level_block_comment() {
        let input = "/* Config for user */\nUser(id: 1)";
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("/* Config for user */"));
        assert!(formatted.contains("User("));
    }

    #[test]
    fn test_comment_before_field() {
        let input = "User(\n    // The user's ID\n    id: 1,\n)";
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("    // The user's ID"));
        assert!(formatted.contains("    id: 1,"));
    }

    #[test]
    fn test_inline_comment_after_field() {
        let input = "User(\n    id: 1, // User identifier\n)";
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("id: 1, // User identifier"));
    }

    #[test]
    fn test_multiple_comments() {
        let input = r#"// Top comment
User(
    // Comment for id
    id: 1, // ID value
    // Comment for name
    name: "Alice", // Name value
)"#;
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("// Top comment"));
        assert!(formatted.contains("    // Comment for id"));
        assert!(formatted.contains("id: 1, // ID value"));
        assert!(formatted.contains("    // Comment for name"));
        assert!(formatted.contains("name: \"Alice\", // Name value"));
    }

    #[test]
    fn test_comment_in_array() {
        let input = r#"Config(
    roles: [
        // Admin role
        "admin",
        // User role
        "user",
    ],
)"#;
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("        // Admin role"));
        assert!(formatted.contains("        \"admin\","));
        assert!(formatted.contains("        // User role"));
        assert!(formatted.contains("        \"user\","));
    }

    #[test]
    fn test_block_comment_inside_struct() {
        let input = "User(/* user id */ id: 1)";
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("/* user id */"));
        assert!(formatted.contains("id: 1,"));
    }

    #[test]
    fn test_map_entries_and_nested_containers() {
        let input = r#"Config(
    m: {
        // key a
        "a": 1, // one
        "b": [1, 2, { "deep": (3, 4) }],
        // tail
    },
)"#;
        let expected = r#"Config(
    m: {
        // key a
        "a": 1, // one
        "b": [
            1,
            2,
            {
                "deep": (
                    3,
                    4,
                ),
            },
        ],
        // tail
    },
)"#;
        assert_eq!(format_ron(input), expected);
        // Formatting an already-formatted document is a no-op
        assert_eq!(format_ron(expected), expected);
    }

    #[test]
    fn test_tuple_elements_with_comments() {
        let input = r#"Config(
    t: (
        // first
        1, // one
        (2, 3),
        // tail
    ),
)"#;
        let expected = r#"Config(
    t: (
        // first
        1, // one
        (
            2,
            3,
        ),
        // tail
    ),
)"#;
        assert_eq!(format_ron(input), expected);
        assert_eq!(format_ron(expected), expected);
    }

    #[test]
    fn test_empty_containers() {
        assert_eq!(
            format_ron("Config(a: [], b: {}, c: Unit())"),
            "Config(\n    a: [],\n    b: {},\n    c: Unit(),\n)"
        );
    }

    #[test]
    fn test_nested_struct_with_comments() {
        let input = r#"// Post configuration
Post(
    // Author information
    author: User(
        // The author's ID
        id: 1,
    ),
)"#;
        let formatted = format_ron(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("// Post configuration"));
        assert!(formatted.contains("    // Author information"));
        assert!(formatted.contains("        // The author's ID"));
    }

    /// Laying out a document is recursive, so a document nested far enough
    /// used to exhaust the stack — aborting the whole language server process,
    /// which no amount of error handling in the editor can survive. A document
    /// that deep is left as it is instead.
    ///
    /// If the depth guard regresses this test does not fail, it kills the test
    /// binary with a stack overflow, which is the point.
    #[test]
    fn test_deeply_nested_document_is_left_alone() {
        let depth = ts_utils::MAX_TREE_DEPTH * 20;
        let input = format!("{}1{}", "Outer(inner: ".repeat(depth), ")".repeat(depth));
        assert_eq!(format_ron(&input), input);
    }
}
