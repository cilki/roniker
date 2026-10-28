mod code_actions;
mod completion;
mod diagnostics;
mod format;
mod navigation;
mod symbols;
mod tree_sitter_parser;
mod ts_utils;
mod type_utils;

use crate::rust_analyzer::{self, RustAnalyzer};

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tower_service::Service;

/// The JSON-RPC message types the server loop deals in, named out of the way of
/// the `lsp_types` glob above.
type RpcRequest = tower_lsp::jsonrpc::Request;
type RpcResponse = tower_lsp::jsonrpc::Response;

#[derive(Clone)]
pub struct Document {
    content: String,
    // Parsed once per change and shared by all request handlers.
    // Cloning a Tree is cheap (tree-sitter trees are copy-on-write).
    tree: Option<tree_sitter::Tree>,
    // Whether `tree` can seed an incremental re-parse after an edit; see
    // `ts_utils::RonParser::parse_with`.
    tree_reusable: bool,
}

impl Document {
    pub fn new(content: String) -> Self {
        let (tree, tree_reusable) = ts_utils::parse_with(&content, None);
        Self {
            content,
            tree,
            tree_reusable,
        }
    }

    /// Apply a single LSP content change: incremental when a range is given,
    /// full-document replacement otherwise.
    pub fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
        let Some(range) = change.range else {
            *self = Document::new(change.text);
            return;
        };

        // A range whose end precedes its start is not something the protocol
        // allows, but splicing with a backwards range panics, and a panic here
        // takes the whole server down along with every other open document. Put
        // the two ends back in order instead.
        let (range_start, range_end) = order(range.start, range.end);

        let start_byte = ts_utils::position_to_byte_offset(&self.content, range_start);
        let old_end_byte = ts_utils::position_to_byte_offset(&self.content, range_end);

        // tree-sitter Points use byte columns
        let start_position = byte_point(&self.content, start_byte, range_start.line as usize);
        let old_end_position = byte_point(&self.content, old_end_byte, range_end.line as usize);

        self.content
            .replace_range(start_byte..old_end_byte, &change.text);

        let new_end_byte = start_byte + change.text.len();
        let new_end_position = match change.text.rfind('\n') {
            Some(last_newline) => tree_sitter::Point {
                row: start_position.row + change.text.matches('\n').count(),
                column: change.text.len() - last_newline - 1,
            },
            None => tree_sitter::Point {
                row: start_position.row,
                column: start_position.column + change.text.len(),
            },
        };

        if !self.tree_reusable {
            self.tree = None;
        }
        if let Some(tree) = self.tree.as_mut() {
            tree.edit(&tree_sitter::InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte,
                start_position,
                old_end_position,
                new_end_position,
            });
        }
        (self.tree, self.tree_reusable) = ts_utils::parse_with(&self.content, self.tree.as_ref());
    }
}

/// The two positions in document order, swapped if they arrive reversed.
fn order(a: Position, b: Position) -> (Position, Position) {
    if (a.line, a.character) <= (b.line, b.character) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Compute the tree-sitter Point (row + byte column) for a byte offset.
fn byte_point(content: &str, byte_offset: usize, line: usize) -> tree_sitter::Point {
    let line_start = content[..byte_offset]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    tree_sitter::Point {
        row: line,
        column: byte_offset - line_start,
    }
}

pub struct Backend {
    pub client: Client,
    pub documents: Arc<RwLock<HashMap<String, Document>>>,
    pub rust_analyzer: Arc<rust_analyzer::RustAnalyzer>,
    pub info_diagnostics: bool,
}

impl Backend {
    pub fn new(client: Client, analyzer: RustAnalyzer, info_diagnostics: bool) -> Self {
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            rust_analyzer: Arc::new(analyzer),
            info_diagnostics,
        }
    }

}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Log workspace info (types are now added via add_file/add_source methods)
        if let Some(workspace_folders) = &params.workspace_folders
            && let Some(folder) = workspace_folders.first()
        {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("Workspace root: {}", folder.uri.as_str()),
                )
                .await;
        }

        // Log registered types
        let types = self.rust_analyzer.get_all_types();
        self.client
            .log_message(
                MessageType::INFO,
                format!("Registered {} types", types.len()),
            )
            .await;
        for type_info in types.iter().take(10) {
            self.client
                .log_message(MessageType::INFO, format!("  - {}", type_info.name))
                .await;
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        ":".to_string(),
                        " ".to_string(),
                        ",".to_string(),
                    ]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_symbol_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "RON LSP server initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let doc = Document::new(params.text_document.text);

        self.documents
            .write()
            .await
            .insert(uri.clone(), doc.clone());

        // Publish diagnostics using root type from rust_analyzer
        self.publish_diagnostics(&uri, &doc).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();

        let doc = {
            let mut documents = self.documents.write().await;
            let doc = documents
                .entry(uri.clone())
                .or_insert_with(|| Document::new(String::new()));
            for change in params.content_changes {
                doc.apply_change(change);
            }
            doc.clone()
        };

        // Publish diagnostics using root type from rust_analyzer
        self.publish_diagnostics(&uri, &doc).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let doc = {
            let documents = self.documents.read().await;
            documents.get(&uri).cloned()
        };
        if let Some(doc) = doc {
            self.publish_diagnostics(&uri, &doc).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        self.documents.write().await.remove(&uri);
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        let documents = self.documents.read().await;
        let Some(doc) = documents.get(&uri) else {
            return Ok(None);
        };
        let (content, tree) = (&doc.content, doc.tree.as_ref());

        // Get the word at cursor position - early return if none
        let Some(word) = get_word_at_position(content, position) else {
            return Ok(None);
        };

        let owner = self.resolve_owner_type_at(content, tree, position);
        let Some(symbol) = self.resolve_symbol(owner, tree, content, position, &word) else {
            return Ok(None);
        };

        let (source_file, line, column) = match symbol {
            Symbol::Field { owner, field } => (&owner.source_file, field.line, field.column),
            Symbol::Variant { owner, variant } => {
                (&owner.source_file, variant.line, variant.column)
            }
            Symbol::Type(info) => (&info.source_file, info.line, info.column),
        };
        create_location_response(source_file, line, column)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        let documents = self.documents.read().await;
        let Some(doc) = documents.get(&uri) else {
            return Ok(None);
        };
        let (content, tree) = (&doc.content, doc.tree.as_ref());

        // Get the word at cursor position
        let Some(word) = get_word_at_position(content, position) else {
            return Ok(None);
        };

        let owner = self.resolve_owner_type_at(content, tree, position);
        let Some(symbol) = self.resolve_symbol(owner, tree, content, position, &word) else {
            return Ok(None);
        };

        let value = match symbol {
            Symbol::Field { field, .. } => format!(
                "```rust\n{}: {}\n```\n\n{}",
                field.name,
                field.type_name,
                field.docs.as_deref().unwrap_or("")
            ),
            Symbol::Variant { owner, variant } => format_variant_hover(&owner.name, variant),
            Symbol::Type(info) => self.format_type_hover(info),
        };

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let position = params.text_document_position.position;

        let documents = self.documents.read().await;
        let Some(doc) = documents.get(&uri) else {
            return Ok(None);
        };
        let Some(tree) = doc.tree.as_ref() else {
            return Ok(None);
        };

        let Some(type_info) = self.resolve_owner_type_at(&doc.content, Some(tree), position) else {
            return Ok(None);
        };
        let completions = completion::generate_completions_for_type(
            tree,
            &doc.content,
            position,
            type_info,
            self.rust_analyzer.clone(),
        );
        Ok(Some(CompletionResponse::Array(completions)))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let url = params.text_document.uri;
        let uri = url.to_string();

        let documents = self.documents.read().await;
        let Some(doc) = documents.get(&uri) else {
            return Ok(None);
        };
        let Some(tree) = doc.tree.as_ref() else {
            return Ok(None);
        };
        let Some(type_info) = self.resolve_root_type_info(&doc.content, Some(tree)) else {
            return Ok(None);
        };

        let actions = code_actions::generate_code_actions(
            tree,
            &doc.content,
            type_info,
            &url,
            self.rust_analyzer.clone(),
            params.range,
            &params.context.diagnostics,
        );
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.to_string();

        let documents = self.documents.read().await;
        if let Some(doc) = documents.get(&uri) {
            // Basic RON formatting: normalize whitespace and indentation. The
            // document's own tree is laid out directly: it is the tree of
            // exactly this content, and parsing a large file again costs more
            // than the layout itself.
            let formatted = match doc.tree.as_ref() {
                Some(tree) => format::format_tree(tree, &doc.content),
                None => Backend::format_ron(&doc.content),
            };

            if formatted != doc.content {
                let line_count = doc.content.lines().count() as u32;
                let last_line = line_count.saturating_sub(1);
                let last_column = ts_utils::line_width(&doc.content, last_line as usize);
                return Ok(Some(vec![TextEdit {
                    range: Range::new(
                        Position::new(0, 0),
                        Position::new(last_line, last_column),
                    ),
                    new_text: formatted,
                }]));
            }
        }

        Ok(None)
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.to_string();
        // Put a reversed selection back in order: `end_line - start_line` below
        // underflows on one, which panics and takes the server down.
        let (range_start, range_end) = order(params.range.start, params.range.end);
        let range = Range::new(range_start, range_end);

        let documents = self.documents.read().await;
        if let Some(doc) = documents.get(&uri) {
            // Extract the text in the range
            let lines: Vec<&str> = doc.content.lines().collect();
            let start_line = range.start.line as usize;
            let end_line = range.end.line as usize;

            if start_line < lines.len() && end_line < lines.len() {
                // Get the selected text
                let mut selected_text = String::new();
                for (i, line) in lines
                    .iter()
                    .enumerate()
                    .skip(start_line)
                    .take(end_line - start_line + 1)
                {
                    if i == start_line && i == end_line {
                        // Single line selection
                        let start_byte =
                            ts_utils::utf16_column_to_byte(line, range.start.character as usize);
                        let end_byte =
                            ts_utils::utf16_column_to_byte(line, range.end.character as usize);
                        selected_text.push_str(&line[start_byte..end_byte.max(start_byte)]);
                    } else if i == start_line {
                        let start_byte =
                            ts_utils::utf16_column_to_byte(line, range.start.character as usize);
                        selected_text.push_str(&line[start_byte..]);
                        selected_text.push('\n');
                    } else if i == end_line {
                        let end_byte =
                            ts_utils::utf16_column_to_byte(line, range.end.character as usize);
                        selected_text.push_str(&line[..end_byte]);
                    } else {
                        selected_text.push_str(line);
                        selected_text.push('\n');
                    }
                }

                // Format the selected text
                let formatted = Backend::format_ron(&selected_text);

                if formatted != selected_text {
                    return Ok(Some(vec![TextEdit {
                        range,
                        new_text: formatted,
                    }]));
                }
            }
        }

        Ok(None)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();

        let documents = self.documents.read().await;
        let Some(doc) = documents.get(&uri) else {
            return Ok(None);
        };
        let Some(tree) = doc.tree.as_ref() else {
            return Ok(None);
        };

        let symbols = symbols::document_symbols(tree, &doc.content);
        if symbols.is_empty() {
            return Ok(None);
        }
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.to_string();
        let position = params.position;

        let documents = self.documents.read().await;
        let Some(doc) = documents.get(&uri) else {
            return Ok(None);
        };
        let Some(tree) = doc.tree.as_ref() else {
            return Ok(None);
        };

        // Only field name identifiers are renameable
        Ok(field_name_node_at(tree, &doc.content, position)
            .map(|name_node| {
                PrepareRenameResponse::Range(ts_utils::node_to_lsp_range(&name_node, &doc.content))
            }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let url = params.text_document_position.text_document.uri;
        let uri = url.to_string();
        let position = params.text_document_position.position;
        let new_name = params.new_name;

        if !is_valid_identifier(&new_name) {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "'{}' is not a valid identifier",
                new_name
            )));
        }

        let documents = self.documents.read().await;
        let Some(doc) = documents.get(&uri) else {
            return Ok(None);
        };
        let Some(tree) = doc.tree.as_ref() else {
            return Ok(None);
        };
        let content = &doc.content;
        let Some(name_node) = field_name_node_at(tree, content, position) else {
            return Ok(None);
        };
        let Some(field_name) = ts_utils::node_text(&name_node, content) else {
            return Ok(None);
        };

        // Resolve the type that owns the field being renamed, so occurrences of
        // the same name under *other* types are left alone
        let target_type = self.resolve_owner_type_at(content, Some(tree), position);

        // Collect the exact name-node ranges of matching fields. Every
        // occurrence is one byte-offset-to-position conversion, and a document
        // can hold as many occurrences as it is long, so the long rows are
        // indexed once instead of being scanned again for every edit.
        let lines = ts_utils::LineIndex::new(content);
        let mut changes = Vec::new();
        for (node, owner) in self.fields_named(tree, content, field_name) {
            // When either side can't be resolved, fall back to matching by name
            if let (Some(target), Some(owner)) = (target_type, owner)
                && target.name != owner.name
            {
                continue;
            }

            changes.push(TextEdit {
                range: lines.node_range(&node, content),
                new_text: new_name.clone(),
            });
        }

        if changes.is_empty() {
            return Ok(None);
        }

        let mut map = std::collections::HashMap::new();
        map.insert(url, changes);
        Ok(Some(WorkspaceEdit {
            changes: Some(map),
            ..Default::default()
        }))
    }
}

