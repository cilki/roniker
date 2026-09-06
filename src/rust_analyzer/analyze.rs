//! Source-file analysis: parse Rust with `syn` and extract type definitions.
//!
//! Everything in this module is compiled only under the `analyze` feature, so
//! the feature gate lives once on the `mod analyze;` declaration in the parent
//! rather than being repeated on each item.
use super::{EnumVariant, FieldInfo, RustAnalyzer, TypeInfo, TypeKind};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Component, Path};
use syn::{Fields, Item, ItemEnum, ItemStruct, ItemType};

impl RustAnalyzer {
    /// Add a Rust source file to the analyzer.
    ///
    /// Parses the file and extracts all type definitions (structs, enums, type aliases).
    /// The module path is inferred from the file path (e.g., `src/models/user.rs` -> `crate::models::user`).
    ///
    /// # Arguments
    /// * `file_path` - Path to the Rust source file
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of types extracted from the file
    /// * `Err` - If the file cannot be read or parsed
    pub fn add_file(&mut self, file_path: &Path) -> Result<usize> {
        let content = fs::read_to_string(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;
        self.add_source(file_path, &content)
    }

    /// Add Rust source code to the analyzer with an associated file path.
    ///
    /// Parses the source and extracts all type definitions.
    /// The module path is inferred from the file path.
    ///
    /// # Arguments
    /// * `file_path` - Path to associate with this source (used for module path inference)
    /// * `source` - Rust source code to parse
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of types extracted from the source
    /// * `Err` - If the source cannot be parsed
    pub fn add_source(&mut self, file_path: &Path, source: &str) -> Result<usize> {
        let syntax_tree = syn::parse_file(source).with_context(|| {
            format!("Failed to parse Rust source from: {}", file_path.display())
        })?;

        let initial_count = self.type_cache.len();
        self.extract_types_from_file(&syntax_tree, file_path);
        let types_added = self.type_cache.len() - initial_count;

        Ok(types_added)
    }

    /// Add Rust source code with a custom module prefix.
    ///
    /// Unlike `add_source`, this allows you to specify the module path directly
    /// instead of inferring it from a file path.
    ///
    /// # Arguments
    /// * `module_prefix` - The module path prefix (e.g., "crate::models")
    /// * `source` - Rust source code to parse
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of types extracted from the source
    /// * `Err` - If the source cannot be parsed
    pub fn add_source_with_prefix(&mut self, module_prefix: &str, source: &str) -> Result<usize> {
        let syntax_tree = syn::parse_file(source).context("Failed to parse Rust source")?;

        let initial_count = self.type_cache.len();
        self.extract_types_from_syntax_tree(&syntax_tree, module_prefix, None);
        let types_added = self.type_cache.len() - initial_count;

        Ok(types_added)
    }

    fn extract_types_from_file(&mut self, syntax_tree: &syn::File, file_path: &Path) {
        let module_prefix = self.file_path_to_module_path(file_path);
        self.extract_types_from_syntax_tree(syntax_tree, &module_prefix, Some(file_path));
    }

    fn extract_types_from_syntax_tree(
        &mut self,
        syntax_tree: &syn::File,
        module_prefix: &str,
        file_path: Option<&Path>,
    ) {
        for item in &syntax_tree.items {
            if let Item::Struct(struct_item) = item {
                if let Some(type_info) =
                    self.extract_struct_info(struct_item, module_prefix, file_path)
                {
                    self.type_cache.insert(type_info.name.clone(), type_info);
                }
            } else if let Item::Enum(enum_item) = item {
                if let Some(type_info) = self.extract_enum_info(enum_item, module_prefix, file_path)
                {
                    self.type_cache.insert(type_info.name.clone(), type_info);
                }
            } else if let Item::Type(type_item) = item {
                self.extract_type_alias(type_item, module_prefix);
            } else if let Item::Mod(mod_item) = item {
                // Handle inline modules
                if let Some((_, items)) = &mod_item.content {
                    let mod_name = mod_item.ident.to_string();
                    let nested_prefix = if module_prefix.is_empty() {
                        mod_name
                    } else {
                        format!("{}::{}", module_prefix, mod_name)
                    };

                    for item in items {
                        if let Item::Struct(struct_item) = item {
                            if let Some(type_info) =
                                self.extract_struct_info(struct_item, &nested_prefix, file_path)
                            {
                                self.type_cache.insert(type_info.name.clone(), type_info);
                            }
                        } else if let Item::Enum(enum_item) = item {
                            if let Some(type_info) =
                                self.extract_enum_info(enum_item, &nested_prefix, file_path)
                            {
                                self.type_cache.insert(type_info.name.clone(), type_info);
                            }
                        } else if let Item::Type(type_item) = item {
                            self.extract_type_alias(type_item, &nested_prefix);
                        }
                    }
                }
            }
        }
    }

    /// Convert a file path to a module path
    ///
    /// Example: `"src/models/user.rs"` -> `"crate::models::user"`
    fn file_path_to_module_path(&self, file_path: &Path) -> String {
        let components: Vec<_> = file_path
            .components()
            .filter_map(|c| match c {
                Component::Normal(os) => os.to_str(),
                _ => None,
            })
            .collect();

        if let Some(src_index) = components.iter().position(|c| *c == "src") {
            let relative = &components[src_index + 1..];
            if relative.is_empty() {
                return "crate".to_string();
            }

            let mut parts = relative.to_vec();
            if let Some(last) = parts.last_mut() {
                if last.ends_with(".rs") {
                    let stem = last.trim_end_matches(".rs");
                    *last = stem;
                }
                if *last == "lib" || *last == "main" {
                    parts.pop();
                    if parts.is_empty() {
                        return "crate".to_string();
                    }
                } else if *last == "mod" {
                    parts.pop();
                }
            }

            if parts.is_empty() {
                "crate".to_string()
            } else {
                format!("crate::{}", parts.join("::"))
            }
        } else {
            String::new()
        }
    }

    fn extract_struct_info(
        &self,
        struct_item: &ItemStruct,
        module_prefix: &str,
        file_path: Option<&Path>,
    ) -> Option<TypeInfo> {
        let struct_name = struct_item.ident.to_string();
        let full_path = qualified_name(module_prefix, &struct_name);

        let docs = extract_docs(&struct_item.attrs);
        let fields = extract_fields(&struct_item.fields);

        let start = struct_item.ident.span().start();
        let line = Some(start.line);
        let column = Some(start.column);
        let has_default = has_default_derive(&struct_item.attrs);
        let rename_all = serde_attributes::extract_serde_attributes(&struct_item.attrs).rename_all;

        Some(TypeInfo {
            name: full_path,
            kind: TypeKind::Struct(fields),
            docs,
            source_file: file_path.map(|p| p.to_path_buf()),
            line,
            column,
            has_default,
            rename_all,
        })
    }

    fn extract_enum_info(
        &self,
        enum_item: &ItemEnum,
        module_prefix: &str,
        file_path: Option<&Path>,
    ) -> Option<TypeInfo> {
        let enum_name = enum_item.ident.to_string();
        let full_path = qualified_name(module_prefix, &enum_name);

        let docs = extract_docs(&enum_item.attrs);

        let variants = enum_item
            .variants
            .iter()
            .map(|variant| {
                let variant_start = variant.ident.span().start();

                EnumVariant {
                    name: variant.ident.to_string(),
                    fields: extract_fields(&variant.fields),
                    docs: extract_docs(&variant.attrs),
                    line: Some(variant_start.line),
                    column: Some(variant_start.column),
                    rename: serde_attributes::extract_serde_attributes(&variant.attrs).rename,
                }
            })
            .collect();

        let start = enum_item.ident.span().start();
        let line = Some(start.line);
        let column = Some(start.column);
        let has_default = has_default_derive(&enum_item.attrs);
        let rename_all = serde_attributes::extract_serde_attributes(&enum_item.attrs).rename_all;

        Some(TypeInfo {
            name: full_path,
            kind: TypeKind::Enum(variants),
            docs,
            source_file: file_path.map(|p| p.to_path_buf()),
            line,
            column,
            has_default,
            rename_all,
        })
    }

    fn extract_type_alias(&mut self, type_item: &ItemType, module_prefix: &str) {
        let alias_name = type_item.ident.to_string();
        let full_alias_path = qualified_name(module_prefix, &alias_name);

        let target_type = type_to_string(&type_item.ty);

        self.type_aliases.insert(full_alias_path, target_type);
    }
}

/// Join a module prefix and a type name into a full path.
fn qualified_name(module_prefix: &str, name: &str) -> String {
    if module_prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", module_prefix, name)
    }
}

