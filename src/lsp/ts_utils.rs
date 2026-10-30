/// Tree-sitter utilities for RON LSP
/// This module provides high-level helpers for working with tree-sitter AST nodes
use std::borrow::Cow;
use std::cell::RefCell;

use tower_lsp_server::ls_types::{Position, Range};
use tree_sitter::{Node, Parser, Point, Tree};

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
        self.parse_with(content, None).0
    }

    /// Re-parse content reusing a previous (edited) tree for incremental
    /// parsing.
    ///
    /// Alongside the tree comes whether it may be handed back here as
    /// `old_tree` after the next edit. It may not when `content` holds a raw
    /// string: tree-sitter was then shown [masked](mask_raw_strings) text, and
    /// an edit can change the masking of bytes well outside the edited range
    /// (opening a `"` earlier on turns a later raw string into string
    /// contents), which an incremental re-parse would never look at again.
    /// For the same reason `old_tree` is ignored when `content` holds one.
    pub fn parse_with(&mut self, content: &str, old_tree: Option<&Tree>) -> (Option<Tree>, bool) {
        match mask_raw_strings(content) {
            Cow::Borrowed(text) => (self.parser.parse(text, old_tree), true),
            Cow::Owned(text) => (self.parser.parse(&text, None), false),
        }
    }
}

/// `content` with every raw string (`r"..."`, `r#"..."#`, ...) rewritten as an
/// ordinary quoted string of exactly the same length, for tree-sitter to parse
/// in its place.
///
/// The RON grammar lexes a raw string as `r`, any `#`s, `"`, as much as it can
/// of the rest of the line, and a last `"`. That is the longest match, not the
/// first closing delimiter, so `host: r"h", user: "u"` reads as one field whose
/// value runs to the last quote on the line — `user` vanishes from the tree and
/// is reported missing — and a raw string spanning lines does not lex at all.
///
/// Masking keeps every byte offset, row and column where it was, so the nodes
/// tree-sitter returns still line up with `content`, and their text is still
/// read from `content`: only the tree's shape comes from the masked text. Only
/// ASCII bytes are rewritten, so the result stays valid UTF-8. The opening
/// `r`/`#`s/`"` become `"` and filler, the contents keep everything but the
/// `"`s and `\`s that would end or escape a quoted string, and the closing
/// `"`/`#`s become filler and `"`.
///
/// Comments, strings and chars are skipped the way RON lexes them, so an `r"`
/// inside one is left alone. Borrowed when there is nothing to mask, which is
/// almost always.
pub fn mask_raw_strings(content: &str) -> Cow<'_, str> {
    let bytes = content.as_bytes();
    let mut masked: Option<Vec<u8>> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = memchr(b'\n', bytes, i).unwrap_or(bytes.len());
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                // RON's block comments nest.
                let mut depth = 1;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                // `r"..."`, or `br"..."`, whose `b` the grammar handles as it
                // does on any byte string.
                let r = match &bytes[start..i] {
                    b"r" => start,
                    b"br" => start + 1,
                    _ => continue,
                };
                let hashes = bytes[i..].iter().take_while(|&&b| b == b'#').count();
                let open_quote = i + hashes;
                if bytes.get(open_quote) != Some(&b'"') {
                    // A raw identifier such as `r#type`, or a plain `r`.
                    continue;
                }
                let body = open_quote + 1;
                let mut close = None;
                let mut j = body;
                while let Some(quote) = memchr(b'"', bytes, j) {
                    let end = quote + 1 + hashes;
                    if bytes.len() >= end && bytes[quote + 1..end].iter().all(|&b| b == b'#') {
                        close = Some(quote);
                        break;
                    }
                    j = quote + 1;
                }
                // Unterminated: everything after it is its contents, so there is
                // nothing further to find, and nothing that masking would help.
                let Some(close) = close else { break };
                let end = close + 1 + hashes;

                let out = masked.get_or_insert_with(|| bytes.to_vec());
                out[r] = b'"';
                for b in &mut out[r + 1..body] {
                    *b = b'x';
                }
                for b in &mut out[body..close] {
                    if matches!(*b, b'"' | b'\\') {
                        *b = b'x';
                    }
                }
                for b in &mut out[close..end - 1] {
                    *b = b'x';
                }
                out[end - 1] = b'"';
                i = end;
            }
            _ => i += 1,
        }
    }
    match masked {
        // Only ASCII bytes were replaced, and only with ASCII bytes.
        Some(out) => Cow::Owned(String::from_utf8(out).expect("masking keeps UTF-8 valid")),
        None => Cow::Borrowed(content),
    }
}

