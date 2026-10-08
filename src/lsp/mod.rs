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
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Clone)]
pub struct Document {
    content: String,
    // Parsed once per change and shared by all request handlers.
    // Cloning a Tree is cheap (tree-sitter trees are copy-on-write).
    tree: Option<tree_sitter::Tree>,
}

impl Document {
    pub fn new(content: String) -> Self {
        let tree = ts_utils::parse(&content);
        Self { content, tree }
    }

    /// Apply a single LSP content change: incremental when a range is given,
    /// full-document replacement otherwise.
    pub fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
        let Some(range) = change.range else {
            *self = Document::new(change.text);
            return;
        };

        let start_byte = ts_utils::position_to_byte_offset(&self.content, range.start);
        let old_end_byte = ts_utils::position_to_byte_offset(&self.content, range.end);

        // tree-sitter Points use byte columns
        let start_position = byte_point(&self.content, start_byte, range.start.line as usize);
        let old_end_position = byte_point(&self.content, old_end_byte, range.end.line as usize);

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
        self.tree = ts_utils::parse_with(&self.content, self.tree.as_ref());
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

        // Get document content
        let (content, tree) = {
            let documents = self.documents.read().await;
            match documents.get(&uri) {
                Some(doc) => (doc.content.clone(), doc.tree.clone()),
                None => return Ok(None),
            }
        };

        // Get the word at cursor position - early return if none
        let word = match get_word_at_position(&content, position) {
            Some(w) => w,
            None => return Ok(None),
        };

        let current_type_info = self.resolve_owner_type_at(&content, tree.as_ref(), position);

        // Check if the word is a valid field name in the current context type
        if let (Some(info), Some(tree)) = (current_type_info.as_ref(), tree.as_ref()) {
            // The cursor may be on a field inside an enum variant
            if let Some(response) =
                self.resolve_variant_field_definition(info, tree, &content, position, &word)
            {
                return response;
            }

            // Check if the word is a struct field name
            if let Some(field) = info.find_field(&word) {
                return create_location_response(&info.source_file, field.line, field.column);
            }

            // Check if the word is a valid enum variant in this type
            if let Some(variant) = info.find_variant(&word) {
                return create_location_response(&info.source_file, variant.line, variant.column);
            }

            // Check if the word matches the type name itself
            if info.name.ends_with(&format!("::{}", word)) || info.name == word {
                return create_location_response(&info.source_file, info.line, info.column);
            }

            // Check if we're on a field, and if the word is a variant of that field's type
            // e.g., in "post_type: Short", if cursor is near Short, check if it's a variant of PostType
            if let Some(field_name) =
                tree_sitter_parser::get_field_at_position(tree, &content, position)
                && let Some(field) = info.find_field(&field_name)
                && let Some(field_type_info) = self.rust_analyzer.get_type_info(&field.type_name)
                && let Some(variant) = field_type_info.find_variant(&word)
            {
                return create_location_response(
                    &field_type_info.source_file,
                    variant.line,
                    variant.column,
                );
            }
        }

        // If no type annotation, or word doesn't match a field, try to find as a type
        if let Some(info) = self.rust_analyzer.get_type_info(&word).cloned() {
            return create_location_response(&info.source_file, info.line, info.column);
        }

        Ok(None)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        // Get document content
        let (content, tree) = {
            let documents = self.documents.read().await;
            match documents.get(&uri) {
                Some(doc) => (doc.content.clone(), doc.tree.clone()),
                None => return Ok(None),
            }
        };

        // Get the word at cursor position
        let word = match get_word_at_position(&content, position) {
            Some(w) => w,
            None => return Ok(None),
        };