/// Extract FieldInfo entries from struct or enum-variant fields.
/// Unnamed (tuple) fields are named by their index.
fn extract_fields(fields: &Fields) -> Vec<FieldInfo> {
    match fields {
        Fields::Named(fields) => fields
            .named
            .iter()
            .map(|field| {
                let (line, column) = field
                    .ident
                    .as_ref()
                    .map(|i| {
                        let start = i.span().start();
                        (start.line, start.column)
                    })
                    .unzip();

                let serde_attrs = serde_attributes::extract_serde_attributes(&field.attrs);
                FieldInfo {
                    name: field
                        .ident
                        .as_ref()
                        .expect("named field must have ident")
                        .to_string(),
                    type_name: type_to_string(&field.ty),
                    docs: extract_docs(&field.attrs),
                    line,
                    column,
                    has_default: serde_attrs.has_default,
                    rename: serde_attrs.rename,
                    aliases: serde_attrs.aliases,
                    skip: serde_attrs.skip,
                    flatten: serde_attrs.flatten,
                }
            })
            .collect(),
        Fields::Unnamed(fields) => fields
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let serde_attrs = serde_attributes::extract_serde_attributes(&field.attrs);
                FieldInfo {
                    name: i.to_string(),
                    type_name: type_to_string(&field.ty),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: serde_attrs.has_default,
                    rename: serde_attrs.rename,
                    aliases: serde_attrs.aliases,
                    skip: serde_attrs.skip,
                    flatten: serde_attrs.flatten,
                }
            })
            .collect(),
        Fields::Unit => vec![],
    }
}