/// The index of the first `needle` in `bytes` at or after `from`.
fn memchr(needle: u8, bytes: &[u8], from: usize) -> Option<usize> {
    bytes
        .get(from..)?
        .iter()
        .position(|&b| b == needle)
        .map(|at| from + at)
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
/// backed by the thread-local reusable parser. See [`RonParser::parse_with`]
/// for what the `bool` says.
pub fn parse_with(content: &str, old_tree: Option<&Tree>) -> (Option<Tree>, bool) {
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
    position_on_row(content, line_start(content, offset), offset, row)
}

/// The LSP position of the byte offset `offset`, which falls on row `row` and
/// whose row begins at byte `row_start`.
///
/// The UTF-16 column is the width of what precedes the offset on its own row,
/// which [`utf16_len`] answers without looking at the rest of the document.
fn position_on_row(content: &str, row_start: usize, offset: usize, row: usize) -> Position {
    Position {
        line: row as u32,
        character: utf16_len(&content[row_start..offset]),
    }
}

/// The LSP position of the byte offset a tree-sitter [`Point`] describes.
///
/// A `Point` carries the row *and the byte column* of the offset, and
/// tree-sitter counts that column from the start of the row exactly as
/// [`line_start`] does — so the row's start is a subtraction away, where
/// [`byte_offset_to_position`] has to search the text backwards for the newline
/// that precedes the offset. Every node range the server hands the editor is
/// two of these conversions, and a large document is converted tens of
/// thousands of times per request, so skipping that search is worth the
/// narrower entry point.
fn point_to_position(content: &str, byte_offset: usize, point: Point) -> Position {
    let offset = floor_char_boundary(content, byte_offset);

    // The point and the content only disagree if the tree was parsed from
    // different text than it is being read against; fall back to finding the
    // row's start in the text rather than slicing at an offset it doesn't have.
    match offset.checked_sub(point.column) {
        Some(row_start) if content.is_char_boundary(row_start) => {
            position_on_row(content, row_start, offset, point.row)
        }
        _ => byte_offset_to_position(content, offset, point.row),
    }
}

/// The LSP position of a node's first byte.
pub fn node_start_position(node: &Node, content: &str) -> Position {
    point_to_position(content, node.start_byte(), node.start_position())
}

/// The LSP position just past a node's last byte.
pub fn node_end_position(node: &Node, content: &str) -> Position {
    point_to_position(content, node.end_byte(), node.end_position())
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
///
/// Sliced out of `content` rather than read through `Node::utf8_text`, which
/// takes the source as bytes and so has to validate the node's whole span as
/// UTF-8 to hand back a `&str`. `content` is already a `&str`, so the only
/// thing left to check is that the node's bounds fall on character boundaries
/// — which `str::get` answers by looking at two bytes instead of at every byte
/// of the node. Every value and every name the server reads goes through here,
/// and the root value's span is the whole document, so the difference is a pass
/// over the file per level of nesting.
pub fn node_text<'a>(node: &Node, content: &'a str) -> Option<&'a str> {
    content.get(node.start_byte()..node.end_byte())
}

/// How long a row has to be before [`LineIndex`] checkpoints it, and so how
/// much of one a single conversion can end up counting.
const CHECKPOINT_BYTES: usize = 1024;