/// What an identifier in a RON document refers to, together with the type that
/// declares it.
///
/// Hover and go-to-definition ask the same question of a word under the cursor
/// and differ only in what they report about the answer — a Markdown
/// description or a source location — so the lookup lives in one place
/// ([`Backend::resolve_symbol`]) and they each render what it returns.
enum Symbol<'a> {
    /// A field of a struct, or of one of `owner`'s enum variants. Both are
    /// declared in `owner`'s source file.
    Field {
        owner: &'a rust_analyzer::TypeInfo,
        field: &'a rust_analyzer::FieldInfo,
    },
    /// A variant of the enum `owner`.
    Variant {
        owner: &'a rust_analyzer::TypeInfo,
        variant: &'a rust_analyzer::EnumVariant,
    },
    /// A struct or enum named by the document.
    Type(&'a rust_analyzer::TypeInfo),
}

/// The identifier node of the field whose *name* contains `position`, if any.
fn field_name_node_at<'a>(
    tree: &'a tree_sitter::Tree,
    content: &str,
    position: Position,
) -> Option<tree_sitter::Node<'a>> {
    let node = ts_utils::node_at_position(tree, content, position)?;
    let field_node = if node.kind() == "field" {
        node
    } else {
        ts_utils::find_ancestor_by_kind(tree, node, "field")?
    };
    let name_node = field_node.child(0)?;

    // The cursor must be on the name itself, not the value
    let byte_offset = ts_utils::position_to_byte_offset(content, position);
    if byte_offset < name_node.start_byte() || byte_offset > name_node.end_byte() {
        return None;
    }
    Some(name_node)
}

/// Whether a proposed rename target is a valid RON identifier
fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

fn create_location_response(
    source_file: &Option<PathBuf>,
    line: Option<usize>,
    column: Option<usize>,
) -> Result<Option<GotoDefinitionResponse>> {
    let source_file = match source_file.as_ref() {
        Some(f) => f,
        None => return Ok(None),
    };

    let line = line.unwrap_or(1).saturating_sub(1) as u32;
    let column = column.unwrap_or(0) as u32;

    let uri = match tower_lsp::lsp_types::Url::from_file_path(source_file) {
        Ok(u) => u,
        Err(_) => return Ok(None),
    };

    Ok(Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: Range::new(Position::new(line, column), Position::new(line, column)),
    })))
}

fn get_word_at_position(content: &str, position: Position) -> Option<String> {
    let line = content.lines().nth(position.line as usize)?;

    // The LSP character offset is a column in UTF-16 code units, not a byte
    // index. Convert it to a char index and work over the line's chars, so a
    // multibyte character (in a string value or comment) earlier on the line
    // neither shifts the word boundaries nor lets us slice mid-codepoint.
    let chars: Vec<char> = line.chars().collect();
    if position.character > ts_utils::utf16_column(line, line.len()) {
        return None;
    }
    let byte_col = ts_utils::utf16_column_to_byte(line, position.character as usize);
    let col = line[..byte_col].chars().count();

    let is_word = |c: char| c.is_alphanumeric() || c == '_';

    // If the cursor sits just after a non-word character, step back one so a
    // word ending at the cursor is still found.
    let actual_col = if col > 0 && !is_word(chars[col - 1]) {
        col - 1
    } else {
        col
    };

    // Find word boundaries around actual_col.
    let start = chars[..actual_col]
        .iter()
        .rposition(|&c| !is_word(c))
        .map_or(0, |i| i + 1);

    let end = chars[actual_col..]
        .iter()
        .position(|&c| !is_word(c))
        .map_or(chars.len(), |i| actual_col + i);

    if start < end {
        let word: String = chars[start..end].iter().collect();
        // Only return identifiers (starts with a letter or underscore, not a number).
        if is_valid_identifier(&word) {
            return Some(word);
        }
    }

    None
}

impl Backend {
    fn format_ron(content: &str) -> String {
        format::format_ron(content)
    }

    /// Resolve the root TypeInfo for a document, either from the configured root type
    /// or by inferring it from the struct name at the top of the RON content.
    ///
    /// Either way the answer is one of the analyzer's registered types, so it
    /// is borrowed from the analyzer — as is everything resolved from it below
    /// — rather than copied, fields, docs and all, once per request.
    fn resolve_root_type_info(
        &self,
        content: &str,
        tree: Option<&tree_sitter::Tree>,
    ) -> Option<&rust_analyzer::TypeInfo> {
        self.rust_analyzer.root_type_info().or_else(|| {
            let main_value = ts_utils::find_main_value(tree?)?;
            let name = ts_utils::struct_name(&main_value, content)?;
            self.rust_analyzer.get_type_info(name)
        })
    }

