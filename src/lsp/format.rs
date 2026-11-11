/// Tree-sitter based RON formatter with comment preservation
/// This formatter uses the AST to properly handle formatting while preserving comments
use super::ts_utils;
use tree_sitter::{Node, Tree};

/// One level of indentation. The formatter has never offered a choice of width,
/// so it is a constant rather than something every layout step carries.
const INDENT: &str = "    ";

/// Represents a comment found in the source
#[derive(Debug, Clone)]
struct Comment {
    /// The comment's text. A line comment in a CRLF document runs up to the
    /// `\n`, so its `\r` is cut off here: the formatter ends the line itself.
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
                text: text.trim_end_matches('\r').to_string(),
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

/// Format RON content using tree-sitter AST, preserving comments.
///
/// For a whole open document, prefer [`format_tree`]: the server already holds
/// that document's parse tree, and parsing a large file again costs more than
/// laying it out does.
pub fn format_ron(content: &str) -> String {
    format_ron_with_line_ending(content, ts_utils::line_ending(content))
}

/// [`format_ron`] ending every line it lays out with `newline` rather than with
/// whatever `content` itself uses.
///
/// For text cut out of a larger document — a range-formatting selection — whose
/// own line endings, if it even has a line break, don't say what the document's
/// are.
pub fn format_ron_with_line_ending(content: &str, newline: &'static str) -> String {
    match ts_utils::parse(content) {
        Some(tree) => layout(&tree, content, newline),
        // If parsing fails, return original content
        None => content.to_string(),
    }
}

/// [`format_ron`] for content whose parse tree the caller already has.
///
/// `tree` must be the tree of `content`: the layout is read off the tree and
/// the text of each leaf is read out of `content` at the byte offsets the tree
/// recorded, so a tree belonging to different text would reproduce that text's
/// contents.
pub fn format_tree(tree: &Tree, content: &str) -> String {
    layout(tree, content, ts_utils::line_ending(content))
}

/// Lay out `content`, whose parse tree is `tree`, ending each line written with
/// `newline`. Comments and other leaves are copied verbatim, line breaks inside
/// them included.
fn layout(tree: &Tree, content: &str, newline: &'static str) -> String {
    // Laying the document out is recursive, so a tree too deep to walk is left
    // alone, exactly as an unparseable one is.
    if ts_utils::exceeds_max_depth(&tree.root_node()) {
        return content.to_string();
    }

    // So is a tree tree-sitter had to recover from a syntax error. Its ERROR
    // and MISSING nodes don't sit where the text has them — a half-typed
    // `timeout: ` has its name and its colon split into separate values — so
    // laying one out writes back something other than what was there:
    // `timeout,\n:,`. Editors format on save, which is exactly when a document
    // is most often half-typed, so the text is left alone until it parses.
    if tree.root_node().has_error() {
        return content.to_string();
    }

    let Some(main_value) = ts_utils::find_main_value(tree) else {
        return String::new();
    };

    let mut formatter = Formatter {
        content,
        newline,
        output: String::new(),
        malformed: false,
        prev_end: None,
    };

    // The document's top level is laid out like the inside of a container, one
    // piece per line: the `#![enable(...)]` header (and a type annotation, if
    // the grammar found one) copied as written, then the root value. Comments
    // keep their place around them — above a piece, after it on its line, or
    // at the end of the file. Writing out only the root value deleted the
    // header, and with `implicit_some` gone a document `ron` accepted no longer
    // loaded.
    let root = tree.root_node();
    let comments = collect_inner_comments(&root, content);
    let mut cursor = root.walk();
    let mut pieces: Vec<Node> = root
        .named_children(&mut cursor)
        .filter(|child| {
            child.end_byte() <= main_value.start_byte()
                && matches!(child.kind(), "extensions" | "extension" | "type_annotation")
        })
        .collect();
    pieces.push(main_value);

    for (i, piece) in pieces.iter().enumerate() {
        for comment in &comments {
            if !comment.is_trailing
                && comment.end_byte <= piece.start_byte()
                && (i == 0 || comment.start_byte > pieces[i - 1].end_byte())
            {
                formatter.own_line_comment(comment, 0);
            }
        }

        formatter.separate_from_previous(piece.start_byte());
        if piece.id() == main_value.id() {
            formatter.node(piece, 0);
        } else if let Some(text) = ts_utils::node_text(piece, content) {
            formatter.output.push_str(text);
        }
        formatter.prev_end = Some(piece.end_byte());

        formatter.trailing_comment(&comments, &pieces, i);
        formatter.output.push_str(newline);
    }

    for comment in &comments {
        if !comment.is_trailing && comment.start_byte > main_value.end_byte() {
            formatter.own_line_comment(comment, 0);
        }
    }

    // The grammar accepts a few things RON doesn't, without an ERROR node to
    // show for it; the layout notices them on the way and they are left alone
    // for the same reason a syntax error is.
    if formatter.malformed {
        return content.to_string();
    }

    formatter.output.trim_end().to_string()
}

/// The document being laid out, and the text being laid out from it.
///
/// Every step of the layout needs the source — to read each node's text out of
/// — and somewhere to write, and neither changes as it descends. Holding both
/// here leaves each step taking only what actually varies: the node, and how
/// deep it sits.
struct Formatter<'a> {
    content: &'a str,
    /// What every line written ends with: the document's own line terminator.
    newline: &'static str,
    output: String,
    /// Set when the tree holds something RON can't parse that tree-sitter
    /// accepted anyway, and that the layout would lose part of.
    malformed: bool,
    /// Where the last line-starting piece of the current container (an item,
    /// or a comment on its own line) ends in `content`, or `None` before the
    /// first one. A blank line in the source between it and the next piece is
    /// kept, so the groups a document is written in survive formatting.
    prev_end: Option<usize>,
}