/// Byte offset to [`Position`] for a request that converts many of them, in
/// time that doesn't depend on how long the rows are.
///
/// [`byte_offset_to_position`] finds the start of the row by scanning backwards
/// from the offset, then counts the UTF-16 width of everything between: one
/// conversion costs as much as the row is long. A request converting a single
/// position can afford that. One converting a position per node — the outline
/// carries two ranges for every value and every field, and a rename one for
/// every occurrence of the field — pays it per node, so the request costs the
/// document's size multiplied by its longest row. A RON document written on a
/// single line makes those the same number and the request quadratic, and
/// nothing stops a RON document from being written on a single line.
///
/// This records the UTF-16 column at intervals along the rows long enough to be
/// worth it, leaving a conversion on one to count at most
/// [`CHECKPOINT_BYTES`] of it. Rows shorter than that are left to
/// [`byte_offset_to_position`], which already costs no more than they are long,
/// so an ordinary document is indexed by doing nothing and converted exactly as
/// it was before.
pub struct LineIndex {
    /// `(byte offset, row, UTF-16 column)` at each checkpoint, by ascending
    /// offset. Empty for a document whose every row is short.
    checkpoints: Vec<(usize, u32, u32)>,
}

impl LineIndex {
    /// Index the rows of `content` that are long enough to need it.
    pub fn new(content: &str) -> Self {
        let mut checkpoints = Vec::new();
        let mut row_start = 0;

        for (row, line) in content.split('\n').enumerate() {
            // Only a long row is walked character by character; splitting on
            // `\n` to find out is a pass over the bytes and nothing more.
            if line.len() > CHECKPOINT_BYTES {
                let mut column = 0u32;
                let mut next = CHECKPOINT_BYTES;
                for (byte, ch) in line.char_indices() {
                    if byte >= next {
                        checkpoints.push((row_start + byte, row as u32, column));
                        next = byte + CHECKPOINT_BYTES;
                    }
                    column += ch.len_utf16() as u32;
                }
            }
            row_start += line.len() + 1; // + the '\n' that split consumed
        }

        Self { checkpoints }
    }

    /// The LSP position of `byte_offset`, which falls on `row`.
    ///
    /// Takes the row the way [`byte_offset_to_position`] does — from the caller,
    /// which has it from tree-sitter — and clamps the offset the same way, so
    /// the two cannot disagree about where an offset lands.
    pub fn position(&self, content: &str, byte_offset: usize, row: usize) -> Position {
        let offset = floor_char_boundary(content, byte_offset);
        let passed = self
            .checkpoints
            .partition_point(|&(byte, _, _)| byte <= offset)
            .checked_sub(1)
            .map(|last| self.checkpoints[last])
            .filter(|&(_, checkpoint_row, _)| checkpoint_row as usize == row);

        match passed {
            // Carry on counting from the last checkpoint passed on this row.
            Some((from, _, counted)) => Position {
                line: row as u32,
                character: counted + utf16_len(&content[from..offset]),
            },
            // The row holds no checkpoint, so it is short enough to scan.
            None => byte_offset_to_position(content, offset, row),
        }
    }

    /// The LSP range `node` spans: the indexed counterpart of
    /// [`node_to_lsp_range`].
    pub fn node_range(&self, node: &Node, content: &str) -> Range {
        Range {
            start: self.position(content, node.start_byte(), node.start_position().row),
            end: self.position(content, node.end_byte(), node.end_position().row),
        }
    }
}

/// Whether a node is a RON comment (`// ...` line or `/* ... */` block).
pub fn is_comment(node: &Node) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

/// The deepest parse tree the server will walk.
///
/// Tree-sitter parses iteratively and happily builds a tree as deep as the
/// document nests, but the walks over it here are recursive, so a document
/// nested deeply enough exhausts the thread's stack. That is not an error a
/// server can recover from: the runtime aborts the whole process, taking the
/// language server down with whatever file the editor happened to open. The
/// walks therefore refuse to start on a tree deeper than this.
///
/// Nothing a RON document can legitimately contain comes close. `ron` itself
/// gives up past 64 levels of nesting, which is a tree about 130 nodes deep, so
/// a document that trips this limit already fails to deserialize and is already
/// reported as a syntax error.
pub const MAX_TREE_DEPTH: usize = 256;