    /// What the word at `position` refers to, resolved against `owner` — the
    /// type that owns the value the cursor is in, as returned by
    /// [`Backend::resolve_owner_type_at`].
    ///
    /// `None` when the word names nothing the analyzer knows about.
    fn resolve_symbol<'a>(
        &'a self,
        owner: Option<&'a rust_analyzer::TypeInfo>,
        tree: Option<&tree_sitter::Tree>,
        content: &str,
        position: Position,
        word: &str,
    ) -> Option<Symbol<'a>> {
        if let (Some(owner), Some(tree)) = (owner, tree) {
            let cursor_field = tree_sitter_parser::get_field_at_position(tree, content, position);

            // The field the cursor is on, when the word is its name: a field of
            // the enum variant being spelled out, or one of the type's own.
            if cursor_field.as_deref() == Some(word) {
                let in_variant =
                    tree_sitter_parser::find_current_variant_context(tree, content, position)
                        .and_then(|variant_name| owner.find_variant(&variant_name))
                        .and_then(|variant| variant.fields.iter().find(|f| f.name == word));
                if let Some(field) = in_variant.or_else(|| owner.find_field(word)) {
                    return Some(Symbol::Field { owner, field });
                }
            }

            // A variant of the enum the cursor is in — the `Detailed` of
            // `Detailed(length: 1)`.
            if let Some(variant) = owner.find_variant(word) {
                return Some(Symbol::Variant { owner, variant });
            }

            // The name of the type itself.
            if owner.name == word || owner.name.ends_with(&format!("::{}", word)) {
                return Some(Symbol::Type(owner));
            }

            // A variant of the type the field under the cursor is declared as —
            // the `Short` of `post_type: Short`, which `owner` is not the enum
            // for because the value names no variant to navigate into.
            if let Some(field_name) = cursor_field
                && let Some(field) = owner.find_field(&field_name)
                && let Some(field_type) = self.rust_analyzer.get_type_info(&field.type_name)
                && let Some(variant) = field_type.find_variant(word)
            {
                return Some(Symbol::Variant {
                    owner: field_type,
                    variant,
                });
            }
        }

        // Nothing in the surrounding type accounts for the word, so it may name
        // a type outright.
        self.rust_analyzer.get_type_info(word).map(Symbol::Type)
    }

    /// Resolve the TypeInfo that owns the fields at `position`: the document's
    /// root type navigated through the nested type contexts around the
    /// position. Used by goto_definition, hover, completion and rename.
    fn resolve_owner_type_at(
        &self,
        content: &str,
        tree: Option<&tree_sitter::Tree>,
        position: Position,
    ) -> Option<&rust_analyzer::TypeInfo> {
        let root = self.resolve_root_type_info(content, tree)?;
        let contexts = tree
            .map(|t| tree_sitter_parser::find_type_context_at_position(t, content, position))
            .unwrap_or_default();
        navigation::navigate_type_contexts(&self.rust_analyzer, Some(root), &contexts)
    }

    /// Every field in the document whose name is spelled `field_name`, as
    /// `(name node, the type that owns it)` pairs in document order. A `None`
    /// type means the value the field belongs to could not be resolved.
    ///
    /// This is [`Backend::resolve_owner_type_at`] for a whole document at once,
    /// and is what a caller wanting more than one position should use.
    /// Resolving positions one at a time re-walks the nesting path up to the
    /// root and re-resolves every step of it for each of them, so a pass over
    /// the document costs time proportional to its size *squared*. The type of
    /// a struct value follows from the type of the value it sits in, so walking
    /// the document downwards instead resolves each of them exactly once.
    fn fields_named<'a, 't>(
        &'a self,
        tree: &'t tree_sitter::Tree,
        content: &str,
        field_name: &str,
    ) -> Vec<(tree_sitter::Node<'t>, Option<&'a rust_analyzer::TypeInfo>)> {
        let root = self.resolve_root_type_info(content, Some(tree));
        // The type owning each struct value's fields, by node id, filled in as
        // the walk descends. Nodes carry the id of the struct value enclosing
        // them rather than the type itself, which keeps what the walk passes
        // around to one `usize`.
        let mut owners: HashMap<usize, Option<&rust_analyzer::TypeInfo>> = HashMap::new();
        let mut found = Vec::new();
        // Children are pushed in reverse so that they pop in document order.
        // Each carries the field its value belongs to, for the same reason it
        // carries the enclosing struct: recovering either from the node means
        // walking back up the tree, once per value.
        let mut stack = vec![(tree.root_node(), None, None)];

        while let Some((node, enclosing, owning_field)) = stack.pop() {
            let enclosing = if node.kind() == "struct" {
                let owner = match enclosing.map(|id| owners[&id]) {
                    // One step down from the value this one sits in.
                    Some(Some(outer)) => navigation::step_type_context(
                        &self.rust_analyzer,
                        outer,
                        &tree_sitter_parser::struct_type_context(&node, content, owning_field),
                    ),
                    // Nothing encloses it, so it is the document's root value.
                    None => root,
                    // The value it sits in didn't resolve, so neither can it.
                    Some(None) => None,
                };
                owners.insert(node.id(), owner);
                Some(node.id())
            } else {
                enclosing
            };

            if node.kind() == "field"
                && let Some(name) = node.child(0)
                && ts_utils::node_text(&name, content) == Some(field_name)
            {
                found.push((name, enclosing.and_then(|id| owners[&id])));
            }

            let child_field = ts_utils::owning_field_for_children(&node, content, owning_field);
            for i in (0..node.child_count()).rev() {
                if let Some(child) = node.child(i) {
                    stack.push((child, enclosing, child_field));
                }
            }
        }

        found
    }

    fn format_type_hover(&self, type_info: &rust_analyzer::TypeInfo) -> String {
        use crate::rust_analyzer::TypeKind;

        let type_name = type_utils::short_name(&type_info.name);
        let mut hover_text = String::new();

        match &type_info.kind {
            TypeKind::Struct(fields) => {
                hover_text.push_str(&format!("```rust\nstruct {}\n```", type_name));

                if let Some(ref docs) = type_info.docs {
                    hover_text.push_str(&format!("\n\n{}", docs));
                }

                if !fields.is_empty() {
                    hover_text.push_str("\n\n**Fields:**\n");
                    for field in fields {
                        hover_text.push_str(&format!("- `{}`: `{}`", field.name, field.type_name));
                        if let Some(ref field_docs) = field.docs {
                            hover_text.push_str(&format!(" - {}", field_docs));
                        }
                        hover_text.push('\n');
                    }
                }
            }
            TypeKind::Enum(variants) => {
                hover_text.push_str(&format!("```rust\nenum {}\n```", type_name));

                if let Some(ref docs) = type_info.docs {
                    hover_text.push_str(&format!("\n\n{}", docs));
                }

                if !variants.is_empty() {
                    hover_text.push_str("\n\n**Variants:**\n");
                    for variant in variants {
                        if variant.fields.is_empty() {
                            hover_text.push_str(&format!("- `{}`", variant.name));
                        } else {
                            hover_text.push_str(&format!("- `{}` with fields:", variant.name));
                            for field in &variant.fields {
                                hover_text.push_str(&format!(
                                    "\n  - `{}`: `{}`",
                                    field.name, field.type_name
                                ));
                            }
                        }
                        if let Some(ref variant_docs) = variant.docs {
                            hover_text.push_str(&format!(" - {}", variant_docs));
                        }
                        hover_text.push('\n');
                    }
                }
            }
        }

        hover_text
    }

    async fn publish_diagnostics(&self, uri: &str, doc: &Document) {
        let content = &doc.content;
        let url: Url = match uri.parse() {
            Ok(url) => url,
            Err(_) => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("Cannot publish diagnostics: invalid URI {}", uri),
                    )
                    .await;
                return;
            }
        };

        let diagnostics = match self.resolve_root_type_info(content, doc.tree.as_ref()) {
            Some(type_info) => diagnostics::validate_ron_with_analyzer(
                content,
                doc.tree.as_ref(),
                type_info,
                self.rust_analyzer.clone(),
            ),
            None => vec![],
        };

        let diagnostics: Vec<_> = if self.info_diagnostics {
            diagnostics
        } else {
            diagnostics
                .into_iter()
                .filter(|d| d.severity != Some(DiagnosticSeverity::INFORMATION))
                .collect()
        };

        self.client
            .log_message(
                MessageType::LOG,
                format!("Publishing {} diagnostics for {}", diagnostics.len(), uri),
            )
            .await;
        self.client
            .publish_diagnostics(url, diagnostics, None)
            .await;
    }
}

/// Run the LSP server on stdin/stdout for the given analyzer.
///
/// Returns once the client has sent the `exit` notification, or once the input
/// stream ends, so the caller can let its process terminate.
///
/// `info_diagnostics` turns on the informational diagnostics that annotate a
/// field with the type it is declared as (`port: u16`). They are only emitted
/// for the root value's own fields, and only where the value doesn't already
/// name its type; pass `false` to publish warnings and errors alone.
pub async fn serve(analyzer: RustAnalyzer, info_diagnostics: bool) {
    serve_on(
        stdin_stream(),
        tokio::io::stdout(),
        analyzer,
        info_diagnostics,
    )
    .await
}

/// `stdin` as a stream the caller's runtime can walk away from.
///
/// [`tokio::io::stdin`] reads on the runtime's blocking pool, and a blocking
/// read can't be cancelled: dropping the server loop leaves a `read(2)` on the
/// pipe outstanding, and shutting the runtime down then waits for it to come
/// back. Nothing but the editor closing the pipe ends that wait, so a process
/// that returns from [`serve`] still hangs at the end of `main` — which is where
/// a `#[tokio::main]` application drops its runtime. Reading on a plain OS
/// thread leaves the shutdown nothing to join; the thread goes when the process
/// does.
fn stdin_stream() -> ThreadedStdin {
    let (chunks_tx, chunks_rx) = mpsc::channel(4);

    std::thread::spawn(move || {
        use std::io::Read;

        let mut stdin = std::io::stdin().lock();
        loop {
            let mut chunk = vec![0; 8192];
            match stdin.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    chunk.truncate(read);
                    // The server is gone; so is any reason to keep reading.
                    if chunks_tx.blocking_send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = chunks_tx.blocking_send(Err(error));
                    break;
                }
            }
        }
    });

    ThreadedStdin {
        chunks: chunks_rx,
        chunk: Vec::new(),
        handed_out: 0,
    }
}

/// The reading half of [`stdin_stream`]: the chunks that thread sends, handed to
/// the server loop as an [`AsyncRead`].
struct ThreadedStdin {
    chunks: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    /// The chunk currently being handed out, and how much of it already was.
    chunk: Vec<u8>,
    handed_out: usize,
}

impl AsyncRead for ThreadedStdin {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        if this.handed_out == this.chunk.len() {
            match this.chunks.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.chunk = chunk;
                    this.handed_out = 0;
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                // The reading thread is done, which is end of input.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }

        let take = buf.remaining().min(this.chunk.len() - this.handed_out);
        buf.put_slice(&this.chunk[this.handed_out..this.handed_out + take]);
        this.handed_out += take;

        Poll::Ready(Ok(()))
    }
}

/// [`serve`] over an arbitrary pair of streams, so the shutdown handshake can be
/// driven from a test without taking over the process's stdio.
async fn serve_on<I, O>(input: I, output: O, analyzer: RustAnalyzer, info_diagnostics: bool)
where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
{
    let (service, socket) =
        LspService::new(|client| Backend::new(client, analyzer, info_diagnostics));

    let (exited_tx, exited_rx) = oneshot::channel();
    let service = ExitNotifier {
        inner: service,
        exited: Some(exited_tx),
    };

    // `tower_lsp`'s loop only learns that the service has exited when it tries
    // to dispatch the *next* message, and after `exit` there is no next message:
    // it parks on the input stream forever. A client that sends `exit` and then
    // waits for the process to go away — which is what the protocol says `exit`
    // means — waits until it gives up and kills us. Stop as soon as the
    // notification has been handled instead.
    tokio::select! {
        _ = Server::new(input, output, socket).serve(service) => {}
        _ = exited_rx => {}
    }
}

/// Wraps the LSP service to signal `exited` once the client's `exit`
/// notification has been handled.
///
/// `exit` is answered by `tower_lsp`'s own router rather than by [`Backend`],
/// so this is the only place the request is visible to us.
struct ExitNotifier<S> {
    inner: S,
    exited: Option<oneshot::Sender<()>>,
}

impl<S> Service<RpcRequest> for ExitNotifier<S>
where
    S: Service<RpcRequest, Response = Option<RpcResponse>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RpcRequest) -> Self::Future {
        // Claim the sender before handing the request on: `req` is moved into
        // the inner service, and only one `exit` can be the one that stops us.
        let exited = if req.method() == "exit" {
            self.exited.take()
        } else {
            None
        };

        let response = self.inner.call(req);
        Box::pin(async move {
            let response = response.await;
            if let Some(exited) = exited {
                let _ = exited.send(());
            }
            response
        })
    }
}