/// True if `gap` holds a line with nothing but whitespace on it.
///
/// The first and last lines of `gap` are the tails of the lines the pieces on
/// either side sit on, so only the lines strictly between them count. That
/// keeps a block comment spanning several lines from reading as a blank line.
fn has_blank_line(gap: &str) -> bool {
    let lines: Vec<&str> = gap.split('\n').collect();
    lines.len() > 2
        && lines[1..lines.len() - 1]
            .iter()
            .any(|l| l.trim().is_empty())
}

impl Formatter<'_> {
    /// Write an empty line before a piece starting at `start` if the source
    /// had at least one between it and the piece before. Several blank lines
    /// are written as one, and nothing is written before a container's first
    /// piece.
    fn separate_from_previous(&mut self, start: usize) {
        if let Some(prev) = self.prev_end
            && prev < start
            && has_blank_line(&self.content[prev..start])
        {
            self.output.push_str(self.newline);
        }
    }

    /// Write `level` levels of indentation.
    fn indent(&mut self, level: usize) {
        for _ in 0..level {
            self.output.push_str(INDENT);
        }
    }

    /// Lay out a single node, recursing into whatever it contains.
    fn node(&mut self, node: &Node, level: usize) {
        match node.kind() {
            "struct" => self.struct_value(node, level),
            "array" => self.container(node, level, ('[', ']'), &ts_utils::array_elements(node)),
            "tuple" => self.container(node, level, ('(', ')'), &ts_utils::tuple_elements(node)),
            "map" => {
                let entries = ts_utils::children_by_kind(node, "map_entry");
                self.container(node, level, ('{', '}'), &entries)
            }
            "field" => self.field(node, level),
            "map_entry" => self.map_entry(node, level),
            // Leaf nodes, and anything the grammar hands us that we don't lay
            // out ourselves, are reproduced verbatim
            _ => {
                if let Some(text) = ts_utils::node_text(node, self.content) {
                    self.output.push_str(text);
                }
            }
        }
    }

    /// Lay out `items` one per line between `delimiters`, with their comments:
    /// leading comments above the item, the item itself at one extra level of
    /// indentation, a terminating comma, and any trailing comment after it.
    /// Comments that follow the last item go before the closing delimiter.
    ///
    /// This is the one layout rule shared by arrays, maps, tuples and
    /// field-style structs — they differ only in their delimiters and in what
    /// their items are. An empty container keeps its delimiters adjacent.
    fn container(
        &mut self,
        node: &Node,
        level: usize,
        (open, close): (char, char),
        items: &[Node<'_>],
    ) {
        self.output.push(open);
        if !items.is_empty() {
            let comments = collect_inner_comments(node, self.content);

            self.output.push_str(self.newline);
            self.prev_end = None;
            for (i, item) in items.iter().enumerate() {
                self.leading_comments(&comments, items, i, level);

                self.separate_from_previous(item.start_byte());
                self.indent(level + 1);
                self.node(item, level + 1);
                self.prev_end = Some(item.end_byte());
                self.output.push(',');

                self.trailing_comment(&comments, items, i);
                self.output.push_str(self.newline);
            }

            self.remaining_comments(&comments, items, level);
            self.indent(level);
        }
        self.output.push(close);
    }

    /// Lay out a struct value: `Name(...)`, or `(...)` when the document leaves
    /// the name to be inferred.
    fn struct_value(&mut self, node: &Node, level: usize) {
        if let Some(name) = ts_utils::struct_name(node, self.content) {
            self.output.push_str(name);
        }

        // A struct is either field-style — `User(id: 1)` — or tuple-style —
        // `Point(1, 2)`, `Some("value")`. Both are containers and lay out as
        // one, bar the single exception below.
        let fields = ts_utils::struct_fields(node);
        let values = ts_utils::struct_values(node, self.content);

        // The grammar lets the two styles mix — `Config(port: 1, debug)` parses
        // as a struct with one field and one value, with no ERROR node — but
        // RON doesn't, and laying out only the fields deleted the bare value.
        // That is a field whose name has been typed and its colon not yet,
        // exactly what format-on-save meets mid-edit.
        if !fields.is_empty() && !values.is_empty() {
            self.malformed = true;
            return;
        }

        if !fields.is_empty() {
            self.container(node, level, ('(', ')'), &fields);
            return;
        }

        // A tuple-style struct holding one value keeps that value on the
        // struct's own line: `Some([` rather than `Some(\n    [`. The value
        // therefore sits at the struct's level and not one deeper — whatever it
        // opens has to close back at the column the struct's own `)` is in.
        //
        // A comment among the values is the one thing that line can't hold, so
        // it gives up the shorter form rather than the comment.
        if let [only] = values.as_slice()
            && collect_inner_comments(node, self.content).is_empty()
        {
            self.output.push('(');
            self.node(only, level);
            self.output.push(')');
            return;
        }

        self.container(node, level, ('(', ')'), &values);
    }

    /// Lay out a `name: value` field of a struct.
    fn field(&mut self, node: &Node, level: usize) {
        if let Some(name) = ts_utils::field_name(node, self.content) {
            self.output.push_str(name);
            self.output.push_str(": ");
        }

        if let Some(value) = ts_utils::field_value(node) {
            self.node(&value, level);
        }
    }

    /// Lay out a `key: value` entry of a map.
    fn map_entry(&mut self, node: &Node, level: usize) {
        let Some((key, value)) = ts_utils::map_entry_halves(node) else {
            return;
        };

        self.node(&key, level);
        self.output.push_str(": ");
        self.node(&value, level);
    }

    /// Emit the leading (non-trailing) comments that fall before `items[i]` and
    /// after the item before it, one per line at one level deeper than `level`.
    ///
    /// No comment reaches this twice, so nothing records which ones have
    /// already been written. The window each `i` accepts — past `items[i - 1]`
    /// and before `items[i]` — is disjoint from every other `i`'s, from the
    /// window [`Formatter::trailing_comment`] accepts (which wants a *trailing*
    /// comment), and from [`Formatter::remaining_comments`]' (which starts past
    /// the last item). A comment belongs to exactly one of them, and the three
    /// only ever look at one node's own children.
    fn leading_comments(
        &mut self,
        comments: &[Comment],
        items: &[Node<'_>],
        i: usize,
        level: usize,
    ) {
        for comment in comments {
            if !comment.is_trailing
                && comment.end_byte <= items[i].start_byte()
                && (i == 0 || comment.start_byte > items[i - 1].end_byte())
            {
                self.own_line_comment(comment, level + 1);
            }
        }
    }

    /// Emit the single trailing comment (if any) that belongs after `items[i]`
    /// and before the next item.
    fn trailing_comment(&mut self, comments: &[Comment], items: &[Node<'_>], i: usize) {
        let next_start = items.get(i + 1).map(|n| n.start_byte());
        let trailing = comments.iter().find(|comment| {
            comment.is_trailing
                && comment.start_byte > items[i].end_byte()
                && next_start.is_none_or(|start| comment.end_byte <= start)
        });

        if let Some(comment) = trailing {
            self.output.push(' ');
            self.output.push_str(&comment.text);
        }
    }

    /// Emit the non-trailing comments that follow the last item in a container.
    fn remaining_comments(&mut self, comments: &[Comment], items: &[Node<'_>], level: usize) {
        let Some(last) = items.last() else {
            return;
        };
        for comment in comments {
            if !comment.is_trailing && comment.start_byte > last.end_byte() {
                self.own_line_comment(comment, level + 1);
            }
        }
    }

    /// Write `comment` on a line of its own at `level` levels of indentation,
    /// after a blank line if the source had one before it.
    fn own_line_comment(&mut self, comment: &Comment, level: usize) {
        self.separate_from_previous(comment.start_byte);
        self.indent(level);
        self.output.push_str(&comment.text);
        self.output.push_str(self.newline);
        self.prev_end = Some(comment.end_byte);
    }
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

    /// A tuple struct holding one value keeps it on the struct's own line, so
    /// the value belongs at the struct's level of indentation: what it opens on
    /// that line has to close back at the column the struct's own `)` is in.
    /// Laying the value out one level deeper instead pushed every nested
    /// container — which is every `Option` field written `Some([...])` or
    /// `Some(Inner(...))` — four spaces past where it belongs, and left its
    /// closing delimiter hanging inside the struct's.
    #[test]
    fn test_single_value_tuple_struct_indents_its_value_at_its_own_level() {
        assert_eq!(
            format_ron("Root(extra: Some([1, 2]))"),
            "Root(\n    extra: Some([\n        1,\n        2,\n    ]),\n)"
        );
        assert_eq!(
            format_ron("Root(server: Some(Inner(a: 1)))"),
            "Root(\n    server: Some(Inner(\n        a: 1,\n    )),\n)"
        );
        // Nesting one inside another compounded the error four spaces at a time
        assert_eq!(
            format_ron("Root(a: Some(Some([1])))"),
            "Root(\n    a: Some(Some([\n        1,\n    ])),\n)"
        );
        // A root value of its own has nothing above it to be relative to
        assert_eq!(format_ron("Some([1])"), "Some([\n    1,\n])");
        // ...and a value that needs no line of its own still gets none
        assert_eq!(format_ron("Root(x: Some(2))"), "Root(\n    x: Some(2),\n)");
    }

    /// Every other container terminates its items with a comma; a tuple struct
    /// separated them instead, so the last value of `Span(1, 2)` came back
    /// without one. RON accepts the trailing comma here as it does everywhere
    /// else, and the README documents one after each item.
    #[test]
    fn test_multi_value_tuple_struct_terminates_its_values() {
        let formatted = format_ron("Root(span: Span(1, 5))");
        assert_eq!(
            formatted,
            "Root(\n    span: Span(\n        1,\n        5,\n    ),\n)"
        );
        assert_eq!(format_ron(&formatted), formatted);
    }

    /// `struct_values` counted a comment as a value, and a comment laid out as a
    /// value got a terminating comma of its own: `Some(/* why */ 1)` came back
    /// as `Some(/* why */, 1)`, which RON rejects — formatting turned a
    /// deserializable document into an unparseable one. The comment is a
    /// comment, and belongs on its own line above the value like any other.
    #[test]
    fn test_comment_in_tuple_struct_stays_a_comment() {
        let formatted = format_ron("Some(/* why */ 1)");
        assert_eq!(formatted, "Some(\n    /* why */\n    1,\n)");
        assert_eq!(
            ron::from_str::<Option<u32>>(&formatted).ok(),
            Some(Some(1)),
            "the formatted document still deserializes: {formatted:?}"
        );
        assert_eq!(format_ron(&formatted), formatted);
    }

    /// And a comment *after* a tuple struct's value is a trailing comment,
    /// which stays on that value's line. Laying the comments out as values put
    /// `// one` on a line of its own below the `1` it describes, and appended a
    /// comma that the line comment then swallowed.
    #[test]
    fn test_trailing_comment_in_tuple_struct_keeps_its_line() {
        let expected = "Point(\n    // first\n    1, // one\n    2,\n)";
        assert_eq!(format_ron("Point(// first\n1, // one\n2)"), expected);
        assert_eq!(format_ron(expected), expected);
    }

    /// Config files group their fields with blank lines, and the formatter
    /// used to delete every one of them: with format-on-save, a file's
    /// sections ran together on the first save. A blank line between two
    /// items, or between an item and a comment above the next one, is kept;
    /// a run of them is collapsed to one, and one just inside a delimiter is
    /// dropped.
    #[test]
    fn test_blank_lines_between_items_are_kept() {
        let input = "// header\n\n\nAppConfig(\n\n    debug: true, // why\n\n\n    // network\n    port: 8080,\n\n    server: ServerConfig(\n        host: \"localhost\",\n\n        mode: Production,\n\n    ),\n    features: [\n        \"a\",\n\n        \"b\",\n    ],\n)\n";
        let expected = "// header\n\nAppConfig(\n    debug: true, // why\n\n    // network\n    port: 8080,\n\n    server: ServerConfig(\n        host: \"localhost\",\n\n        mode: Production,\n    ),\n    features: [\n        \"a\",\n\n        \"b\",\n    ],\n)";
        assert_eq!(format_ron(input), expected);
        assert_eq!(format_ron(expected), expected);
    }

    /// A block comment spanning several lines has no blank line in it, so it
    /// doesn't add one; nor does a comment directly above its item.
    #[test]
    fn test_multiline_block_comment_is_not_a_blank_line() {
        let input =
            "Config(\n    a: 1, /* first\n    second */\n    b: 2,\n    // about c\n    c: 3,\n)";
        assert_eq!(format_ron(input), input);
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

    /// RON's extension header is a node of the document rather than a comment
    /// (`source_file: seq(optional($.extensions), ..., $._value)`). The layout
    /// used to write out only the root value and the comments above it, so the
    /// header was deleted — and with `implicit_some` gone every `Option`
    /// written as a bare value stopped deserializing. It is kept as written.
    #[test]
    fn test_extension_header_is_kept() {
        let input = "#![enable(implicit_some)]\nConfig(port: 8080)";
        assert_eq!(
            format_ron(input),
            "#![enable(implicit_some)]\nConfig(\n    port: 8080,\n)"
        );
    }

    /// Several extensions, the comments among and after them, and the blank
    /// line between the header and the value all keep their place; a comment
    /// on the header's own line stays on it.
    #[test]
    fn test_extension_header_keeps_its_comments_and_blank_lines() {
        let input = "#![enable(implicit_some)] // for Option fields\n// and newtypes\n#![enable(unwrap_newtypes)]\n\n// the config\nConfig(\n    port: 8080,\n)";
        assert_eq!(format_ron(input), input);
    }

    /// A comment after the root value is a sibling of it rather than a child,
    /// so the layout of the root value never saw it and it used to be deleted.
    /// It is kept, on its own line or on the root value's closing line,
    /// whichever it was written on, along with the blank lines before it.
    #[test]
    fn test_comment_after_root_value_is_kept() {
        let formatted = format_ron(
            "Config(port: 8080) // end\n// keep this in sync with nginx\n\n/* last */\n",
        );
        assert_eq!(
            formatted,
            "Config(\n    port: 8080,\n) // end\n// keep this in sync with nginx\n\n/* last */"
        );
    }

    /// The header and the comments around it are written with the document's
    /// own line endings, and formatting the result again changes nothing.
    #[test]
    fn test_header_and_trailing_comment_in_crlf_document_are_stable() {
        let input = "#![enable(implicit_some)]\r\nConfig(port: 8080)\r\n// tail\r\n";
        let formatted = format_ron(input);
        assert_eq!(
            formatted,
            "#![enable(implicit_some)]\r\nConfig(\r\n    port: 8080,\r\n)\r\n// tail"
        );
        assert_eq!(format_ron(&formatted), formatted);
    }

    /// A document with no value in it is one tree-sitter reports a missing
    /// value for, so it is left alone like any other that doesn't parse. It
    /// used to be laid out as nothing at all, and the server turned that into
    /// an edit emptying the file.
    #[test]
    fn test_document_with_no_value_is_left_alone() {
        let input = "// TODO: fill this in\n";
        assert_eq!(format_ron(input), input);
    }

    /// A half-typed document doesn't parse, and the tree tree-sitter recovers
    /// for it has its pieces in the wrong places: laying it out turned
    /// `timeout: ` into `timeout,\n:,`. Format-on-save hands the server exactly
    /// these documents, so they come back unchanged.
    #[test]
    fn test_document_with_syntax_error_is_left_alone() {
        for input in [
            "AppConfig(\n    server: ServerConfig(timeout: ),\n)\n",
            "AppConfig(\n    port: 1,\n    host: \n)\n",
            "Config(port: 8080\n",
        ] {
            assert_eq!(format_ron(input), input);
        }
    }

    /// A field name typed without its colon yet — `debug` among `name: value`
    /// fields — isn't RON, but the grammar takes it as a tuple-style value in
    /// a field-style struct and reports no error. Laying out only the fields
    /// deleted it, so format-on-save threw away what was being typed. It is
    /// left alone like any other document that doesn't parse.
    #[test]
    fn test_struct_mixing_fields_and_values_is_left_alone() {
        for input in [
            "AppConfig(\n    port: 1,\n    debug\n)\n",
            "AppConfig(\n    debug,\n    port: 1,\n)\n",
            "AppConfig(\n    server: ServerConfig(host: \"h\", mode),\n)\n",
            "(port: 1, true)\n",
        ] {
            assert_eq!(format_ron(input), input);
        }
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

    /// A CRLF document is laid out with CRLF lines. It used to come back with
    /// every line ending in a bare `\n`, so formatting it rewrote every line of
    /// the file, and an already formatted one could never be left alone.
    #[test]
    fn test_crlf_document_keeps_crlf() {
        let input = "// top\r\nConfig(\r\n    // lead\r\n    port: 1, // trail\r\n    tags: [\"a\"],\r\n    inner: Inner(a: 1),\r\n    /* done */\r\n)\r\n";
        let expected = "// top\r\nConfig(\r\n    // lead\r\n    port: 1, // trail\r\n    tags: [\r\n        \"a\",\r\n    ],\r\n    inner: Inner(\r\n        a: 1,\r\n    ),\r\n    /* done */\r\n)";
        assert_eq!(format_ron(input), expected);
        assert_eq!(format_ron(expected), expected, "formatting is idempotent");

        // A selection without a line break of its own takes the document's.
        assert_eq!(
            format_ron_with_line_ending("Inner(a: 1)", "\r\n"),
            "Inner(\r\n    a: 1,\r\n)"
        );
        assert_eq!(format_ron("Inner(a: 1)"), "Inner(\n    a: 1,\n)");
    }

    /// The blank line kept between groups of a CRLF document is a CRLF line
    /// too, not a bare `\n` in the middle of the file.
    #[test]
    fn test_crlf_blank_lines_are_kept_as_crlf() {
        let input =
            "// top\r\n\r\nConfig(\r\n    a: 1,\r\n\r\n\r\n    // net\r\n    b: 2,\r\n)\r\n";
        let expected = "// top\r\n\r\nConfig(\r\n    a: 1,\r\n\r\n    // net\r\n    b: 2,\r\n)";
        assert_eq!(format_ron(input), expected);
        assert_eq!(format_ron(expected), expected, "formatting is idempotent");
    }
}