/// Whether `node`'s subtree is nested deeper than [`MAX_TREE_DEPTH`].
///
/// Deliberately iterative: a recursive depth check would blow the stack on
/// exactly the input it exists to reject.
///
/// A subtree is only ever as deep as it has nodes, so one holding fewer nodes
/// than the walk has levels left cannot reach the limit however it is shaped,
/// and is skipped whole. Tree-sitter keeps that node count on the node itself,
/// so the question costs one field read. An ordinary document is mostly small
/// subtrees — a value, its fields, their literals — which is why this matters:
/// the guard runs before every outline request and every format, and visiting
/// every node of the document to find out it is shallow cost more than the
/// request it was guarding.
pub fn exceeds_max_depth(node: &Node) -> bool {
    let mut stack = vec![(*node, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_TREE_DEPTH {
            return true;
        }
        // A subtree is never deeper than it has nodes, and tree-sitter keeps
        // that count on the node itself, so a subtree with no room left to
        // exceed the limit needs no walking at all.
        if node.descendant_count() <= MAX_TREE_DEPTH - depth + 1 {
            continue;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push((child, depth + 1));
        }
    }
    false
}

/// Find the deepest node at a given position
pub fn node_at_position<'a>(tree: &'a Tree, content: &str, position: Position) -> Option<Node<'a>> {
    let byte_offset = position_to_byte_offset(content, position);
    let root = tree.root_node();
    root.descendant_for_byte_range(byte_offset, byte_offset)
}

/// Every node from the root of `tree` down to `node`, outermost first, with
/// `node` itself last.
///
/// `Node::parent` is not the single step it looks like. Tree-sitter stores no
/// parent pointers: it answers `parent` by descending from the root of the tree
/// again, so one parent costs the node's depth and a node's whole ancestry,
/// asked for one `parent` at a time, costs the *square* of it. Resolving what
/// a value's type is means collecting every struct value it is nested in, which
/// is that ancestry, so on a deeply nested document the square is the
/// difference between answering a request and hanging the editor that sent it.
///
/// Descending from the root once costs the depth instead. The descent is
/// steered by `node`'s byte range: a node's ancestor at each level is the one
/// child of that level spanning the range, so following it down arrives at
/// `node` having passed through exactly its ancestors.
pub fn ancestor_chain<'a>(tree: &'a Tree, node: Node<'a>) -> Vec<Node<'a>> {
    let mut chain = vec![tree.root_node()];
    while let Some(&current) = chain.last() {
        if current.id() == node.id() {
            return chain;
        }
        let mut cursor = current.walk();
        let spanning = current.children(&mut cursor).find(|child| {
            child.start_byte() <= node.start_byte() && node.end_byte() <= child.end_byte()
        });
        match spanning {
            Some(child) => chain.push(child),
            // The descent lost `node` — because it belongs to another tree, or
            // because two nodes sharing a byte range sent it down the wrong
            // one. Nothing here needs either to be impossible: fall back to
            // the walk up, which is slow but cannot miss.
            None => break,
        }
    }

    let mut chain = Vec::new();
    let mut next = Some(node);
    while let Some(current) = next {
        chain.push(current);
        next = current.parent();
    }
    chain.reverse();
    chain
}

/// Iterate over `node` and each of its ancestors, innermost first (the node
/// itself comes first, then its parent, and so on up to the root of `tree`).
///
/// Prefer [`ancestor_chain`] where the ancestry is wanted outermost first, or
/// where more than one node's ancestry is: an ancestry costs a descent from
/// the root, so a walk that asks per value should carry the answer down with
/// it instead — see [`owning_field_for_children`].
pub fn ancestors<'a>(tree: &'a Tree, node: Node<'a>) -> impl Iterator<Item = Node<'a>> {
    ancestor_chain(tree, node).into_iter().rev()
}

/// Find the first ancestor of a node with a given kind
pub fn find_ancestor_by_kind<'a>(tree: &'a Tree, node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    ancestors(tree, node).skip(1).find(|n| n.kind() == kind)
}

