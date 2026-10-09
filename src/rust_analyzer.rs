use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FieldInfo {
    pub name: String,
    pub type_name: String,
    pub docs: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub has_default: bool,
    /// `#[serde(rename = "...")]` on the field
    #[serde(default)]
    pub rename: Option<String>,
    /// `#[serde(alias = "...")]` names on the field. Serde accepts each of these
    /// (in addition to the serialized name) when deserializing, so they must not
    /// be flagged as unknown fields.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// `#[serde(skip)]` or `#[serde(skip_deserializing)]` on the field
    #[serde(default)]
    pub skip: bool,
    /// `#[serde(flatten)]` on the field
    #[serde(default)]
    pub flatten: bool,
}

impl FieldInfo {
    pub fn is_optional(&self) -> bool {
        if self.has_default {
            return true;
        }
        // Normalize first: type names from `quote!` are tokenized with spaces
        // (e.g. "Option < String >")
        self.type_name.replace(' ', "").starts_with("Option<")
    }

    /// Whether this field is positional rather than named — i.e. a field of a
    /// tuple or newtype struct/variant, whose name is its index (`"0"`, `"1"`,
    /// ...) instead of a real identifier. Named-field validation does not apply
    /// to these.
    pub fn is_positional(&self) -> bool {
        self.name.parse::<usize>().is_ok()
    }

    /// The name serde expects for this field, honoring `#[serde(rename)]`
    /// and the container's `#[serde(rename_all)]` convention.
    pub fn serialized_name(&self, container_rename_all: Option<&str>) -> String {
        if let Some(rename) = &self.rename {
            return rename.clone();
        }
        if let Some(convention) = container_rename_all {
            return rename_all_field(&self.name, convention);
        }
        self.name.clone()
    }

    /// Whether serde would accept `name` for this field when deserializing:
    /// its serialized name (honoring rename/rename_all), its Rust name, or any
    /// `#[serde(alias = "...")]`.
    pub fn accepts_name(&self, name: &str, container_rename_all: Option<&str>) -> bool {
        self.serialized_name(container_rename_all) == name
            || self.name == name
            || self.aliases.iter().any(|a| a == name)
    }
}

/// Apply a serde `rename_all` convention to a field name.
/// Mirrors serde's conversions, which assume snake_case Rust field names.
pub fn rename_all_field(name: &str, convention: &str) -> String {
    match convention {
        "lowercase" | "snake_case" => name.to_string(),
        "UPPERCASE" | "SCREAMING_SNAKE_CASE" => name.to_ascii_uppercase(),
        "PascalCase" => name
            .split('_')
            .map(capitalize)
            .collect(),
        "camelCase" => {
            let pascal = rename_all_field(name, "PascalCase");
            uncapitalize(&pascal)
        }
        "kebab-case" => name.replace('_', "-"),
        "SCREAMING-KEBAB-CASE" => name.to_ascii_uppercase().replace('_', "-"),
        _ => name.to_string(),
    }
}