/// Build the Markdown hover shown when the cursor is on an enum variant name,
/// listing the variant's docs and any tuple/struct fields. `enum_type_name` is
/// the fully-qualified name of the owning enum; only its last segment is shown.
fn format_variant_hover(enum_type_name: &str, variant: &rust_analyzer::EnumVariant) -> String {
    let mut hover_text = format!(
        "```rust\nenum {}\n```\n\n**Variant:** `{}`",
        type_utils::short_name(enum_type_name),
        variant.name
    );

    if let Some(ref docs) = variant.docs {
        hover_text.push_str(&format!("\n\n{}", docs));
    }

    if !variant.fields.is_empty() {
        hover_text.push_str("\n\n**Fields:**\n");
        for field in &variant.fields {
            hover_text.push_str(&format!("- `{}`: `{}`", field.name, field.type_name));
            if let Some(ref field_docs) = field.docs {
                hover_text.push_str(&format!(" - {}", field_docs));
            }
            hover_text.push('\n');
        }
    }

    hover_text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_analyzer::{EnumVariant, FieldInfo, TypeInfo, TypeKind};

    /// Helper to create a test Backend with a pre-configured analyzer
    async fn create_test_backend_with_analyzer(analyzer: RustAnalyzer) -> Backend {
        let (service, _) = LspService::new(|client| Backend::new(client, analyzer, true));
        service.inner().clone()
    }

    impl Clone for Backend {
        fn clone(&self) -> Self {
            Backend {
                client: self.client.clone(),
                documents: self.documents.clone(),
                rust_analyzer: self.rust_analyzer.clone(),
                info_diagnostics: self.info_diagnostics,
            }
        }
    }

    /// The `exit` notification means "stop the process", so a server that goes
    /// on reading its input after one arrives leaves the editor waiting on a
    /// process that never goes away. Drive the real handshake over a pipe and
    /// check that `serve_on` returns by itself — with the client's write half
    /// still open, so that nothing but `exit` can be what ended it.
    #[tokio::test]
    async fn serve_returns_after_exit_notification() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client, server) = tokio::io::duplex(64 * 1024);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);

        let serving = tokio::spawn(serve_on(
            server_read,
            server_write,
            RustAnalyzer::new(),
            false,
        ));

        // Drain the replies, so the server never parks on a full pipe instead.
        let draining = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while client_read.read(&mut buf).await.unwrap_or(0) > 0 {}
        });

        for message in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#,
            r#"{"jsonrpc":"2.0","method":"exit"}"#,
        ] {
            client_write
                .write_all(format!("Content-Length: {}\r\n\r\n{message}", message.len()).as_bytes())
                .await
                .unwrap();
            client_write.flush().await.unwrap();
        }

        tokio::time::timeout(std::time::Duration::from_secs(10), serving)
            .await
            .expect("serve kept running after the exit notification")
            .unwrap();

        drop(client_write);
        draining.abort();
    }

    /// A JSON-RPC frame doesn't arrive in one `read(2)`, so the reader behind
    /// [`stdin_stream`] has to hand a chunk out across as many `poll_read`s as
    /// the caller's buffer needs, then go back for the next one. Read a pair of
    /// chunks two bytes at a time and check nothing is lost or repeated at the
    /// seam, that the error the thread reports surfaces, and that the thread
    /// hanging up reads as end of input.
    #[tokio::test]
    async fn threaded_stdin_hands_out_chunks_in_order() {
        use tokio::io::AsyncReadExt;

        let (chunks_tx, chunks_rx) = mpsc::channel(4);
        chunks_tx.send(Ok(b"Content-Len".to_vec())).await.unwrap();
        chunks_tx.send(Ok(b"gth: 2".to_vec())).await.unwrap();

        let mut stdin = ThreadedStdin {
            chunks: chunks_rx,
            chunk: Vec::new(),
            handed_out: 0,
        };

        let mut read = Vec::new();
        let mut buf = [0u8; 2];
        for _ in 0.."Content-Length: 2".len().div_ceil(2) {
            let n = stdin.read(&mut buf).await.unwrap();
            read.extend_from_slice(&buf[..n]);
        }
        assert_eq!(read, b"Content-Length: 2");

        chunks_tx
            .send(Err(std::io::Error::other("pipe broke")))
            .await
            .unwrap();
        let error = stdin.read(&mut buf).await.unwrap_err();
        assert_eq!(error.to_string(), "pipe broke");

        drop(chunks_tx);
        assert_eq!(stdin.read(&mut buf).await.unwrap(), 0);
    }

    /// Test that hover returns documentation for struct fields
    #[tokio::test]
    async fn test_lsp_hover_on_struct_field() {
        // Set up analyzer with a type with documented fields
        let mut analyzer = RustAnalyzer::with_root_type("crate::User");
        analyzer.add_type(TypeInfo {
            name: "crate::User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u64".to_string(),
                    docs: Some("The unique user identifier".to_string()),
                    line: Some(10),
                    column: Some(4),
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: Some("The user's display name".to_string()),
                    line: Some(12),
                    column: Some(4),
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "email".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: Some(14),
                    column: Some(4),
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: Some("A user in the system".to_string()),
            source_file: None,
            line: Some(8),
            column: Some(0),
            has_default: false,
            ..Default::default()
        });

        let backend = create_test_backend_with_analyzer(analyzer).await;

        // Simulate opening a document
        let uri: Url = "file:///test/user.ron".parse().unwrap();
        let content = r#"User(
    id: 42,
    name: "Alice",
    email: "alice@example.com",
)"#;

        backend.documents.write().await.insert(
            uri.to_string(),
Document::new(content.to_string()),
        );

        // Test hover on "id" field (line 1, character 4)
        let hover_params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(1, 6), // On "id"
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let hover_result = backend.hover(hover_params).await.unwrap();
        assert!(
            hover_result.is_some(),
            "Hover should return a result for 'id' field"
        );

        let hover = hover_result.unwrap();
        if let HoverContents::Markup(markup) = hover.contents {
            assert!(
                markup.value.contains("id"),
                "Hover should contain field name. Got: {}",
                markup.value
            );
            assert!(
                markup.value.contains("u64"),
                "Hover should contain type. Got: {}",
                markup.value
            );
            assert!(
                markup.value.contains("unique user identifier"),
                "Hover should contain documentation. Got: {}",
                markup.value
            );
        } else {
            panic!("Expected Markup hover contents");
        }

        // Test hover on "name" field (line 2, character 4)
        let hover_params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(2, 6), // On "name"
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let hover_result = backend.hover(hover_params).await.unwrap();
        assert!(
            hover_result.is_some(),
            "Hover should return a result for 'name' field"
        );

        let hover = hover_result.unwrap();
        if let HoverContents::Markup(markup) = hover.contents {
            assert!(
                markup.value.contains("name"),
                "Hover should contain field name"
            );
            assert!(markup.value.contains("String"), "Hover should contain type");
            assert!(
                markup.value.contains("display name"),
                "Hover should contain documentation"
            );
        } else {
            panic!("Expected Markup hover contents");
        }
    }

    /// Test that hover returns documentation for enum variants
    #[tokio::test]
    async fn test_lsp_hover_on_enum_variant() {
        // Set up an enum type
        let mut analyzer = RustAnalyzer::with_root_type("crate::Status");
        analyzer.add_type(TypeInfo {
            name: "crate::Status".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Active".to_string(),
                    fields: vec![],
                    docs: Some("The user is currently active".to_string()),
                    line: Some(5),
                    column: Some(4),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Inactive".to_string(),
                    fields: vec![FieldInfo {
                        name: "reason".to_string(),
                        type_name: "String".to_string(),
                        docs: Some("Why the user is inactive".to_string()),
                        line: Some(8),
                        column: Some(8),
                        has_default: false,
                        ..Default::default()
                    }],
                    docs: Some("The user is not active".to_string()),
                    line: Some(7),
                    column: Some(4),
                    ..Default::default()
                },
            ]),
            docs: Some("User account status".to_string()),
            source_file: None,
            line: Some(3),
            column: Some(0),
            has_default: false,
            ..Default::default()
        });

        let backend = create_test_backend_with_analyzer(analyzer).await;

        // Simulate opening a document with an enum variant
        let uri: Url = "file:///test/status.ron".parse().unwrap();
        let content = r#"Inactive(
    reason: "On vacation",
)"#;

        backend.documents.write().await.insert(
            uri.to_string(),
Document::new(content.to_string()),
        );

        // Test hover on "Inactive" variant (line 0, character 2)
        let hover_params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(0, 2), // On "Inactive"
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let hover_result = backend.hover(hover_params).await.unwrap();
        assert!(
            hover_result.is_some(),
            "Hover should return a result for 'Inactive' variant"
        );

        let hover = hover_result.unwrap();
        if let HoverContents::Markup(markup) = hover.contents {
            assert!(
                markup.value.contains("Inactive"),
                "Hover should contain variant name. Got: {}",
                markup.value
            );
            assert!(
                markup.value.contains("not active"),
                "Hover should contain variant documentation. Got: {}",
                markup.value
            );
        } else {
            panic!("Expected Markup hover contents");
        }
    }

    /// A document names a variant the way serde serializes it, so hover has to
    /// resolve that name — not only the Rust one. Both `rename_all` and an
    /// explicit `rename` have to be honored.
    #[tokio::test]
    async fn test_lsp_hover_on_serde_renamed_enum_variant() {
        let mut analyzer = RustAnalyzer::with_root_type("crate::Status");
        analyzer.add_type(TypeInfo {
            name: "crate::Status".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "OnVacation".to_string(),
                    docs: Some("The user is away".to_string()),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Retired".to_string(),
                    rename: Some("legacy".to_string()),
                    docs: Some("The user is never coming back".to_string()),
                    ..Default::default()
                },
            ]),
            rename_all: Some("snake_case".to_string()),
            ..Default::default()
        });

        let backend = create_test_backend_with_analyzer(analyzer).await;

        // `on_vacation` is OnVacation under rename_all = "snake_case";
        // `legacy` comes from the explicit rename on Retired.
        for (content, column, expected_docs) in [
            ("on_vacation", 3, "The user is away"),
            ("legacy", 1, "The user is never coming back"),
        ] {
            let uri: Url = "file:///test/status.ron".parse().unwrap();
            backend
                .documents
                .write()
                .await
                .insert(uri.to_string(), Document::new(content.to_string()));

            let hover_result = backend
                .hover(HoverParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri },
                        position: Position::new(0, column),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .await
                .unwrap();

            let Some(Hover {
                contents: HoverContents::Markup(markup),
                ..
            }) = hover_result
            else {
                panic!("Hover should resolve the serialized variant name '{content}'");
            };
            assert!(
                markup.value.contains(expected_docs),
                "Hover on '{content}' should document the variant. Got: {}",
                markup.value
            );
        }
    }

    /// Test that hover works on the type name itself
    #[tokio::test]
    async fn test_lsp_hover_on_type_name() {
        // Set up a simple type
        let config_type = TypeInfo {
            name: "crate::Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "debug".to_string(),
                type_name: "bool".to_string(),
                docs: Some("Enable debug mode".to_string()),
                line: Some(5),
                column: Some(4),
                has_default: false,
                ..Default::default()
            }]),
            docs: Some("Application configuration settings".to_string()),
            source_file: None,
            line: Some(3),
            column: Some(0),
            has_default: false,
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::with_root_type("crate::Config");
        analyzer.add_type(config_type);
        let backend = create_test_backend_with_analyzer(analyzer).await;

        let uri: Url = "file:///test/config.ron".parse().unwrap();
        let content = r#"Config(
    debug: true,
)"#;

        backend.documents.write().await.insert(
            uri.to_string(),
Document::new(content.to_string()),
        );

        // Test hover on "Config" type name (line 0, character 2)
        let hover_params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(0, 2), // On "Config"
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let hover_result = backend.hover(hover_params).await.unwrap();
        assert!(
            hover_result.is_some(),
            "Hover should return a result for type name"
        );

        let hover = hover_result.unwrap();
        if let HoverContents::Markup(markup) = hover.contents {
            assert!(
                markup.value.contains("Config"),
                "Hover should contain type name. Got: {}",
                markup.value
            );
            assert!(
                markup.value.contains("struct"),
                "Hover should indicate it's a struct. Got: {}",
                markup.value
            );
            assert!(
                markup.value.contains("configuration settings"),
                "Hover should contain type documentation. Got: {}",
                markup.value
            );
        } else {
            panic!("Expected Markup hover contents");
        }
    }

    /// Test that hover returns None for positions with no meaningful content
    #[tokio::test]
    async fn test_lsp_hover_on_whitespace() {
        let simple_type = TypeInfo {
            name: "crate::Simple".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::with_root_type("crate::Simple");
        analyzer.add_type(simple_type);
        let backend = create_test_backend_with_analyzer(analyzer).await;

        let uri: Url = "file:///test/simple.ron".parse().unwrap();
        let content = r#"Simple(

)"#;

        backend.documents.write().await.insert(
            uri.to_string(),
Document::new(content.to_string()),
        );

        // Test hover on empty line (line 1, which is just whitespace)
        let hover_params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position::new(1, 0), // Empty line
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let hover_result = backend.hover(hover_params).await.unwrap();
        assert!(
            hover_result.is_none(),
            "Hover on whitespace should return None"
        );
    }

    /// Test completion returns field suggestions
    #[tokio::test]
    async fn test_lsp_completion_suggests_fields() {
        let user_type = TypeInfo {
            name: "crate::User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u64".to_string(),
                    docs: Some("User ID".to_string()),
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        let mut analyzer = RustAnalyzer::with_root_type("crate::User");
        analyzer.add_type(user_type);
        let backend = create_test_backend_with_analyzer(analyzer).await;

        let uri: Url = "file:///test/user.ron".parse().unwrap();
        // Content with cursor position where we want completions
        let content = r#"User(

)"#;

        backend.documents.write().await.insert(
            uri.to_string(),
Document::new(content.to_string()),
        );

        // Request completion inside the struct
        let completion_params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position::new(1, 4), // Inside struct, empty line
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        };

        let completion_result = backend.completion(completion_params).await.unwrap();
        assert!(
            completion_result.is_some(),
            "Completion should return suggestions"
        );

        if let Some(CompletionResponse::Array(items)) = completion_result {
            let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(
                labels.contains(&"id"),
                "Completion should suggest 'id' field. Got: {:?}",
                labels
            );
            assert!(
                labels.contains(&"name"),
                "Completion should suggest 'name' field. Got: {:?}",
                labels
            );
        } else {
            panic!("Expected Array completion response");
        }
    }

    /// A root type that was configured but never registered is not a usable
    /// root, so the document's own struct name is used instead. Diagnostics and
    /// code actions have always done this; hover, go-to-definition and
    /// completion now resolve the root the same way.
    #[tokio::test]
    async fn test_hover_falls_back_when_root_type_unregistered() {
        let mut analyzer = RustAnalyzer::with_root_type("crate::NotRegistered");
        analyzer.add_type(TypeInfo {
            name: "crate::User".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "id".to_string(),
                type_name: "u64".to_string(),
                docs: Some("The unique user identifier".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let backend = create_test_backend_with_analyzer(analyzer).await;
        let uri: Url = "file:///test/user.ron".parse().unwrap();
        let content = "User(\n    id: 42,\n)";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content.to_string()));

        let hover = backend
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position::new(1, 6),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .unwrap()
            .expect("hover should fall back to the document's struct name");

        let HoverContents::Markup(markup) = hover.contents else {
            panic!("Expected Markup hover contents");
        };
        assert!(
            markup.value.contains("u64") && markup.value.contains("unique user identifier"),
            "Hover should describe User::id. Got: {}",
            markup.value
        );
    }

    /// RON lets a struct value omit its name, both at the root (`(...)`) and
    /// for a nested value (`server: (...)`). Hover, go-to-definition and
    /// completion inside a nested value must resolve the nested type in all
    /// four spellings; they used to resolve one level too shallow — against
    /// the outer type — whenever a name was left out.
    #[tokio::test]
    async fn test_nested_resolution_with_omitted_struct_names() {
        let mut analyzer = RustAnalyzer::with_root_type("crate::Config");
        analyzer.add_type(TypeInfo {
            name: "crate::Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "server".to_string(),
                type_name: "crate::Server".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "crate::Server".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "port".to_string(),
                    type_name: "u16".to_string(),
                    docs: Some("Port to bind".to_string()),
                    line: Some(7),
                    column: Some(4),
                    ..Default::default()
                },
                FieldInfo {
                    name: "host".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
            ]),
            source_file: Some(std::path::PathBuf::from("/test/src/config.rs")),
            ..Default::default()
        });
        let backend = create_test_backend_with_analyzer(analyzer).await;

        for (root, nested) in [
            ("Config", "Server"),
            ("Config", ""),
            ("", "Server"),
            ("", ""),
        ] {
            let content = format!("{root}(\n    server: {nested}(\n        port: 80,\n    ),\n)");
            let label = format!("root {:?}, nested {:?}", root, nested);
            let uri: Url = "file:///test/config.ron".parse().unwrap();
            backend
                .documents
                .write()
                .await
                .insert(uri.to_string(), Document::new(content.clone()));

            // Hover on the nested `port` must describe Server::port.
            let hover = backend
                .hover(HoverParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position: Position::new(2, 9),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .await
                .unwrap();
            let Some(Hover {
                contents: HoverContents::Markup(markup),
                ..
            }) = hover
            else {
                panic!("hover on nested 'port' returned nothing ({label})");
            };
            assert!(
                markup.value.contains("Port to bind"),
                "hover should document Server::port ({label}). Got: {}",
                markup.value
            );

            // Go-to-definition must point at Server::port's source location.
            let definition = backend
                .goto_definition(GotoDefinitionParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position: Position::new(2, 9),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .unwrap();
            let Some(GotoDefinitionResponse::Scalar(location)) = definition else {
                panic!("goto_definition on nested 'port' returned nothing ({label})");
            };
            assert_eq!(
                location.range.start,
                Position::new(6, 4),
                "definition should land on Server::port ({label})"
            );

            // Completion on a fresh line inside the nested value must offer
            // Server's fields, not Config's — and of those, only the ones the
            // nested value hasn't written yet.
            let with_blank_line =
                format!("{root}(\n    server: {nested}(\n        port: 80,\n        \n    ),\n)");
            backend
                .documents
                .write()
                .await
                .insert(uri.to_string(), Document::new(with_blank_line));
            let completion = backend
                .completion(CompletionParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position: Position::new(3, 8),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: None,
                })
                .await
                .unwrap();
            let Some(CompletionResponse::Array(items)) = completion else {
                panic!("completion inside the nested value returned nothing ({label})");
            };
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(
                labels.contains(&"host"),
                "completion should offer Server's own fields ({label}). Got: {labels:?}"
            );
            assert!(
                !labels.contains(&"port"),
                "completion should not offer a field the value has written ({label}). \
                 Got: {labels:?}"
            );
            assert!(
                !labels.contains(&"server"),
                "completion should not offer Config's fields ({label}). Got: {labels:?}"
            );
        }
    }

    /// Whole-document formatting lays out the tree the document already
    /// carries instead of parsing its text again, so what the editor is handed
    /// has to be exactly what parsing from scratch would have produced — over
    /// the whole document, including the comments and the `#![enable(...)]`
    /// header that only the tree knows the position of.
    #[tokio::test]
    async fn formatting_a_document_matches_formatting_its_text() {
        let backend = create_test_backend_with_analyzer(RustAnalyzer::new()).await;

        let uri: Url = "file:///test/format.ron".parse().unwrap();
        let content = "#![enable(implicit_some)]\n// leading\nConfig(port:8080,hosts:[\"a\",\"b\"],\nnested:Inner(x:1),)\n";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content.to_string()));

        let edits = backend
            .formatting(DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                options: FormattingOptions::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .unwrap()
            .expect("an unformatted document should produce an edit");

        assert_eq!(edits.len(), 1, "the whole document is replaced at once");
        assert_eq!(
            edits[0].new_text,
            Backend::format_ron(content),
            "the cached tree and a fresh parse must lay out identically"
        );
        assert_eq!(
            edits[0].range.start,
            Position::new(0, 0),
            "the edit replaces the document from its first character"
        );

        // An already-formatted document is left alone: the handler compares the
        // layout against the content it came from, not against a reparse.
        let formatted = edits[0].new_text.clone();
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(formatted));
        assert!(
            backend
                .formatting(DocumentFormattingParams {
                    text_document: TextDocumentIdentifier { uri },
                    options: FormattingOptions::default(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .await
                .unwrap()
                .is_none(),
            "formatting an already-formatted document should produce no edit"
        );
    }

    #[test]
    fn test_format_ron_named_struct() {
        let input = r#"User(
    id: 1,
    name: "Alice",
    age: 28
)"#;
        let formatted = Backend::format_ron(input);

        // Should be able to parse the result
        let parsed = ron::from_str::<ron::Value>(&formatted);
        assert!(
            parsed.is_ok(),
            "Formatted RON should be parseable. Got: {}",
            formatted
        );

        // Original should also be parseable
        let original_parsed: ron::Value = ron::from_str(input).expect("Original should parse");
        let formatted_parsed: ron::Value =
            ron::from_str(&formatted).expect("Formatted should parse");

        // Both should represent the same value
        assert_eq!(
            original_parsed, formatted_parsed,
            "Formatted RON should preserve value"
        );
    }

    #[test]
    fn test_format_ron_with_comment() {
        // Type annotations are no longer special - they're just regular comments
        // that get stripped during formatting
        let input = r#"/* @[crate::models::User] */

User(
    id: 1,
    name: "Alice"
)"#;
        let formatted = Backend::format_ron(input);

        // Should be parseable
        let parsed = ron::from_str::<ron::Value>(&formatted);
        assert!(
            parsed.is_ok(),
            "Formatted RON should be parseable. Got: {}\nError: {:?}",
            formatted,
            parsed.as_ref().err()
        );
    }

    #[test]
    fn test_format_ron_unnamed_struct() {
        let input = r#"(
    id: 1,
    name: "Alice",
    roles: ["admin", "user"]
)"#;
        let formatted = Backend::format_ron(input);

        // Should be able to parse both
        let original_parsed: ron::Value = ron::from_str(input).expect("Original should parse");
        let formatted_parsed = ron::from_str::<ron::Value>(&formatted);

        assert!(
            formatted_parsed.is_ok(),
            "Formatted RON should be parseable.\nInput:\n{}\n\nFormatted:\n{}\n\nError: {:?}",
            input,
            formatted,
            formatted_parsed.as_ref().err()
        );

        let formatted_parsed = formatted_parsed.unwrap();
        assert_eq!(
            original_parsed, formatted_parsed,
            "Formatted RON should preserve value.\nOriginal: {:?}\nFormatted: {:?}",
            original_parsed, formatted_parsed
        );
    }

    #[test]
    fn test_format_ron_real_example() {
        // This is actual RON syntax from user.ron (without the old type annotation comment)
        let input = r#"User(
    id: 1,
    name: "Alice Johnson",
    email: "alice@example.com",
    age: 28,
    bio: Some("Full-stack developer passionate about Rust and web technologies."),
    is_active: true,
    roles: ["admin", "developer"],
)"#;

        let formatted = Backend::format_ron(input);

        // Both should parse
        let original_parsed: ron::Value = ron::from_str(input).expect("Original should parse");
        let formatted_parsed = ron::from_str::<ron::Value>(&formatted);

        assert!(
            formatted_parsed.is_ok(),
            "Formatted RON should be parseable.\n\nOriginal RON:\n{}\n\nFormatted RON:\n{}\n\nError: {:?}",
            input,
            formatted,
            formatted_parsed.as_ref().err()
        );

        let formatted_parsed = formatted_parsed.unwrap();
        assert_eq!(
            original_parsed, formatted_parsed,
            "Formatted RON should preserve value.\nOriginal: {:?}\nFormatted: {:?}",
            original_parsed, formatted_parsed
        );
    }

    #[test]
    fn test_format_ron_outputs_valid_ron() {
        // Test complex nested RON
        let input = r#"Post(
    id: 123,
    title: "Mixed Syntax Example",
    content: "Demonstrating both explicit and unnamed struct syntax",
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

        let formatted = Backend::format_ron(input);

        println!("===== FORMATTED OUTPUT =====");
        println!("{}", formatted);
        println!("===== END =====");

        // The formatted output MUST be valid RON
        let parse_result = ron::from_str::<ron::Value>(&formatted);
        assert!(
            parse_result.is_ok(),
            "Formatted output must be valid RON.\n\nFormatted:\n{}\n\nError: {:?}",
            formatted,
            parse_result.err()
        );

        // And it should NOT look like JSON (no leading braces for maps)
        assert!(
            !formatted.trim().starts_with('{'),
            "Formatted RON should not start with '{{' (that's JSON syntax). Got:\n{}",
            formatted
        );
    }

    #[test]
    fn test_find_variant_context_without_type_prefix() {
        // Test finding variant when type prefix isn't present (common case)
        let content = r#"Detailed(
            length: 1,
        )"#;
        let position = Position::new(1, 12); // On "length" line

        let tree = ts_utils::RonParser::new().parse(content).unwrap();
        let variant = tree_sitter_parser::find_current_variant_context(&tree, content, position);
        assert_eq!(
            variant,
            Some("Detailed".to_string()),
            "Should detect Detailed variant"
        );
    }

    #[test]
    fn test_find_current_variant_context_with_full_syntax() {
        // Test the explicit TypeName::VariantName syntax
        let content = r#"PostType::Detailed(
            length: 1,
        )"#;
        let position = Position::new(1, 20); // On "length" line, after colon

        let tree = ts_utils::RonParser::new().parse(content).unwrap();
        let variant_name =
            tree_sitter_parser::find_current_variant_context(&tree, content, position);
        assert!(
            variant_name.is_some(),
            "Should detect enum variant context with :: syntax"
        );
        assert_eq!(
            variant_name.unwrap(),
            "Detailed",
            "Should detect Detailed variant"
        );
    }

    #[test]
    fn test_get_word_at_position_on_field() {
        let content = "    length: 1,";
        let position = Position::new(0, 8); // On "length"

        let word = get_word_at_position(content, position);
        assert_eq!(word, Some("length".to_string()));
    }

    #[test]
    fn test_get_word_at_position_with_multibyte_line() {
        // A multibyte character before the cursor must not cause a mid-codepoint
        // slice. Columns are char offsets: c(0) a(1) f(2) é(3) :(4) ' '(5) x(6).
        // `é` is two UTF-8 bytes, so a cursor just past it (col 4) used to index
        // the byte string at a non-char boundary and panic.
        let content = "café: x";
        let word = get_word_at_position(content, Position::new(0, 4));
        assert_eq!(word, Some("café".to_string()));

        // Cursor just after the trailing identifier resolves to it, unaffected
        // by the multibyte char earlier on the line.
        let word = get_word_at_position(content, Position::new(0, 7));
        assert_eq!(word, Some("x".to_string()));
    }

    #[test]
    fn test_enum_variant_field_detection_full_scenario() {
        // The ACTUAL content from enum_variants.ron
        let content = r#"/* @[crate::models::Message] */
// Complex nested enum variant with Post containing User
PostReference(
    Post(
        id: 42,
        title: "Enum Variants in RON",
        content: "Shows tuple variants, struct variants, and nesting",
        author: User(
            id: 1,
            name: "Alice",
            email: "alice@example.com",
            age: 30,
            bio: Some(
                "Rust developer",
            ),
            is_active: true,
            roles: [
                "admin",
            ],
            invalid_field: "should error",
        ),
        likes: 100,
        tags: [
            "rust",
            "ron",
        ],
        published: true,
        post_type: Detailed(
            length: 1,
        ),
    )
)"#;

        // Position on "length" - line 28 (0-indexed), character 16
        let position = Position::new(28, 16);

        // Test what we detect
        let word = get_word_at_position(content, position);
        assert_eq!(
            word,
            Some("length".to_string()),
            "Should detect 'length' word"
        );

        let tree = ts_utils::RonParser::new().parse(content).unwrap();
        let variant = tree_sitter_parser::find_current_variant_context(&tree, content, position);
        assert_eq!(
            variant,
            Some("Detailed".to_string()),
            "Should detect Detailed variant"
        );
    }

    #[test]
    fn test_incremental_change_mid_document() {
        let mut doc = Document::new("User(\n    id: 1,\n    name: \"Bob\",\n)".to_string());

        // Replace "Bob" with "Alice" (inside the quotes on line 2)
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(2, 11), Position::new(2, 14))),
            range_length: None,
            text: "Alice".to_string(),
        });

        assert_eq!(doc.content, "User(\n    id: 1,\n    name: \"Alice\",\n)");

        // The reused tree must still answer queries correctly
        let tree = doc.tree.as_ref().unwrap();
        let field = tree_sitter_parser::get_field_at_position(
            tree,
            &doc.content,
            Position::new(2, 6),
        );
        assert_eq!(field, Some("name".to_string()));
        let fields = tree_sitter_parser::extract_fields_from_ron(tree, &doc.content, Position::new(0, 0));
        assert!(fields.contains(&"id".to_string()));
        assert!(fields.contains(&"name".to_string()));
    }

    /// Incoming change ranges are columns in UTF-16 code units. Treating them
    /// as characters put the edit one column off for every surrogate pair
    /// earlier on the line, which silently garbles the document the server
    /// holds — and so everything it answers from then on.
    #[test]
    fn test_incremental_change_after_surrogate_pair() {
        let mut doc = Document::new("User(name: \"🚀\", id: 1)".to_string());

        // Replace the `1` with `2`. `🚀` is two UTF-16 code units, so the LSP
        // column of the `1` is two past its character index.
        let column = "User(name: \"🚀\", id: ".encode_utf16().count() as u32;
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range::new(
                Position::new(0, column),
                Position::new(0, column + 1),
            )),
            range_length: None,
            text: "2".to_string(),
        });

        assert_eq!(doc.content, "User(name: \"🚀\", id: 2)");

        // The reused tree must still agree with the content.
        let tree = doc.tree.as_ref().unwrap();
        let fields = tree_sitter_parser::extract_fields_from_ron(tree, &doc.content, Position::new(0, 0));
        assert!(fields.contains(&"name".to_string()), "got {fields:?}");
        assert!(fields.contains(&"id".to_string()), "got {fields:?}");
        assert_eq!(
            tree_sitter_parser::get_field_at_position(tree, &doc.content, Position::new(0, column)),
            Some("id".to_string())
        );
    }

    /// `get_word_at_position` is handed a UTF-16 column too. Enough surrogate
    /// pairs earlier on the line and a column read as characters doesn't just
    /// slip within the word, it lands in the next one.
    #[test]
    fn test_word_at_position_after_surrogate_pair() {
        let content = "Config(name: \"🚀🚀🚀🚀\", host: \"a\", port: 80)";
        let prefix = "Config(name: \"🚀🚀🚀🚀\", ho";
        let column = prefix.encode_utf16().count() as u32;
        assert_eq!(
            get_word_at_position(content, Position::new(0, column)),
            Some("host".to_string())
        );
    }

    #[test]
    fn test_incremental_change_insertion_and_full_replace() {
        let mut doc = Document::new("User(id: 1)".to_string());

        // Insert a new field before the closing paren
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(0, 10), Position::new(0, 10))),
            range_length: None,
            text: ", name: \"X\"".to_string(),
        });
        assert_eq!(doc.content, "User(id: 1, name: \"X\")");
        let tree = doc.tree.as_ref().unwrap();
        let fields = tree_sitter_parser::extract_fields_from_ron(tree, &doc.content, Position::new(0, 0));
        assert!(fields.contains(&"name".to_string()));

        // A change without a range replaces the whole document
        doc.apply_change(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "Post(title: \"hi\")".to_string(),
        });
        assert_eq!(doc.content, "Post(title: \"hi\")");
        let tree = doc.tree.as_ref().unwrap();
        let fields = tree_sitter_parser::extract_fields_from_ron(tree, &doc.content, Position::new(0, 0));
        assert_eq!(fields, vec!["title".to_string()]);
    }

    /// A change range whose end precedes its start is malformed, but the server
    /// has to survive one: splicing the document with a backwards range panics,
    /// and a panic in a notification handler tears down the whole server
    /// process along with every other document the editor has open.
    #[test]
    fn test_incremental_change_with_reversed_range() {
        let mut doc = Document::new("User(\n    id: 1,\n    name: \"Bob\",\n)".to_string());

        // The same edit as `test_incremental_change_mid_document`, with the two
        // ends of the range handed over the wrong way round.
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(2, 14), Position::new(2, 11))),
            range_length: None,
            text: "Alice".to_string(),
        });

        assert_eq!(doc.content, "User(\n    id: 1,\n    name: \"Alice\",\n)");

        // ...and the reused tree has to agree with it, not just the content.
        let tree = doc.tree.as_ref().unwrap();
        assert_eq!(
            tree.root_node().to_sexp(),
            ts_utils::parse(&doc.content).unwrap().root_node().to_sexp()
        );
    }

    /// tree-sitter parses raw strings from masked text, and an edit can change
    /// that masking far from where it lands: here, opening a string at the start
    /// turns a later raw string into ordinary string contents. Reusing the old
    /// tree would keep the raw string's node, which no longer exists.
    #[test]
    fn test_incremental_change_around_raw_strings() {
        let mut doc = Document::new("User(a: 1, b: r\"x\")".to_string());
        let edits = [
            // Open a string before the raw string: everything after is in it.
            (Position::new(0, 8), Position::new(0, 9), "\""),
            // Close it again.
            (Position::new(0, 8), Position::new(0, 9), "1"),
            // Drop the `r`: no longer a raw string.
            (Position::new(0, 14), Position::new(0, 15), ""),
            // Put it back.
            (Position::new(0, 14), Position::new(0, 14), "r"),
        ];
        for (start, end, text) in edits {
            doc.apply_change(TextDocumentContentChangeEvent {
                range: Some(Range::new(start, end)),
                range_length: None,
                text: text.to_string(),
            });
            assert_eq!(
                doc.tree.as_ref().unwrap().root_node().to_sexp(),
                ts_utils::parse(&doc.content).unwrap().root_node().to_sexp(),
                "after editing to {:?}",
                doc.content
            );
        }
        assert_eq!(doc.content, "User(a: 1, b: r\"x\")");
    }

    /// Same for a reversed range spanning lines, which is the case that reaches
    /// the multi-line branch of the splice.
    #[test]
    fn test_incremental_change_with_reversed_multiline_range() {
        let mut doc = Document::new("User(\n    id: 1,\n    name: \"Bob\",\n)".to_string());

        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(2, 4), Position::new(1, 4))),
            range_length: None,
            text: "age: 3,\n    ".to_string(),
        });

        assert_eq!(doc.content, "User(\n    age: 3,\n    name: \"Bob\",\n)");
        let tree = doc.tree.as_ref().unwrap();
        assert_eq!(
            tree.root_node().to_sexp(),
            ts_utils::parse(&doc.content).unwrap().root_node().to_sexp()
        );
    }

    /// `textDocument/rangeFormatting` over a reversed selection used to compute
    /// `end_line - start_line`, which underflows and panics. Formatting a
    /// backwards range has to give the same answer as formatting the ordered
    /// one.
    #[tokio::test]
    async fn test_range_formatting_with_reversed_range() {
        let backend = create_test_backend_with_analyzer(RustAnalyzer::new()).await;
        let url = Url::parse("file:///test.ron").unwrap();
        let content = "User(\n   id:1,\n   name:\"Bob\",\n)";

        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: url.clone(),
                    language_id: "ron".to_string(),
                    version: 1,
                    text: content.to_string(),
                },
            })
            .await;

        let format = async |range| {
            backend
                .range_formatting(DocumentRangeFormattingParams {
                    text_document: TextDocumentIdentifier { uri: url.clone() },
                    range,
                    options: Default::default(),
                    work_done_progress_params: Default::default(),
                })
                .await
                .expect("range formatting must not fail")
        };

        let forwards = format(Range::new(Position::new(0, 0), Position::new(3, 1))).await;
        let backwards = format(Range::new(Position::new(3, 1), Position::new(0, 0))).await;

        assert!(
            forwards.is_some(),
            "the fixture should have something to reformat"
        );
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn test_is_valid_identifier() {
        assert!(is_valid_identifier("name"));
        assert!(is_valid_identifier("_private"));
        assert!(is_valid_identifier("field2"));
        assert!(!is_valid_identifier(""));
        assert!(!is_valid_identifier("2fast"));
        assert!(!is_valid_identifier("has space"));
        assert!(!is_valid_identifier("has-dash"));
    }

    #[test]
    fn test_field_name_node_at() {
        let doc = Document::new("User(\n    id: 1,\n    name: \"Bob\",\n)".to_string());
        let tree = doc.tree.as_ref().unwrap();

        // On the field name
        let node = field_name_node_at(tree, &doc.content, Position::new(2, 5)).unwrap();
        assert_eq!(ts_utils::node_text(&node, &doc.content), Some("name"));

        // On the field value — not renameable
        assert!(field_name_node_at(tree, &doc.content, Position::new(2, 12)).is_none());
    }

    #[tokio::test]
    async fn test_rename_scoped_to_owning_type() {
        // Two types both have a field named "name"; renaming the inner one
        // must not touch the outer one
        let mut analyzer = RustAnalyzer::with_root_type("crate::Post");
        analyzer.add_type(TypeInfo {
            name: "crate::Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "author".to_string(),
                    type_name: "User".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "crate::User".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "name".to_string(),
                type_name: "String".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let backend = create_test_backend_with_analyzer(analyzer).await;

        let content = "Post(\n    name: \"post\",\n    author: User(\n        name: \"bob\",\n    ),\n)";
        let uri = "file:///test.ron";
        backend.documents.write().await.insert(
            uri.to_string(),
            Document::new(content.to_string()),
        );

        // Rename the User.name field (line 3)
        let result = backend
            .rename(RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: Url::parse(uri).unwrap(),
                    },
                    position: Position::new(3, 9),
                },
                new_name: "username".to_string(),
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap();

        let edit = result.expect("rename should produce an edit");
        let changes = edit.changes.unwrap();
        let edits = &changes[&Url::parse(uri).unwrap()];
        assert_eq!(
            edits.len(),
            1,
            "Only the User.name occurrence should be renamed: {:?}",
            edits
        );
        assert_eq!(edits[0].range.start.line, 3);
        assert_eq!(edits[0].new_text, "username");
    }

    /// A self-nesting type, for the rename tests that care about how the cost
    /// of a rename grows with the document.
    fn self_nesting_analyzer() -> RustAnalyzer {
        let mut analyzer = RustAnalyzer::with_root_type("crate::Server");
        analyzer.add_type(TypeInfo {
            name: "crate::Server".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "host".to_string(),
                    type_name: "String".to_string(),
                    ..Default::default()
                },
                FieldInfo {
                    name: "inner".to_string(),
                    type_name: "Server".to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        });
        analyzer
    }

    /// Renaming has to know which type owns each field spelled the way the one
    /// under the cursor is. Resolving that from each candidate's position
    /// re-walks the nesting path to the root and re-resolves every step of it
    /// once per candidate, so the work grew with the square of the document:
    /// renaming one field in a 19 KB document nested 800 values deep took 79
    /// seconds in a release build, and the editor that asked for it waits that
    /// long for an answer. Resolving each struct value's type once, on a single
    /// walk down the document, is linear.
    ///
    /// The bound below is deliberately loose. What it is testing is the
    /// complexity class, not the constant: the walk answers this in a few
    /// milliseconds, while re-resolving per candidate took 48 seconds at this
    /// depth in the unoptimized build the tests run in.
    #[tokio::test]
    async fn test_rename_cost_does_not_grow_with_the_square_of_the_document() {
        let backend = create_test_backend_with_analyzer(self_nesting_analyzer()).await;

        let depth = 400;
        let content = format!(
            "{}Server(host: \"innermost\"){}",
            "Server(host: \"x\", inner: ".repeat(depth),
            ")".repeat(depth)
        );
        let uri = "file:///deep.ron";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content));

        let started = std::time::Instant::now();
        let result = backend
            .rename(RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: Url::parse(uri).unwrap(),
                    },
                    // the outermost `host`
                    position: Position::new(0, 7),
                },
                new_name: "hostname".to_string(),
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap();
        let elapsed = started.elapsed();

        let changes = result
            .expect("rename should produce edits")
            .changes
            .unwrap();
        let edits = &changes[&Url::parse(uri).unwrap()];
        assert_eq!(
            edits.len(),
            depth + 1,
            "every nested Server owns a 'host', so all of them are renamed"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "renaming in a {depth}-deep document took {elapsed:?}"
        );
    }

    /// Hover, go-to-definition, completion and rename all resolve what type a
    /// value has by collecting the struct values it is nested in, which was a
    /// `Node::parent` walk from the cursor out to the root of the document.
    /// Tree-sitter stores no parent pointers — it answers `parent` by
    /// descending from the root again — so that walk cost the square of the
    /// depth, and so did the walk down the document that rename resolves
    /// owners on, which asked each value's own ancestry what field it belonged
    /// to. In a release build a single hover inside a 15 KB document nested
    /// 1000 values deep took 400 ms, 60 KB and 4000 deep took 5.4 s, and
    /// 120 KB and 8000 deep took 21 s — with the editor blocked on the answer
    /// and nothing bounding the wait.
    ///
    /// The bounds below are deliberately loose. What they test is the
    /// complexity class, not the constant: one descent answers all of these in
    /// well under a tenth of a second, while the walk out took 24 seconds per
    /// request at this depth in the unoptimized build the tests run in.
    #[tokio::test]
    async fn test_position_resolution_does_not_grow_with_the_square_of_the_document() {
        let backend = create_test_backend_with_analyzer(self_nesting_analyzer()).await;

        let depth = 4000;
        let level = "Server(host: \"x\", inner: ";
        let content = format!(
            "{}Server(host: \"innermost\"){}",
            level.repeat(depth),
            ")".repeat(depth)
        );
        let uri = "file:///deep.ron";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content));

        // Inside the `host` of the innermost value, which is as far from the
        // root as the document goes.
        let position = Position::new(0, (level.len() * depth + "Server(h".len()) as u32);
        let text_document_position = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse(uri).unwrap(),
            },
            position,
        };

        let started = std::time::Instant::now();
        let hover = backend
            .hover(HoverParams {
                text_document_position_params: text_document_position.clone(),
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap();
        let hovered = started.elapsed();
        let Some(Hover {
            contents: HoverContents::Markup(markup),
            ..
        }) = hover
        else {
            panic!("hover on the innermost 'host' returned nothing");
        };
        assert!(
            markup.value.contains("host: String"),
            "hover should describe the field. Got: {}",
            markup.value
        );
        assert!(
            hovered < std::time::Duration::from_secs(5),
            "hovering at depth {depth} took {hovered:?}"
        );

        let started = std::time::Instant::now();
        let completions = backend
            .completion(CompletionParams {
                text_document_position,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            })
            .await
            .unwrap();
        let completed = started.elapsed();
        assert!(
            completions.is_some(),
            "completion at depth {depth} returned nothing"
        );
        assert!(
            completed < std::time::Duration::from_secs(5),
            "completing at depth {depth} took {completed:?}"
        );
    }

    /// The owner of a field is resolved on a walk down the document, so sibling
    /// values have to be resolved independently of each other: every entry of a
    /// list owns its own copy of the field, and renaming one renames all of
    /// them.
    #[tokio::test]
    async fn test_rename_covers_every_entry_of_a_list() {
        let mut analyzer = RustAnalyzer::with_root_type("crate::Config");
        analyzer.add_type(TypeInfo {
            name: "crate::Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "servers".to_string(),
                type_name: "Vec<Server>".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "crate::Server".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "host".to_string(),
                type_name: "String".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let backend = create_test_backend_with_analyzer(analyzer).await;

        let content = "Config(\n    servers: [\n        Server(host: \"a\"),\n        Server(host: \"b\"),\n        Server(host: \"c\"),\n    ],\n)";
        let uri = "file:///list.ron";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content.to_string()));

        let result = backend
            .rename(RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: Url::parse(uri).unwrap(),
                    },
                    // the `host` of the middle entry
                    position: Position::new(3, 16),
                },
                new_name: "hostname".to_string(),
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap();

        let changes = result
            .expect("rename should produce edits")
            .changes
            .unwrap();
        let edits = &changes[&Url::parse(uri).unwrap()];
        let lines: Vec<u32> = edits.iter().map(|e| e.range.start.line).collect();
        assert_eq!(lines, vec![2, 3, 4], "all three entries: {edits:?}");
    }

    /// Each edit a rename produces is one byte-offset-to-position conversion,
    /// and a document holds as many occurrences of a field as it is long. When
    /// that conversion scans the row the offset sits on, a document written as
    /// one long line pays the length of the whole document per edit: renaming
    /// 16000 occurrences on one line took 1.2 seconds in a release build,
    /// against 102ms for the same occurrences one per line, and the cost
    /// quadrupled every time the document doubled.
    ///
    /// The bound below is deliberately loose. What it tests is the complexity
    /// class, not the constant.
    #[tokio::test]
    async fn test_rename_cost_does_not_grow_with_the_square_of_the_row() {
        let mut analyzer = RustAnalyzer::with_root_type("crate::Config");
        analyzer.add_type(TypeInfo {
            name: "crate::Config".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "servers".to_string(),
                type_name: "Vec<Server>".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "crate::Server".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "host".to_string(),
                type_name: "String".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let backend = create_test_backend_with_analyzer(analyzer).await;

        let entries = 4000;
        let content = format!(
            "Config(servers: [{}])",
            "Server(host: \"x\"), ".repeat(entries)
        );
        let uri = "file:///one-line.ron";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content));

        let started = std::time::Instant::now();
        let result = backend
            .rename(RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: Url::parse(uri).unwrap(),
                    },
                    // the `host` of the first entry
                    position: Position::new(0, 24),
                },
                new_name: "hostname".to_string(),
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap();
        let elapsed = started.elapsed();

        let changes = result
            .expect("rename should produce edits")
            .changes
            .unwrap();
        let edits = &changes[&Url::parse(uri).unwrap()];
        assert_eq!(
            edits.len(),
            entries,
            "every entry owns a 'host', so all of them are renamed"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "renaming {entries} occurrences on one line took {elapsed:?}"
        );
    }

    /// A workspace with a struct whose field is an enum, both carrying source
    /// positions, so that hover and go-to-definition can be checked against the
    /// same document.
    fn post_analyzer() -> RustAnalyzer {
        let source = std::path::PathBuf::from("/test/src/models.rs");
        let mut analyzer = RustAnalyzer::with_root_type("crate::models::Post");
        analyzer.add_type(TypeInfo {
            name: "crate::models::Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u64".to_string(),
                    docs: Some("The post's identifier".to_string()),
                    line: Some(10),
                    column: Some(4),
                    ..Default::default()
                },
                FieldInfo {
                    name: "post_type".to_string(),
                    type_name: "crate::models::PostType".to_string(),
                    line: Some(12),
                    column: Some(4),
                    ..Default::default()
                },
            ]),
            docs: Some("A post in the system".to_string()),
            source_file: Some(source.clone()),
            line: Some(8),
            column: Some(0),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "crate::models::PostType".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Short".to_string(),
                    docs: Some("A short post".to_string()),
                    line: Some(20),
                    column: Some(4),
                    ..Default::default()
                },
                EnumVariant {
                    name: "Detailed".to_string(),
                    fields: vec![FieldInfo {
                        name: "length".to_string(),
                        type_name: "usize".to_string(),
                        docs: Some("How long the post is".to_string()),
                        line: Some(24),
                        column: Some(8),
                        ..Default::default()
                    }],
                    docs: Some("A detailed post".to_string()),
                    line: Some(23),
                    column: Some(4),
                    ..Default::default()
                },
            ]),
            docs: Some("How a post is laid out".to_string()),
            source_file: Some(source),
            line: Some(18),
            column: Some(0),
            ..Default::default()
        });
        analyzer.add_type(TypeInfo {
            name: "crate::models::Tag".to_string(),
            docs: Some("A label on a post".to_string()),
            ..Default::default()
        });
        analyzer
    }

    /// Hover and go-to-definition answer the same question — what does the word
    /// under the cursor refer to? — so every kind of word a document can hold
    /// has to resolve the same way for both. These are all five kinds: a
    /// struct's own field, a field of the enum variant being spelled out, the
    /// variant name itself, a variant written as a bare field value, and a type
    /// named nowhere in particular.
    #[tokio::test]
    async fn test_hover_and_definition_resolve_the_same_words() {
        let backend = create_test_backend_with_analyzer(post_analyzer()).await;
        let uri: Url = "file:///test/post.ron".parse().unwrap();
        let content = "Post(\n    id: 42,\n    post_type: Detailed(\n        length: 1,\n    ),\n)";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content.to_string()));

        // Line/column pairs below are 0-based; the analyzer's are 1-based lines.
        let cases = [
            // `id` on the root struct: Post::id, declared at 10:4.
            ("struct field", 1, 6, (9, 4), "The post's identifier"),
            // `length` inside the `Detailed(...)` variant: declared at 24:8.
            ("variant field", 3, 10, (23, 8), "How long the post is"),
            // The variant name itself: declared at 23:4.
            ("variant name", 2, 18, (22, 4), "A detailed post"),
            // The root struct's own name: declared at 8:0.
            ("type name", 0, 2, (7, 0), "A post in the system"),
        ];

        for (label, line, character, expected, expected_docs) in cases {
            let position = Position::new(line, character);

            let definition = backend
                .goto_definition(GotoDefinitionParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position,
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .await
                .unwrap();
            let Some(GotoDefinitionResponse::Scalar(location)) = definition else {
                panic!("goto_definition on the {label} returned nothing");
            };
            assert_eq!(
                (location.range.start.line, location.range.start.character),
                expected,
                "definition of the {label}"
            );

            let hover = backend
                .hover(HoverParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position,
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .await
                .unwrap();
            let Some(Hover {
                contents: HoverContents::Markup(markup),
                ..
            }) = hover
            else {
                panic!("hover on the {label} returned nothing");
            };
            assert!(
                markup.value.contains(expected_docs),
                "hover on the {label} should document it. Got: {}",
                markup.value
            );
        }
    }

    /// A variant written as a bare field value (`post_type: Short`) is resolved
    /// through the field's declared type, which is a different lookup from the
    /// variant of the type the cursor is already in.
    #[tokio::test]
    async fn test_hover_and_definition_on_a_bare_variant_value() {
        let backend = create_test_backend_with_analyzer(post_analyzer()).await;
        let uri: Url = "file:///test/post.ron".parse().unwrap();
        let content = "Post(\n    id: 42,\n    post_type: Short,\n)";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content.to_string()));
        let position = Position::new(2, 17);

        let definition = backend
            .goto_definition(GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .unwrap();
        let Some(GotoDefinitionResponse::Scalar(location)) = definition else {
            panic!("goto_definition on a bare variant value returned nothing");
        };
        assert_eq!(
            (location.range.start.line, location.range.start.character),
            (19, 4),
            "definition should land on PostType::Short"
        );

        let hover = backend
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .unwrap();
        let Some(Hover {
            contents: HoverContents::Markup(markup),
            ..
        }) = hover
        else {
            panic!("hover on a bare variant value returned nothing");
        };
        assert!(
            markup.value.contains("A short post"),
            "hover should document PostType::Short. Got: {}",
            markup.value
        );
    }

    /// A word that names a registered type but nothing in the document's own
    /// type — here one mentioned in a comment — still resolves to that type.
    #[tokio::test]
    async fn test_hover_and_definition_fall_back_to_a_named_type() {
        let backend = create_test_backend_with_analyzer(post_analyzer()).await;
        let uri: Url = "file:///test/post.ron".parse().unwrap();
        let content = "// see Tag\nPost(\n    id: 42,\n)";
        backend
            .documents
            .write()
            .await
            .insert(uri.to_string(), Document::new(content.to_string()));
        let position = Position::new(0, 8);

        let hover = backend
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .unwrap();
        let Some(Hover {
            contents: HoverContents::Markup(markup),
            ..
        }) = hover
        else {
            panic!("hover on a type named in a comment returned nothing");
        };
        assert!(
            markup.value.contains("A label on a post"),
            "hover should document Tag. Got: {}",
            markup.value
        );

        // Tag carries no source file, so there is no location to jump to — but
        // the word still has to resolve to it rather than to anything else.
        let definition = backend
            .goto_definition(GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .unwrap();
        assert!(
            definition.is_none(),
            "a type with no source file has no definition to jump to"
        );
    }

    #[tokio::test]
    async fn test_rename_rejects_invalid_identifier() {
        let analyzer = RustAnalyzer::with_root_type("crate::Post");
        let backend = create_test_backend_with_analyzer(analyzer).await;

        let result = backend
            .rename(RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: Url::parse("file:///test.ron").unwrap(),
                    },
                    position: Position::new(0, 0),
                },
                new_name: "not valid".to_string(),
                work_done_progress_params: Default::default(),
            })
            .await;
        assert!(result.is_err());
    }
}