/// The owning field name to hand a node's children, given the one that reached
/// the node itself — the walking-down form of the question
/// [`super::tree_sitter_parser::struct_type_context`] needs answered.
///
/// A value's owning field is the innermost `field` ancestor above it, with the
/// search cut off at the first enclosing `struct`: a value nested directly
/// inside another struct value — the `Inner(..)` of `Some(Inner(..))` — is that
/// struct's payload rather than the value of any field. Wrappers that aren't
/// structs are threaded through, so an array element still finds the field the
/// array belongs to.
///
/// Threading the answer down a walk costs nothing. Asking for it per value
/// means walking back up per value, which is what makes a walk over a nested
/// document quadratic — see [`ancestor_chain`].
pub fn owning_field_for_children<'a>(
    node: &Node,
    content: &'a str,
    owning_field: Option<&'a str>,
) -> Option<&'a str> {
    if node.kind() == "struct" {
        None
    } else {
        field_name(node, content).or(owning_field)
    }
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

/// `node`'s named children of a given kind, in document order.
///
/// Reached through tree-sitter's indexed accessor rather than a [`TreeCursor`].
/// A cursor visits *every* child — each comma, colon and paren between the
/// ones being looked for — and costs several times as much per visit as the
/// indexed accessor does, which only ever stops on the named ones. The indexed
/// accessor is in turn linear in the index, so this is for nodes whose children
/// are few: a struct value's, which are its fields and their punctuation.
/// A container of unbounded width — an array's elements — keeps the cursor,
/// where walking the punctuation costs less than re-descending per element.
///
/// [`TreeCursor`]: tree_sitter::TreeCursor
fn named_children_by_kind<'a>(node: &Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut results = Vec::new();
    for index in 0..node.named_child_count() {
        if let Some(child) = node.named_child(index)
            && child.kind() == kind
        {
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

    // Field structure: identifier ":" value — so the value is the child just
    // past the colon. Reached through the indexed accessor rather than a
    // `TreeCursor`, which costs several times as much per child visited (see
    // [`named_children_by_kind`]); a field has three children, so the
    // accessor's linear scan never has anything to scan.
    for index in 0..node.child_count() {
        let Some(child) = node.child(index) else {
            break;
        };
        if child.kind() == ":" {
            // A trailing colon has no value after it; fall through to the
            // fallback rather than reporting the colon itself.
            if let Some(value) = node.child(index + 1) {
                return Some(value);
            }
            break;
        }
    }

    // Fallback: the last child that isn't punctuation.
    node.named_child(node.named_child_count().checked_sub(1)?)
}

/// Find all fields in a struct node (non-recursive, only direct children)
pub fn struct_fields<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    if node.kind() != "struct" {
        return Vec::new();
    }

    named_children_by_kind(node, "field")
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

/// The element values of an `array` node, with punctuation and comments left
/// out. Empty for any other node kind.
pub fn array_elements<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    if node.kind() != "array" {
        return Vec::new();
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !is_comment(child))
        .collect()
}

/// The element values of a `tuple` node — `("a", 2)`, which is how RON writes
/// both a Rust tuple and a fixed-size array — with punctuation and comments
/// left out. Empty for any other node kind.
///
/// A `MISSING` node is left out too: an unfinished tuple such as `("a",)`
/// parses as its one element plus a `MISSING unit` standing in for what the
/// trailing comma promised, and counting that as an element would make the
/// tuple look one element longer than it is.
pub fn tuple_elements<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    if node.kind() != "tuple" {
        return Vec::new();
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !is_comment(child) && !child.is_missing())
        .collect()
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