fn extract_docs(attrs: &[syn::Attribute]) -> Option<String> {
    let docs: Vec<String> = attrs
        .iter()
        .filter_map(|attr| {
            if attr.path().is_ident("doc") {
                attr.meta.require_name_value().ok().and_then(|nv| {
                    if let syn::Expr::Lit(lit) = &nv.value
                        && let syn::Lit::Str(s) = &lit.lit
                    {
                        return Some(s.value().trim().to_string());
                    }
                    None
                })
            } else {
                None
            }
        })
        .collect();

    if docs.is_empty() {
        None
    } else {
        Some(docs.join("\n"))
    }
}

fn has_default_derive(attrs: &[syn::Attribute]) -> bool {
    use syn::punctuated::Punctuated;
    use syn::{Path, Token};

    attrs.iter().any(|attr| {
        attr.path().is_ident("derive")
            && attr
                .parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)
                .map(|paths| {
                    paths
                        .iter()
                        .any(|p| p.segments.last().is_some_and(|s| s.ident == "Default"))
                })
                .unwrap_or(false)
    })
}

fn type_to_string(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(type_path) => normalize_type_tokens(&quote::quote!(#type_path).to_string()),
        syn::Type::Reference(type_ref) => {
            let inner = type_to_string(&type_ref.elem);
            if type_ref.mutability.is_some() {
                format!("&mut {}", inner)
            } else {
                format!("&{}", inner)
            }
        }
        _ => normalize_type_tokens(&quote::quote!(#ty).to_string()),
    }
}

/// Render a `quote!`-tokenized type string as an idiomatic Rust type name.
///
/// `quote!(#ty).to_string()` puts a space between every token, so a field of
/// type `Vec<String>` comes out as `"Vec < String >"`. Those names are shown
/// to users verbatim in completions, hover, and inline type hints, so collapse
/// the spurious punctuation spacing back to the conventional form
/// (`Vec<String>`, `HashMap<String, u32>`, `crate::foo::Bar`).
fn normalize_type_tokens(s: &str) -> String {
    let mut out = s.to_string();
    for (from, to) in [
        (" :: ", "::"),
        (" < ", "<"),
        (" > ", ">"),
        (" >", ">"),
        ("< ", "<"),
        (" , ", ", "),
        (" ,", ","),
        (" ; ", "; "),
        (" ;", ";"),
        ("& ", "&"),
    ] {
        out = out.replace(from, to);
    }
    out
}

mod serde_attributes {
    use syn::{Attribute, LitStr, Token};

    #[derive(Default)]
    pub struct SerdeAttributes {
        pub has_default: bool,
        pub rename: Option<String>,
        pub aliases: Vec<String>,
        pub rename_all: Option<String>,
        pub skip: bool,
        pub flatten: bool,
    }

    /// Extract serde attributes from a field, variant, or container attribute list.
    pub fn extract_serde_attributes(attrs: &[Attribute]) -> SerdeAttributes {
        let mut out = SerdeAttributes::default();

        for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("default") {
                    // #[serde(default)] or #[serde(default = "path")]
                    out.has_default = true;
                    if meta.input.peek(Token![=]) {
                        let _: LitStr = meta.value()?.parse()?;
                    }
                } else if meta.path.is_ident("rename") {
                    if meta.input.peek(Token![=]) {
                        let lit: LitStr = meta.value()?.parse()?;
                        out.rename = Some(lit.value());
                    } else {
                        // rename(serialize = "...", deserialize = "...")
                        meta.parse_nested_meta(|inner| {
                            let lit: LitStr = inner.value()?.parse()?;
                            if inner.path.is_ident("deserialize") {
                                out.rename = Some(lit.value());
                            }
                            Ok(())
                        })?;
                    }
                } else if meta.path.is_ident("alias") {
                    // #[serde(alias = "...")], repeatable
                    let lit: LitStr = meta.value()?.parse()?;
                    out.aliases.push(lit.value());
                } else if meta.path.is_ident("rename_all") {
                    if meta.input.peek(Token![=]) {
                        let lit: LitStr = meta.value()?.parse()?;
                        out.rename_all = Some(lit.value());
                    } else {
                        meta.parse_nested_meta(|inner| {
                            let lit: LitStr = inner.value()?.parse()?;
                            if inner.path.is_ident("deserialize") {
                                out.rename_all = Some(lit.value());
                            }
                            Ok(())
                        })?;
                    }
                } else if meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing") {
                    out.skip = true;
                } else if meta.path.is_ident("flatten") {
                    out.flatten = true;
                } else if meta.input.peek(Token![=]) {
                    // Consume values of other attributes so parsing continues
                    let _: syn::Lit = meta.value()?.parse()?;
                } else if meta.input.peek(syn::token::Paren) {
                    // Consume parenthesized attributes like bound(...)
                    meta.parse_nested_meta(|inner| {
                        if inner.input.peek(Token![=]) {
                            let _: syn::Lit = inner.value()?.parse()?;
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            });
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_analyzer::{rename_all_field, rename_all_variant};
    use std::path::PathBuf;

    #[test]
    fn test_type_to_string_is_idiomatic() {
        let cases = [
            ("Vec<String>", "Vec<String>"),
            ("Option<u64>", "Option<u64>"),
            ("HashMap<String, u32>", "HashMap<String, u32>"),
            (
                "std::collections::HashMap<String, Vec<u8>>",
                "std::collections::HashMap<String, Vec<u8>>",
            ),
            ("(u8, u16)", "(u8, u16)"),
            ("[u8; 4]", "[u8; 4]"),
            ("&str", "&str"),
            ("&mut Foo", "&mut Foo"),
            ("Box<dyn Trait>", "Box<dyn Trait>"),
            ("crate::config::AppConfig", "crate::config::AppConfig"),
        ];
        for (input, expected) in cases {
            let ty: syn::Type = syn::parse_str(input).unwrap();
            assert_eq!(type_to_string(&ty), expected, "for input {input}");
        }
    }

    #[test]
    fn test_rust_analyzer_serialization_roundtrip() {
        let mut analyzer = RustAnalyzer::with_root_type("crate::Test");
        analyzer.add_type(TypeInfo {
            name: "Test".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        });

        let json = serde_json::to_string(&analyzer).unwrap();
        let deserialized: RustAnalyzer = serde_json::from_str(&json).unwrap();

        assert!(deserialized.get_type_info("Test").is_some());
        assert_eq!(deserialized.root_type, Some("crate::Test".to_string()));
    }

    #[test]
    fn test_add_source_with_prefix() {
        let mut analyzer = RustAnalyzer::new();

        let source = r#"
            /// A user in the system
            pub struct User {
                /// The user's unique ID
                pub id: u64,
                pub name: String,
            }

            pub enum Status {
                Active,
                Inactive,
            }
        "#;

        let count = analyzer
            .add_source_with_prefix("crate::models", source)
            .unwrap();

        assert_eq!(count, 2, "Should extract 2 types (User and Status)");

        let user = analyzer.get_type_info("crate::models::User");
        assert!(user.is_some(), "User type should exist");

        let user = user.unwrap();
        assert_eq!(user.docs, Some("A user in the system".to_string()));
        assert!(matches!(user.kind, TypeKind::Struct(_)));

        if let TypeKind::Struct(fields) = &user.kind {
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "id");
            assert_eq!(fields[0].docs, Some("The user's unique ID".to_string()));
        }

        let status = analyzer.get_type_info("crate::models::Status");
        assert!(status.is_some(), "Status type should exist");
    }

    #[test]
    fn test_add_source_with_file_path() {
        let mut analyzer = RustAnalyzer::new();

        let source = r#"
            pub struct Config {
                pub debug: bool,
            }
        "#;

        let file_path = PathBuf::from("src/settings/config.rs");
        let count = analyzer.add_source(&file_path, source).unwrap();

        assert_eq!(count, 1);

        // Should be accessible via the inferred module path
        let config = analyzer.get_type_info("crate::settings::config::Config");
        assert!(config.is_some(), "Config should be at inferred path");
    }

    #[test]
    fn test_add_type_directly() {
        let mut analyzer = RustAnalyzer::new();

        let type_info = TypeInfo {
            name: "crate::MyType".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "value".to_string(),
                type_name: "i32".to_string(),
                docs: Some("The value".to_string()),
                line: None,
                column: None,
                has_default: false,
                ..Default::default()
            }]),
            docs: Some("My custom type".to_string()),
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        };

        analyzer.add_type(type_info);

        assert!(analyzer.has_type("crate::MyType"));
        assert_eq!(analyzer.type_count(), 1);
    }

    #[test]
    fn test_remove_type() {
        let mut analyzer = RustAnalyzer::new();

        analyzer.add_type(TypeInfo {
            name: "crate::ToRemove".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        });

        assert!(analyzer.has_type("crate::ToRemove"));

        let removed = analyzer.remove_type("crate::ToRemove");
        assert!(removed.is_some());
        assert!(!analyzer.has_type("crate::ToRemove"));
    }

    #[test]
    fn test_clear() {
        let mut analyzer = RustAnalyzer::new();

        analyzer
            .add_source_with_prefix("crate", "pub struct A {} pub struct B {}")
            .unwrap();

        assert_eq!(analyzer.type_count(), 2);

        analyzer.clear();

        assert_eq!(analyzer.type_count(), 0);
    }

    #[test]
    fn test_type_alias() {
        let mut analyzer = RustAnalyzer::new();

        analyzer.add_type(TypeInfo {
            name: "crate::RealType".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: Some("The real type".to_string()),
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        });

        analyzer.add_type_alias("crate::AliasType", "crate::RealType");

        // Looking up the alias should return the real type
        let via_alias = analyzer.get_type_info("crate::AliasType");
        assert!(via_alias.is_some());
        assert_eq!(via_alias.unwrap().name, "crate::RealType");
    }

    #[test]
    fn test_inline_module() {
        let mut analyzer = RustAnalyzer::new();

        let source = r#"
            pub mod inner {
                pub struct InnerType {
                    pub x: i32,
                }
            }
        "#;

        analyzer.add_source_with_prefix("crate", source).unwrap();

        let inner_type = analyzer.get_type_info("crate::inner::InnerType");
        assert!(inner_type.is_some(), "InnerType should be found");
    }

    #[test]
    fn test_get_type_info_strips_whitespace() {
        let mut analyzer = RustAnalyzer::new();

        analyzer.add_type(TypeInfo {
            name: "sandpolis_probe::config::ProbeLayerConfig".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        });

        // syn's `quote!(#ty).to_string()` produces field type names with
        // spaces around `::`. Callers may forward that string verbatim, so
        // lookups must tolerate the whitespace.
        let found = analyzer.get_type_info("sandpolis_probe :: config :: ProbeLayerConfig");
        assert!(
            found.is_some(),
            "lookup should normalize whitespace in the type path"
        );
        assert_eq!(found.unwrap().name, "sandpolis_probe::config::ProbeLayerConfig");
    }

    #[test]
    fn test_simple_name_lookup() {
        let mut analyzer = RustAnalyzer::new();

        analyzer.add_type(TypeInfo {
            name: "crate::deeply::nested::MyStruct".to_string(),
            kind: TypeKind::Struct(vec![]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        });

        // Should be able to find by simple name
        let found = analyzer.get_type_info("MyStruct");
        assert!(found.is_some(), "Should find by simple name");
        assert_eq!(found.unwrap().name, "crate::deeply::nested::MyStruct");
    }

    #[test]
    fn test_validate_field_types_all_known() {
        let mut analyzer = RustAnalyzer::new();

        // Add User type
        analyzer.add_type(TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
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
        });

        // Add Post type that references User
        analyzer.add_type(TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "author".to_string(),
                    type_name: "User".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "tags".to_string(),
                    type_name: "Vec<String>".to_string(),
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
        });

        let errors = analyzer.validate_field_types();
        assert!(errors.is_empty(), "All types should be known: {:?}", errors);
    }

    #[test]
    fn test_validate_field_types_unknown() {
        let mut analyzer = RustAnalyzer::new();

        // Add Post type that references unknown User type
        analyzer.add_type(TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                    ..Default::default()
                },
                FieldInfo {
                    name: "author".to_string(),
                    type_name: "UnknownUser".to_string(), // NOT registered
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
        });

        let errors = analyzer.validate_field_types();
        assert_eq!(errors.len(), 1, "Should find 1 unknown type");
        assert_eq!(errors[0].0, "Post");
        assert_eq!(errors[0].1, "author");
        assert_eq!(errors[0].2, "UnknownUser");
    }

    #[test]
    fn test_validate_field_types_unknown_in_vec() {
        let mut analyzer = RustAnalyzer::new();

        // Add Container type with Vec<UnknownItem>
        analyzer.add_type(TypeInfo {
            name: "Container".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "items".to_string(),
                type_name: "Vec<UnknownItem>".to_string(), // NOT registered
                docs: None,
                line: None,
                column: None,
                has_default: false,
                ..Default::default()
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
            ..Default::default()
        });

        let errors = analyzer.validate_field_types();
        assert_eq!(errors.len(), 1, "Should find 1 unknown type in Vec");
        assert_eq!(errors[0].2, "UnknownItem");
    }

    #[test]
    fn test_deserialize_pre_0_4_json() {
        // JSON produced by roniker 0.3.x, before the serde-attribute fields
        // (rename/skip/flatten/rename_all) existed. Must still deserialize.
        let json = r#"{
            "name": "crate::Config",
            "kind": {
                "Struct": [{
                    "name": "debug",
                    "type_name": "bool",
                    "docs": null,
                    "line": 3,
                    "column": 4,
                    "has_default": false
                }]
            },
            "docs": null,
            "source_file": null,
            "line": 1,
            "column": 0,
            "has_default": false
        }"#;
        let info: TypeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.name, "crate::Config");
        assert_eq!(info.rename_all, None);
        let fields = info.fields().unwrap();
        assert_eq!(fields[0].name, "debug");
        assert_eq!(fields[0].rename, None);
        assert!(fields[0].aliases.is_empty());
        assert!(!fields[0].skip);
        assert!(!fields[0].flatten);

        let variant_json = r#"{
            "name": "Active",
            "fields": [],
            "docs": null,
            "line": null,
            "column": null
        }"#;
        let variant: EnumVariant = serde_json::from_str(variant_json).unwrap();
        assert_eq!(variant.name, "Active");
        assert_eq!(variant.rename, None);
    }

    #[test]
    fn test_extract_serde_attributes() {
        let mut analyzer = RustAnalyzer::new();
        analyzer
            .add_source_with_prefix(
                "crate",
                r#"
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                pub struct Config {
                    pub max_connections: u32,
                    #[serde(rename = "kind")]
                    pub config_kind: String,
                    #[serde(default = "default_timeout")]
                    pub timeout_ms: u64,
                    #[serde(skip)]
                    pub runtime_state: String,
                    #[serde(flatten)]
                    pub extra: Extra,
                }

                #[derive(serde::Deserialize)]
                pub struct Extra {
                    pub verbose: bool,
                }

                #[derive(serde::Deserialize)]
                #[serde(rename_all = "snake_case")]
                pub enum Mode {
                    FastMode,
                    #[serde(rename = "legacy")]
                    OldMode,
                }
                "#,
            )
            .unwrap();

        let config = analyzer.get_type_info("crate::Config").unwrap();
        assert_eq!(config.rename_all.as_deref(), Some("camelCase"));

        let kind = config.find_field("config_kind").unwrap();
        assert_eq!(kind.rename.as_deref(), Some("kind"));

        let timeout = config.find_field("timeout_ms").unwrap();
        assert!(timeout.has_default, "default = \"path\" sets has_default");

        assert!(config.find_field("runtime_state").unwrap().skip);
        assert!(config.find_field("extra").unwrap().flatten);

        let mode = analyzer.get_type_info("crate::Mode").unwrap();
        assert_eq!(mode.rename_all.as_deref(), Some("snake_case"));
        let old = mode.find_variant("OldMode").unwrap();
        assert_eq!(old.rename.as_deref(), Some("legacy"));

        // Serialized-name resolution built on the extracted attributes
        assert_eq!(
            config
                .find_field("max_connections")
                .unwrap()
                .serialized_name(config.rename_all.as_deref()),
            "maxConnections"
        );
        assert_eq!(
            kind.serialized_name(config.rename_all.as_deref()),
            "kind",
            "explicit rename overrides rename_all"
        );
        assert_eq!(old.serialized_name(mode.rename_all.as_deref()), "legacy");
        assert_eq!(
            mode.find_variant("FastMode")
                .unwrap()
                .serialized_name(mode.rename_all.as_deref()),
            "fast_mode"
        );

        // Lookup by serialized name, with Rust-name fallback
        assert!(config.find_field_serialized("maxConnections").is_some());
        assert!(config.find_field_serialized("max_connections").is_some());
        assert!(mode.find_variant_serialized("fast_mode").is_some());
        assert!(mode.find_variant_serialized("legacy").is_some());

        // effective_fields: skip excluded, flatten expanded, names serialized
        let effective = config.effective_fields(&analyzer);
        let names: Vec<&str> = effective.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["maxConnections", "kind", "timeoutMs", "verbose"]);
        assert!(!config.has_unresolved_flatten(&analyzer));

        let config = config.clone();
        analyzer.remove_type("crate::Extra");
        assert!(config.has_unresolved_flatten(&analyzer));
    }

    #[test]
    fn test_extract_field_aliases() {
        let mut analyzer = RustAnalyzer::new();
        analyzer
            .add_source_with_prefix(
                "crate",
                r#"
                #[derive(serde::Deserialize)]
                pub struct Config {
                    #[serde(alias = "old_port", alias = "legacy_port")]
                    pub port: u16,
                    pub host: String,
                }
                "#,
            )
            .unwrap();

        let config = analyzer.get_type_info("crate::Config").unwrap();
        let port = config.find_field("port").unwrap();
        assert_eq!(port.aliases, vec!["old_port", "legacy_port"]);
        assert!(config.find_field("host").unwrap().aliases.is_empty());

        // An alias is an accepted deserialization name; the canonical serialized
        // name is unchanged.
        assert!(port.accepts_name("port", None));
        assert!(port.accepts_name("old_port", None));
        assert!(port.accepts_name("legacy_port", None));
        assert!(!port.accepts_name("nonsense", None));

        // Lookup by alias resolves to the field
        assert!(config.find_field_serialized("old_port").is_some());
    }

    #[test]
    fn test_rename_all_matches_serde_json() {
        // Cross-check our rename_all implementation against serde itself.
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct CamelFields {
            max_connections: u32,
            single: bool,
        }

        let value = serde_json::to_value(CamelFields {
            max_connections: 1,
            single: true,
        })
        .unwrap();
        let object = value.as_object().unwrap();
        for rust_name in ["max_connections", "single"] {
            assert!(object.contains_key(&rename_all_field(rust_name, "camelCase")));
        }

        #[derive(serde::Serialize)]
        #[serde(rename_all = "kebab-case")]
        enum KebabModes {
            FastMode,
            SlowIoMode,
        }

        for (variant, rust_name) in [(KebabModes::FastMode, "FastMode"), (KebabModes::SlowIoMode, "SlowIoMode")] {
            let value = serde_json::to_value(variant).unwrap();
            assert_eq!(
                value.as_str().unwrap(),
                rename_all_variant(rust_name, "kebab-case")
            );
        }

        assert_eq!(rename_all_variant("FastMode", "SCREAMING_SNAKE_CASE"), "FAST_MODE");
        assert_eq!(rename_all_field("max_size", "PascalCase"), "MaxSize");
        assert_eq!(rename_all_field("max_size", "SCREAMING-KEBAB-CASE"), "MAX-SIZE");
    }
}