/// Apply a serde `rename_all` convention to an enum variant name.
/// Mirrors serde's conversions, which assume PascalCase Rust variant names.
pub fn rename_all_variant(name: &str, convention: &str) -> String {
    match convention {
        "lowercase" => name.to_ascii_lowercase(),
        "UPPERCASE" => name.to_ascii_uppercase(),
        "PascalCase" => name.to_string(),
        "camelCase" => uncapitalize(name),
        "snake_case" => pascal_to_snake(name),
        "SCREAMING_SNAKE_CASE" => pascal_to_snake(name).to_ascii_uppercase(),
        "kebab-case" => pascal_to_snake(name).replace('_', "-"),
        "SCREAMING-KEBAB-CASE" => pascal_to_snake(name)
            .to_ascii_uppercase()
            .replace('_', "-"),
        _ => name.to_string(),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn uncapitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn pascal_to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnumVariant {
    pub name: String,
    pub fields: Vec<FieldInfo>,
    pub docs: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// `#[serde(rename = "...")]` on the variant
    #[serde(default)]
    pub rename: Option<String>,
}

impl EnumVariant {
    /// The fields serde accepts for this variant as `(serialized_name, field)`
    /// pairs, with `skip` fields excluded.
    pub fn effective_fields(&self) -> Vec<(String, FieldInfo)> {
        self.fields
            .iter()
            .filter(|f| !f.skip)
            .map(|f| (f.serialized_name(None), f.clone()))
            .collect()
    }

    /// The name serde expects for this variant, honoring `#[serde(rename)]`
    /// and the enum's `#[serde(rename_all)]` convention.
    pub fn serialized_name(&self, container_rename_all: Option<&str>) -> String {
        if let Some(rename) = &self.rename {
            return rename.clone();
        }
        if let Some(convention) = container_rename_all {
            return rename_all_variant(&self.name, convention);
        }
        self.name.clone()
    }

    /// Whether a document naming `name` is naming this variant: its serialized
    /// name (honoring `rename`/`rename_all`) or its Rust name, the latter also
    /// matched case-insensitively.
    ///
    /// The case-insensitive fallback is deliberately lenient. A `rename_all`
    /// convention the analyzer failed to record would otherwise turn every
    /// variant in a correct document into an "unknown variant" error, and a
    /// false error is worse than a missed one. This is the one place that
    /// decides the question, so every feature answers it the same way.
    pub fn accepts_name(&self, name: &str, container_rename_all: Option<&str>) -> bool {
        self.serialized_name(container_rename_all) == name
            || self.name == name
            || self.name.eq_ignore_ascii_case(name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TypeKind {
    Struct(Vec<FieldInfo>),
    Enum(Vec<EnumVariant>),
}

impl Default for TypeKind {
    fn default() -> Self {
        TypeKind::Struct(Vec::new())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypeInfo {
    pub name: String,
    pub kind: TypeKind,
    pub docs: Option<String>,
    pub source_file: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// Container-level `#[serde(default)]`: serde fills in every absent field,
    /// so none of them are required. A bare `#[derive(Default)]` does not set
    /// this — it has no bearing on what serde will accept.
    pub has_default: bool,
    /// `#[serde(rename_all = "...")]` on the container (e.g. "camelCase")
    #[serde(default)]
    pub rename_all: Option<String>,
}

impl TypeInfo {
    pub fn fields(&self) -> Option<&Vec<FieldInfo>> {
        match &self.kind {
            TypeKind::Struct(fields) => Some(fields),
            TypeKind::Enum(_) => None,
        }
    }

    pub fn find_field(&self, field_name: &str) -> Option<&FieldInfo> {
        match &self.kind {
            TypeKind::Struct(fields) => fields.iter().find(|f| f.name == field_name),
            TypeKind::Enum(variants) => {
                // Search through all variants' fields
                variants
                    .iter()
                    .flat_map(|v| &v.fields)
                    .find(|f| f.name == field_name)
            }
        }
    }

    /// Find the variant that a document naming `variant_name` is referring to,
    /// honoring serde's `rename`/`rename_all` — see
    /// [`EnumVariant::accepts_name`] for exactly which names are accepted.
    /// `None` for a struct.
    pub fn find_variant(&self, variant_name: &str) -> Option<&EnumVariant> {
        let TypeKind::Enum(variants) = &self.kind else {
            return None;
        };
        let rename_all = self.rename_all.as_deref();
        variants
            .iter()
            .find(|v| v.accepts_name(variant_name, rename_all))
    }

    /// Find a field by the name serde expects in the serialized form
    /// (honoring rename/rename_all), falling back to the Rust field name.
    pub fn find_field_serialized(&self, field_name: &str) -> Option<&FieldInfo> {
        let rename_all = self.rename_all.as_deref();
        match &self.kind {
            TypeKind::Struct(fields) => fields
                .iter()
                .find(|f| f.accepts_name(field_name, rename_all)),
            TypeKind::Enum(variants) => variants
                .iter()
                .flat_map(|v| &v.fields)
                .find(|f| f.accepts_name(field_name, None)),
        }
    }

    /// The fields serde accepts for this struct: `skip` fields are excluded,
    /// `flatten` fields are expanded into the flattened type's fields
    /// (recursively, depth-limited), and names are serialized names.
    pub fn effective_fields(&self, analyzer: &RustAnalyzer) -> Vec<(String, FieldInfo)> {
        self.effective_fields_depth(analyzer, 8)
    }

    fn effective_fields_depth(
        &self,
        analyzer: &RustAnalyzer,
        depth: usize,
    ) -> Vec<(String, FieldInfo)> {
        let mut out = Vec::new();
        let Some(fields) = self.fields() else {
            return out;
        };
        for field in fields {
            if field.skip {
                continue;
            }
            if field.flatten {
                if depth > 0
                    && let Some(inner) = analyzer.get_type_info(&field.type_name)
                {
                    out.extend(inner.effective_fields_depth(analyzer, depth - 1));
                }
                continue;
            }
            out.push((
                field.serialized_name(self.rename_all.as_deref()),
                field.clone(),
            ));
        }
        out
    }

    /// Whether this struct has a `flatten` field whose target type cannot be
    /// resolved (e.g. a HashMap). Serde then accepts arbitrary extra keys, so
    /// unknown-field checks should be suppressed.
    pub fn has_unresolved_flatten(&self, analyzer: &RustAnalyzer) -> bool {
        self.fields().is_some_and(|fields| {
            fields
                .iter()
                .any(|f| f.flatten && analyzer.get_type_info(&f.type_name).is_none())
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustAnalyzer {
    pub root_type: Option<String>,
    type_cache: HashMap<String, TypeInfo>,
    type_aliases: HashMap<String, String>,
}

impl Default for RustAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl RustAnalyzer {
    /// Create a new RustAnalyzer without a root type.
    pub fn new() -> Self {
        Self {
            root_type: None,
            type_cache: HashMap::new(),
            type_aliases: HashMap::new(),
        }
    }

    /// Create a new RustAnalyzer with the given root type path (e.g., "crate::models::Config")
    pub fn with_root_type(root_type: impl Into<String>) -> Self {
        Self {
            root_type: Some(root_type.into()),
            type_cache: HashMap::new(),
            type_aliases: HashMap::new(),
        }
    }

    /// Get the TypeInfo for the root type, if one is set.
    pub fn root_type_info(&self) -> Option<&TypeInfo> {
        self.root_type
            .as_deref()
            .and_then(|t| self.get_type_info(t))
    }

    /// Register a type directly with the analyzer.
    ///
    /// This is useful when you have pre-constructed TypeInfo objects.
    pub fn add_type(&mut self, type_info: TypeInfo) {
        self.type_cache.insert(type_info.name.clone(), type_info);
    }

    /// Register a type alias.
    ///
    /// # Arguments
    /// * `alias` - The alias name (e.g., "crate::MyAlias")
    /// * `target` - The target type (e.g., "crate::SomeType")
    pub fn add_type_alias(&mut self, alias: &str, target: &str) {
        self.type_aliases
            .insert(alias.to_string(), target.to_string());
    }

    /// Remove a type from the analyzer.
    ///
    /// # Returns
    /// The removed TypeInfo if it existed
    pub fn remove_type(&mut self, type_path: &str) -> Option<TypeInfo> {
        self.type_cache.remove(type_path)
    }

    /// Clear all types and aliases from the analyzer.
    pub fn clear(&mut self) {
        self.type_cache.clear();
        self.type_aliases.clear();
    }

    pub fn get_type_info(&self, type_path: &str) -> Option<&TypeInfo> {
        // Strip whitespace from the lookup key. Type names extracted from `syn`
        // via `quote!(#ty).to_string()` are tokenized with spaces around `::`
        // and `<>`, but cache keys are stored space-free. Without this every
        // call site would have to remember to pre-normalize.
        let normalized = type_path.replace(' ', "");
        let lookup = normalized.as_str();

        // Resolve type aliases first
        let resolved_type = self
            .type_aliases
            .get(lookup)
            .map(|s| s.as_str())
            .unwrap_or(lookup);

        // Check cache with exact match
        if let Some(info) = self.type_cache.get(resolved_type) {
            return Some(info);
        }
        // Also try the original (normalized) type path
        if let Some(info) = self.type_cache.get(lookup) {
            return Some(info);
        }

        // If not found by exact match, try finding by simple name
        // e.g., "PostType" should match "crate::models::PostType"
        let lookup_suffix = format!("::{}", lookup);
        let resolved_suffix = format!("::{}", resolved_type);
        for (key, value) in self.type_cache.iter() {
            if key.ends_with(&lookup_suffix) || key.ends_with(&resolved_suffix) {
                return Some(value);
            }
        }

        None
    }

    /// Get all types registered with the analyzer
    pub fn get_all_types(&self) -> Vec<&TypeInfo> {
        self.type_cache.values().collect()
    }

    /// Get the number of types registered with the analyzer
    pub fn type_count(&self) -> usize {
        self.type_cache.len()
    }

    /// Check if a type exists in the analyzer
    pub fn has_type(&self, type_path: &str) -> bool {
        self.get_type_info(type_path).is_some()
    }

    /// Validate that all field types referenced in registered types are known.
    ///
    /// This checks that every struct field and enum variant field references
    /// a type that is either:
    /// - A primitive type (bool, i32, String, etc.)
    /// - A standard library generic (Option, Vec, HashMap, etc.)
    /// - A type registered with this analyzer
    ///
    /// # Returns
    /// A list of (type_name, field_name, unknown_type) tuples for any unknown types found.
    pub fn validate_field_types(&self) -> Vec<(String, String, String)> {
        let mut errors = Vec::new();

        for type_info in self.type_cache.values() {
            let fields: Vec<&FieldInfo> = match &type_info.kind {
                TypeKind::Struct(fields) => fields.iter().collect(),
                TypeKind::Enum(variants) => variants.iter().flat_map(|v| &v.fields).collect(),
            };

            for field in fields {
                if let Some(unknown) = self.check_field_type_known(&field.type_name) {
                    errors.push((type_info.name.clone(), field.name.clone(), unknown));
                }
            }
        }

        errors
    }

    /// Check if a field type is known, returning the unknown type name if not.
    fn check_field_type_known(&self, type_name: &str) -> Option<String> {
        let clean = type_name.replace(" ", "");

        // Primitive types are always known
        let primitives = [
            "bool", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128",
            "usize", "f32", "f64", "char", "String", "&str", "str", "()",
        ];
        if primitives.contains(&clean.as_str()) {
            return None;
        }

        // Check generic wrappers and recurse into inner type
        let wrappers = ["Option<", "Vec<", "Box<", "Rc<", "Arc<"];
        for wrapper in wrappers {
            if clean.starts_with(wrapper) && clean.ends_with('>') {
                let inner = &clean[wrapper.len()..clean.len() - 1];
                return self.check_field_type_known(inner);
            }
        }

        // Other std generic types (we don't recurse into these for now)
        if clean.contains("HashMap<")
            || clean.contains("BTreeMap<")
            || clean.contains("HashSet<")
            || clean.contains("BTreeSet<")
            || clean.starts_with("Result<")
        {
            return None;
        }

        // Check if it's a known custom type
        if self.get_type_info(&clean).is_some() {
            return None;
        }

        // Unknown type
        Some(clean)
    }
}

#[cfg(feature = "analyze")]
mod analyze;