/// The single value inside an explicit `Some(...)`, which is how RON spells an
/// inhabited `Option`. `None` for every other shape, including the bare `value`
/// form that RON also accepts and a struct that happens to be named `Some`.
pub fn unwrap_some<'a>(node: &Node<'a>, content: &str) -> Option<Node<'a>> {
    if struct_name(node, content) != Some("Some") {
        return None;
    }
    match struct_values(node, content).as_slice() {
        [only] => Some(*only),
        _ => None,
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

    /// Collect all descendant nodes of a given kind, depth-first.
    fn descendants_by_kind<'a>(tree: &'a Tree, kind: &str) -> Vec<Node<'a>> {
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

    /// A RON document nesting `depth` structs inside each other.
    fn nested(depth: usize) -> String {
        format!("{}1{}", "Outer(inner: ".repeat(depth), ")".repeat(depth))
    }

    /// Every node of a document, breadth-first from the root.
    fn all_nodes<'a>(tree: &'a Tree) -> Vec<Node<'a>> {
        let mut nodes = vec![tree.root_node()];
        let mut visited = 0;
        while visited < nodes.len() {
            let node = nodes[visited];
            visited += 1;
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            nodes.extend(children);
        }
        nodes
    }

    /// The ancestry a descent from the root collects has to be exactly the one
    /// walking up with `Node::parent` gives. The walk up is the definition;
    /// the descent is only the way to get it without paying the depth over
    /// again at every level.
    #[test]
    fn test_ancestor_chain_matches_the_walk_up() {
        let documents = [
            "Config()",
            "Config(port: 8080)",
            "Config(\n    server: Server(\n        host: \"münchen\",\n    ),\n)",
            "Config(servers: [Server(port: 1), (port: 2)], mode: Prod)",
            "Some(Inner(a: [1, 2, {\"k\": (b: 3)}]))",
            "Config(\n    // a comment\n    port: 1, /* and another */\n)",
            // Documents the grammar can only make an error node of still have
            // an ancestry, and the handlers still ask for it.
            "Config(port: ",
            "(((",
            "Config(port: 1) trailing garbage",
            // Deep enough that a chain of more than a handful of nodes is
            // checked; the walk up it is compared against is quadratic, so
            // deeper than this costs the test suite more than it proves.
            &nested(40),
        ];

        for content in documents {
            let tree = parse(content).expect("fixture must parse");
            let nodes = all_nodes(&tree);
            assert!(nodes.len() > 1, "fixture must have children: {content:?}");

            for node in nodes {
                let mut expected = Vec::new();
                let mut next = Some(node);
                while let Some(current) = next {
                    expected.push(current.id());
                    next = current.parent();
                }
                expected.reverse();

                let chain: Vec<usize> =
                    ancestor_chain(&tree, node).iter().map(|n| n.id()).collect();
                assert_eq!(
                    chain,
                    expected,
                    "chain for {} in {content:?}",
                    node.to_sexp()
                );
            }
        }
    }

    /// A value's owning field is the innermost `field` above it, and the
    /// search for it stops at the first enclosing `struct`: a struct's own
    /// payload belongs to no field of it. Threaded down a walk, that means
    /// each node hands its children either its own field name, nothing (if it
    /// is a struct), or whatever it was handed.
    #[test]
    fn test_owning_field_for_children() {
        let content = "Config(server: Some([(port: 1)]))";
        let tree = parse(content).unwrap();

        let field_of = |kind: &str, owning| {
            let node = descendants_by_kind(&tree, kind)
                .into_iter()
                .next()
                .expect("fixture must contain the node");
            owning_field_for_children(&node, content, owning)
        };

        // A field hands its name down, overriding anything above it.
        assert_eq!(field_of("field", Some("outer")), Some("server"));
        // A struct cuts the thread: its payload is not the value of a field.
        assert_eq!(field_of("struct", Some("server")), None);
        // Anything else passes on what it was handed, so an array element
        // still finds the field the array belongs to.
        assert_eq!(field_of("array", Some("server")), Some("server"));
    }

    #[test]
    fn test_exceeds_max_depth_passes_realistic_documents() {
        // The deepest document `ron` will deserialize must still be walkable,
        // or formatting and the outline would quietly stop working on a file
        // the rest of the server considers valid.
        let content = nested(64);
        assert!(
            ron::from_str::<ron::Value>(&content).is_ok(),
            "fixture must be a document ron accepts"
        );
        let tree = parse(&content).unwrap();
        assert!(!exceeds_max_depth(&tree.root_node()));
    }

    #[test]
    fn test_exceeds_max_depth_rejects_deeply_nested_documents() {
        let tree = parse(&nested(MAX_TREE_DEPTH + 1)).unwrap();
        assert!(exceeds_max_depth(&tree.root_node()));
    }

    /// [`exceeds_max_depth`] skips a subtree whose node count leaves no room to
    /// reach the limit, so what it answers now depends on arithmetic rather than
    /// only on the walk. Check it against the walk it replaced, over every
    /// nesting depth either side of the limit: a guard that fires one level
    /// early silently costs a legitimate document its outline and its
    /// formatting, and one that fires one level late is a stack overflow that
    /// takes the server down.
    #[test]
    fn test_exceeds_max_depth_matches_a_full_walk_at_every_depth() {
        /// The whole-tree walk, with no subtree skipped.
        fn full_walk(node: &Node) -> bool {
            let mut stack = vec![(*node, 1usize)];
            while let Some((node, depth)) = stack.pop() {
                if depth > MAX_TREE_DEPTH {
                    return true;
                }
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    stack.push((child, depth + 1));
                }
            }
            false
        }

        // Each shape nests a different number of tree levels per level of
        // document nesting, so between them they cross the limit at different
        // points and no single off-by-one can satisfy them all.
        let shapes: [fn(usize) -> String; 3] = [
            nested,
            |d| format!("{}1{}", "[".repeat(d), "]".repeat(d)),
            |d| format!("{}1{}", "Some(".repeat(d), ")".repeat(d)),
        ];
        for shape in shapes {
            for depth in 0..=MAX_TREE_DEPTH + 2 {
                let content = shape(depth);
                let tree = parse(&content).unwrap();
                let root = tree.root_node();
                assert_eq!(
                    exceeds_max_depth(&root),
                    full_walk(&root),
                    "disagreed on a document nesting {depth} levels"
                );
            }
        }
    }

    /// Each raw string becomes a quoted string of the same length, and nothing
    /// else moves: the tree's offsets, rows and columns all index `content`.
    #[test]
    fn test_mask_raw_strings() {
        let cases = [
            (r#"(a: r"h", b: "u")"#, r#"(a: "xh", b: "u")"#),
            (r##"(a: r#"q"x"#, b: 1)"##, r#"(a: "xxqxxx", b: 1)"#),
            (r##"(a: r#"\"#)"##, r#"(a: "xxxx")"#),
            (r##"(a: br#"z"#)"##, r#"(a: b"xxzx")"#),
            ("(a: r\"x\ny\")", "(a: \"xx\ny\")"),
            // Non-ASCII contents are left alone.
            ("(a: r\"é🚀\")", "(a: \"xé🚀\")"),
        ];
        for (content, expected) in cases {
            let masked = mask_raw_strings(content);
            assert_eq!(masked, expected, "masking {content}");
            assert_eq!(masked.len(), content.len());
        }
    }

    /// What only looks like a raw string is left as it is, and the common case
    /// of no raw string at all costs no copy.
    #[test]
    fn test_mask_raw_strings_leaves_lookalikes() {
        for content in [
            r#"(a: "r\"x\"", b: 1)"#,
            r#"(a: "x", // r"y"
                b: 1)"#,
            r#"(a: /* /* r"y" */ r"z" */ 1)"#,
            r#"(a: '"', b: r#type)"#,
            r#"(bar: 1, var: "x")"#,
            r#"(a: r"unterminated, b: 1)"#,
        ] {
            assert!(
                matches!(mask_raw_strings(content), Cow::Borrowed(_)),
                "{content} was masked"
            );
        }
    }

    /// The tree parsed from masked text has the shape the real text has: one
    /// field per field, each value a `string` spanning exactly its literal.
    #[test]
    fn test_parse_raw_string_fields() {
        let content = r##"User(a: r#"x"#, b: "y", c: r"z")"##;
        let tree = parse(content).unwrap();
        let value = find_main_value(&tree).unwrap();
        let values: Vec<_> = struct_fields(&value)
            .iter()
            .map(|field| {
                let value = field_value(field).unwrap();
                (
                    field_name(field, content).unwrap(),
                    value.kind(),
                    node_text(&value, content).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            values,
            [
                ("a", "string", r##"r#"x"#"##),
                ("b", "string", r#""y""#),
                ("c", "string", r#"r"z""#),
            ]
        );
    }

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

    /// A struct's fields come back in document order with the punctuation and
    /// the comments between them left out. Pinned because the fields are
    /// collected by walking tree-sitter's *named* children, and a comment is a
    /// named child too.
    #[test]
    fn test_struct_fields_in_order_past_comments() {
        let content = "User(\n    // who\n    id: 1, // the id\n    /* and */ name: \"a\",\n)";
        let tree = parse(content).unwrap();
        let value = find_main_value(&tree).unwrap();

        let names: Vec<_> = struct_fields(&value)
            .iter()
            .map(|field| field_name(field, content).unwrap())
            .collect();
        assert_eq!(names, ["id", "name"]);
    }

    /// A field's value is whatever its colon is followed by, at every field of
    /// the struct and not just the first.
    #[test]
    fn test_field_value_follows_the_colon() {
        let content = "User(id: 1, name: \"a\", tags: [1, 2])";
        let tree = parse(content).unwrap();
        let value = find_main_value(&tree).unwrap();

        let values: Vec<_> = struct_fields(&value)
            .iter()
            .map(|field| node_text(&field_value(field).unwrap(), content).unwrap())
            .collect();
        assert_eq!(values, ["1", "\"a\"", "[1, 2]"]);
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

    /// The outline and a rename convert a position per node, so they go through
    /// [`LineIndex`] instead of scanning the row each offset lands on. The two
    /// have to answer identically at every offset: the indexed answer is what
    /// the editor is given, and a column off by one highlights the wrong text.
    #[test]
    fn test_line_index_agrees_with_scanning_the_row() {
        // Rows long enough to carry several checkpoints each, of differing
        // lengths so offsets land both just after a checkpoint and far past
        // one, with CRLF endings, a two-byte character and a surrogate pair.
        let mut content = String::new();
        for row in 0..4 {
            content.push_str(&"aä🚀b".repeat(row * 150 + 150));
            content.push_str(if row % 2 == 0 { "\r\n" } else { "\n" });
        }
        // A short row too, which is left to the unindexed path.
        content.push_str("trailing");

        let index = LineIndex::new(&content);
        assert!(
            index.checkpoints.len() > 8,
            "the fixture has to checkpoint its rows several times over"
        );

        // Every offset, mid-character ones included: both snap back to the
        // character boundary before them, and have to snap to the same one.
        let mut row = 0;
        for offset in 0..=content.len() {
            assert_eq!(
                index.position(&content, offset, row),
                byte_offset_to_position(&content, offset, row),
                "offset {offset} on row {row} should land in the same place either way"
            );
            if content.as_bytes().get(offset) == Some(&b'\n') {
                row += 1;
            }
        }

        // And an offset past the end of the document is clamped into it.
        assert_eq!(
            index.position(&content, content.len() + 99, row),
            byte_offset_to_position(&content, content.len(), row)
        );
    }

    /// An empty document has no row to index and no offset that isn't the start
    /// of it, which still has to convert rather than reach past the end.
    #[test]
    fn test_line_index_of_empty_document() {
        let index = LineIndex::new("");
        assert_eq!(index.position("", 0, 0), Position::new(0, 0));
        assert_eq!(index.position("", 10, 0), Position::new(0, 0));
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

    /// Node positions are derived from the byte column tree-sitter reports for
    /// the node, rather than by counting rows forward from the start of the
    /// document. The two have to answer identically for every node — including
    /// on lines carrying multi-byte and astral characters, where a byte offset
    /// and a UTF-16 column come apart.
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

    /// A node's position is read off the byte column its own tree recorded,
    /// which only describes the text that tree was parsed from. Reading a tree
    /// against text it doesn't belong to is a bug in the caller, but it must
    /// not take the server down: a panic here would abort the process over one
    /// stale document. Every offset is clamped into the content at hand, and a
    /// column the content cannot account for falls back to finding the row's
    /// start in it.
    #[test]
    fn test_node_ranges_against_mismatched_content() {
        let parsed = "Config(\n    city: \"münchen\",\n    port: 8080,\n)";
        let tree = parse(parsed).unwrap();

        // Each of these truncates or re-encodes the text the tree describes,
        // so the nodes' byte offsets and columns no longer fit it.
        for other in ["", "x", "Config(\n", "ü", "Config(\n    city: \"m"] {
            let mut stack = vec![tree.root_node()];
            while let Some(node) = stack.pop() {
                let range = node_to_lsp_range(&node, other);
                assert!(
                    range.start.character as usize <= other.len(),
                    "column of {range:?} should be bounded by {other:?}"
                );

                let mut cursor = node.walk();
                stack.extend(node.children(&mut cursor));
            }
        }
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