        {
            let current_type_info = self.resolve_owner_type_at(&content, tree.as_ref(), position);

            // Now check what the word at cursor is
            if let (Some(type_info), Some(tree)) = (current_type_info, tree.as_ref()) {
                // Case 1: Hovering over a field name, which is either a field of
                // the enum variant being spelled out or one of the struct's own.
                if let Some(field_name) =
                    tree_sitter_parser::get_field_at_position(tree, &content, position)
                    && field_name == word
                {
                    let field =
                        tree_sitter_parser::find_current_variant_context(tree, &content, position)
                            .and_then(|variant_name| type_info.find_variant(&variant_name))
                            .and_then(|variant| {
                                variant.fields.iter().find(|f| f.name == field_name)
                            })
                            .or_else(|| {
                                type_info
                                    .fields()
                                    .and_then(|fields| fields.iter().find(|f| f.name == field_name))
                            });

                    if let Some(field) = field {
                        return Ok(Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: format!(
                                    "```rust\n{}: {}\n```\n\n{}",
                                    field.name,
                                    field.type_name,
                                    field.docs.as_deref().unwrap_or("")
                                ),
                            }),
                            range: None,
                        }));
                    }
                }

                // Case 2: Hovering over a variant name
                if let Some(variant) = type_info.find_variant(&word) {
                    let hover_text = format_variant_hover(&type_info.name, variant);

                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: hover_text,
                        }),
                        range: None,
                    }));
                }

                // Case 3: Hovering over the type name itself (or check if word matches the current type)
                if type_info.name.ends_with(&format!("::{}", word)) || type_info.name == word {
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: self.format_type_hover(&type_info),
                        }),
                        range: None,
                    }));
                }

                // Case 4: Check if hovering over a field value that's a variant (like "Short" in "post_type: Short")
                if let Some(field_name) =
                    tree_sitter_parser::get_field_at_position(tree, &content, position)
                    && let Some(field) = type_info.find_field(&field_name)
                {
                    // Get the type of this field
                    if let Some(field_type_info) =
                        self.rust_analyzer.get_type_info(&field.type_name).cloned()
                    {
                        // Check if word is a variant of this field's type
                        if let Some(variant) = field_type_info.find_variant(&word) {
                            let hover_text = format_variant_hover(&field_type_info.name, variant);

                            return Ok(Some(Hover {
                                contents: HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: hover_text,
                                }),
                                range: None,
                            }));
                        }
                    }
                }
            }
        }

        // Case 5: No type annotation - try to find type by name
        if let Some(info) = self.rust_analyzer.get_type_info(&word).cloned() {
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: self.format_type_hover(&info),
                }),
                range: None,
            }));
        }

        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let position = params.text_document_position.position;

        // Get document content
        let (content, tree) = {
            let documents = self.documents.read().await;
            match documents.get(&uri) {
                Some(doc) => (doc.content.clone(), doc.tree.clone()),
                None => return Ok(None),
            }
        };

        let current_type_info = self.resolve_owner_type_at(&content, tree.as_ref(), position);

        if let (Some(type_info), Some(tree)) = (current_type_info, tree.as_ref()) {
            let completions = completion::generate_completions_for_type(
                tree,
                &content,
                position,
                &type_info,
                self.rust_analyzer.clone(),
            );
            return Ok(Some(CompletionResponse::Array(completions)));
        }

        Ok(None)
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let url = params.text_document.uri;
        let uri = url.to_string();

        // Get document content
        let (content, tree) = {
            let documents = self.documents.read().await;
            match documents.get(&uri) {
                Some(doc) => (doc.content.clone(), doc.tree.clone()),
                None => return Ok(None),
            }
        };

        if let Some(type_info) = self.resolve_root_type_info(&content, tree.as_ref()) {
            let actions = match tree.as_ref() {
                Some(tree) => code_actions::generate_code_actions(
                    tree,
                    &content,
                    &type_info,
                    &url,
                    self.rust_analyzer.clone(),
                    &params.context.diagnostics,
                ),
                None => Vec::new(),
            };

            if !actions.is_empty() {
                return Ok(Some(actions));
            }
        }

        Ok(None)
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.to_string();

        let documents = self.documents.read().await;
        if let Some(doc) = documents.get(&uri) {
            // Basic RON formatting: normalize whitespace and indentation
            let formatted = Backend::format_ron(&doc.content);

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
        let range = params.range;

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

        let (content, tree) = {
            let documents = self.documents.read().await;
            match documents.get(&uri) {
                Some(doc) => (doc.content.clone(), doc.tree.clone()),
                None => return Ok(None),
            }
        };
        let Some(tree) = tree else {
            return Ok(None);
        };
        let Some(name_node) = field_name_node_at(&tree, &content, position) else {
            return Ok(None);
        };
        let Some(field_name) = ts_utils::node_text(&name_node, &content).map(str::to_string)
        else {
            return Ok(None);
        };

        // Resolve the type that owns the field being renamed, so occurrences of
        // the same name under *other* types are left alone
        let target_type = self.resolve_owner_type_at(&content, Some(&tree), position);

        // Collect the exact name-node ranges of matching fields
        let mut changes = Vec::new();
        for field_node in ts_utils::descendants_by_kind(&tree, "field") {
            let Some(node) = field_node.child(0) else {
                continue;
            };
            if ts_utils::node_text(&node, &content) != Some(&field_name) {
                continue;
            }

            let node_pos = ts_utils::point_to_position(&content, node.start_position());
            let owner = self.resolve_owner_type_at(&content, Some(&tree), node_pos);
            // When either side can't be resolved, fall back to matching by name
            if let (Some(target), Some(owner)) = (&target_type, &owner)
                && target.name != owner.name
            {
                continue;
            }

            changes.push(TextEdit {
                range: ts_utils::node_to_lsp_range(&node, &content),
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
        ts_utils::find_ancestor_by_kind(node, "field")?
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
    fn resolve_root_type_info(
        &self,
        content: &str,
        tree: Option<&tree_sitter::Tree>,
    ) -> Option<rust_analyzer::TypeInfo> {
        self.rust_analyzer.root_type_info().cloned().or_else(|| {
            let main_value = ts_utils::find_main_value(tree?)?;
            let name = ts_utils::struct_name(&main_value, content)?;
            self.rust_analyzer.get_type_info(name).cloned()
        })
    }

    /// The variant-field cases of goto_definition: the cursor is inside an enum
    /// variant and `word` may name one of the variant's fields.
    /// Returns `Some(response)` when a definition was found.
    fn resolve_variant_field_definition(
        &self,
        info: &rust_analyzer::TypeInfo,
        tree: &tree_sitter::Tree,
        content: &str,
        position: Position,
        word: &str,
    ) -> Option<Result<Option<GotoDefinitionResponse>>> {
        let variant_name =
            tree_sitter_parser::find_current_variant_context(tree, content, position)?;

        // We might be in a variant of the current type OR a variant of a field's type.
        // Try the current type first.
        if let Some(variant) = info.find_variant(&variant_name)
            && let Some(field) = variant.fields.iter().find(|f| f.name == word)
        {
            return Some(create_location_response(
                &info.source_file,
                field.line,
                field.column,
            ));
        }

        // Otherwise check if we're in a field that contains the variant,
        // e.g. on "length" inside "post_type: Detailed(length: 1)"
        let field_name =
            tree_sitter_parser::get_containing_field_context(tree, content, position)?;
        let field = info.find_field(&field_name)?;
        let field_type_info = self.rust_analyzer.get_type_info(&field.type_name)?;
        let variant = field_type_info.find_variant(&variant_name)?;
        let variant_field = variant.fields.iter().find(|f| f.name == word)?;
        Some(create_location_response(
            &field_type_info.source_file,
            variant_field.line,
            variant_field.column,
        ))
    }

    /// Resolve the TypeInfo that owns the fields at `position`: the document's
    /// root type navigated through the nested type contexts around the
    /// position. Used by goto_definition, hover, completion and rename.
    fn resolve_owner_type_at(
        &self,
        content: &str,
        tree: Option<&tree_sitter::Tree>,
        position: Position,
    ) -> Option<rust_analyzer::TypeInfo> {
        let root = self.resolve_root_type_info(content, tree)?;
        let contexts = tree
            .map(|t| tree_sitter_parser::find_type_context_at_position(t, content, position))
            .unwrap_or_default();
        navigation::navigate_type_contexts(&self.rust_analyzer, Some(root), &contexts)
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

        let type_info = self.resolve_root_type_info(content, doc.tree.as_ref());

        let diagnostics = if let Some(type_info) = type_info {
            diagnostics::validate_ron_with_analyzer(
                content,
                doc.tree.as_ref(),
                &type_info,
                self.rust_analyzer.clone(),
            )
            .await
        } else {
            vec![]
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
/// `info_diagnostics` turns on the informational diagnostics that annotate a
/// field with the type it is declared as (`port: u16`). They are only emitted
/// for the root value's own fields, and only where the value doesn't already
/// name its type; pass `false` to publish warnings and errors alone.
pub async fn serve(analyzer: RustAnalyzer, info_diagnostics: bool) {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) =
        LspService::new(|client| Backend::new(client, analyzer, info_diagnostics));
    Server::new(stdin, stdout, socket).serve(service).await;
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
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "port".to_string(),
                type_name: "u16".to_string(),
                docs: Some("Port to bind".to_string()),
                line: Some(7),
                column: Some(4),
                ..Default::default()
            }]),
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
            // Server's fields, not Config's.
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
                labels.contains(&"port"),
                "completion should offer Server's own fields ({label}). Got: {labels:?}"
            );
            assert!(
                !labels.contains(&"server"),
                "completion should not offer Config's fields ({label}). Got: {labels:?}"
            );
        }
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
        println!("Word at position: {:?}", word);
        assert_eq!(
            word,
            Some("length".to_string()),
            "Should detect 'length' word"
        );

        let tree = ts_utils::RonParser::new().parse(content).unwrap();
        let variant = tree_sitter_parser::find_current_variant_context(&tree, content, position);
        println!("Variant context: {:?}", variant);
        assert_eq!(
            variant,
            Some("Detailed".to_string()),
            "Should detect Detailed variant"
        );

        let containing_field =
            tree_sitter_parser::get_containing_field_context(&tree, content, position);
        println!("Containing field: {:?}", containing_field);
        assert_eq!(
            containing_field,
            Some("post_type".to_string()),
            "Should detect post_type field"
        );

        // This test shows that all the building blocks work!
        // The problem must be in how goto_definition uses them
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
        let fields = tree_sitter_parser::extract_fields_from_ron(tree, &doc.content);
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
        let fields = tree_sitter_parser::extract_fields_from_ron(tree, &doc.content);
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
        let fields = tree_sitter_parser::extract_fields_from_ron(tree, &doc.content);
        assert!(fields.contains(&"name".to_string()));

        // A change without a range replaces the whole document
        doc.apply_change(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "Post(title: \"hi\")".to_string(),
        });
        assert_eq!(doc.content, "Post(title: \"hi\")");
        let tree = doc.tree.as_ref().unwrap();
        let fields = tree_sitter_parser::extract_fields_from_ron(tree, &doc.content);
        assert_eq!(fields, vec!["title".to_string()]);
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
