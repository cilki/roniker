# roniker

This is a library that builds custom LSPs for applications configured with
[RON](https://github.com/ron-rs/ron). Good LSP support can make configuring your
application significantly easier.

### Step 0: Create your configuration structs

Chances are, your application already has these:

```rs
// config.rs

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Configuration {
  /// Run without a database.
  ephemeral: bool,
}
```

### Step 1: add the dependency

`roniker` splits its functionality across two feature flags, and neither is
enabled by default:

- `analyze` - read type definitions out of Rust source files. Needed by the
  build script.
- `lsp` - serve the language server. Needed by your application.

Since the build script and the application need different halves, `roniker`
appears twice:

```toml
[dependencies]
roniker = { version = "0.4", features = ["lsp"] }
serde_json = "1"

[build-dependencies]
roniker = { version = "0.4", features = ["analyze"] }
serde_json = "1"
```

`RustAnalyzer` is carried from the build script to the application as JSON, so
both halves need a serializer. Any `serde` format works; the steps below use
`serde_json`.

### Step 2: build script

The build script reads your config structs and turns them into LSP state that
can be serialized and embedded into your application:

```rs
let config = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("src/config.rs");

let mut analyzer = roniker::RustAnalyzer::with_root_type("crate::config::Configuration");
analyzer.add_file(&config)?;

let json = serde_json::to_string(&analyzer)?;
let dest = PathBuf::from(std::env::var("OUT_DIR")?).join("rust_analyzer.json");
std::fs::write(&dest, json)?;

println!("cargo:rerun-if-changed=src/config.rs");
```

Give `add_file` an **absolute** path. Each path is recorded on the types it
yielded and is what go-to-definition jumps to later; a relative path can't be
turned into a `file://` URL, so `Path::new("src/config.rs")` analyzes your types
perfectly well and then leaves every go-to-definition request unanswered.

The type paths come from the file path as well: `add_file` treats the first
`src` component as the crate root and builds the module path from what follows
it, so types in `src/config.rs` are registered under `crate::config::`
regardless of where the crate sits on disk — which is why
`with_root_type("crate::config::Configuration")` matches above. A file with no
`src` component in its path gets no prefix at all and its types are registered
under their bare names (`Configuration`), in which case a root type of
`crate::config::Configuration` resolves to nothing and the server starts up but
answers nothing.

### Step 3: serve LSP

Now you just need to dedicate a subcommand of your application to running the
LSP:

```rs
#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
  Lsp,
}

pub async fn run_lsp() -> Result<()> {

    let rust_analyzer: RustAnalyzer = serde_json::from_str(include_str!(concat!(
        env!("OUT_DIR"),
        "/rust_analyzer.json"
    )))?;

    roniker::serve(rust_analyzer, true).await;
    Ok(())
}
```

The second argument to `serve` decides whether informational diagnostics are
published alongside the warnings and errors. These are inline type annotations
(`ephemeral: bool`) attached to the fields of the root value — only those whose
value doesn't already name its type, and only at the top level, so fields of
nested structs don't get one. Pass `false` to publish only real problems.

Now you should be able to run `<app> lsp` and it will start reading stdin and
writing LSP messages to stdout.

### Step 4: configure editor

Lastly you need to configure your editor to use the `lsp` subcommand above.
There should be a clear pattern that selects the files you want the custom LSP
to run on.

#### Helix

```toml
[language-server]
custom-lsp = { command = "custom", args = ["lsp"] }

[[language]]
name = "ron"
auto-format = true
scope = "source.ron"
injection-regex = "ron"
file-types = ["ron", { glob = "custom.ron" }]
comment-token = "//"
block-comment-tokens = { start = "/*", end = "*/" }
indent = { tab-width = 4, unit = "    " }
roots = ["Cargo.toml"]
language-servers = ["custom-lsp"]
```

### Now open a file

Opening a RON file matched by the pattern above gets you:

- completions for field names, enum variants, and nested struct types
- hover documentation pulled from the doc comments on your structs
- go-to-definition back to the Rust source, document symbols, rename, and
  formatting (whole document and range)
- diagnostics, each tagged with a stable code so your editor can filter them:
  - `syntax-error` - the file doesn't parse as RON
  - `unknown-field` - the struct has no such field
  - `duplicate-field` - the same field is given twice
  - `missing-required-field` - a field with no default was left out
  - `unknown-variant` - no such variant on the enum the field expects
  - `type-mismatch` - a primitive of the wrong shape, e.g. a string where a
    `u16` is expected
  - `unknown-type` - a field's declared type was never registered with the
    analyzer, which usually means the build script is missing a source file
- code actions:
  - *Add N required fields* and *Add all N missing fields*, for structs and for
    enum variants
  - *Remove field '...'*, offered as a quick-fix on `unknown-field` and
    `duplicate-field`
  - *Make struct name explicit* and *Make field type explicit*, which turn
    `server: (host: "localhost")` into `server: ServerConfig(host: "localhost")`

### Serde attributes

Names come from serde's view of your types rather than from the Rust
identifiers, so the LSP accepts exactly what your application will deserialize:

- `#[serde(rename = "...")]` and `#[serde(rename_all = "...")]` decide the name
  completions insert and diagnostics expect. All of serde's cases are
  understood: `lowercase`, `UPPERCASE`, `PascalCase`, `camelCase`, `snake_case`,
  `SCREAMING_SNAKE_CASE`, `kebab-case`, and `SCREAMING-KEBAB-CASE`. The
  `rename(deserialize = "...")` form is read too, since that's the direction a
  config file travels.
- `#[serde(alias = "...")]` names are accepted alongside the primary one.
- `#[serde(skip)]` and `#[serde(skip_deserializing)]` fields are left out of
  completions and reported as unknown if written.
- `#[serde(flatten)]` expands the inner struct's fields into the outer one. When
  the flattened type can't be resolved - a `HashMap`, say - unknown-field
  reporting is switched off for that struct, because serde would accept any
  extra key there.
- A field stops counting as required if it is an `Option<T>`, carries
  `#[serde(default)]` or `#[serde(default = "path")]`, or its container derives
  `Default` or carries a container-level `#[serde(default)]`.

That list is the whole of what the analyzer reads, which makes the promise above
narrower than it sounds. Four serde attributes change the names or the shape a
config file has to use, and none of them reach the LSP — on three of them a file
your application deserializes without complaint is reported as broken. Avoid
them in config types until [#72](https://git.cilki.net/cilki/roniker/issues/72)
is fixed:

- `#[serde(rename_all_fields = "...")]` on an enum is ignored, so the fields of
  its struct variants are expected under their Rust names. With
  `rename_all_fields = "camelCase"` and a variant `Fast { max_retries: u32 }`,
  the `maxRetries: 3` that serde requires is reported as `unknown-field` plus
  `missing-required-field`, and the "did you mean" suggests `max_retries` —
  the one spelling serde rejects.
- `#[serde(untagged)]` enums are written as the variant's content alone, with no
  variant name. The LSP still expects a variant name, so the `port: 8080` serde
  wants gets `unknown-variant`, while the `port: Number(8080)` serde rejects
  passes.
- `#[serde(transparent)]` structs are written as their single field's value. The
  LSP still expects a struct, so the `limit: 10` serde wants gets
  `type-mismatch`, while the `limit: (value: 10)` serde rejects passes.
- `#[serde(tag = "...")]` and `#[serde(tag = "...", content = "...")]` enums
  carry their variant name as a map key rather than in front of the value, so
  `tag = "kind"` makes `backend: (kind: "Postgres", host: "h")` the correct
  spelling and `backend: Postgres(host: "h")` an error. Nothing is reported as
  broken here, but completion offers the bare variant name serde rejects, and
  the tagged map that serde accepts is not checked at all — neither its tag key
  nor any extra key inside it.

### Runnable examples

Two examples in this repository cover the two ways of getting types into the
analyzer, if you'd rather read working code. Each is a complete language server
speaking LSP over stdin/stdout, so point an editor at it rather than expecting
it to print anything:

```sh
# Root type AppConfig, parsed out of examples/data/config_types.rs
cargo run --example analyze_lsp --features "analyze,lsp"

# Root type Config, registered by hand without the analyze feature
cargo run --example simple_lsp --features "lsp"
```

They register different types, so each comes with its own file to open:
`examples/data/example.ron` for `analyze_lsp`, `examples/data/simple.ron` for
`simple_lsp`. Opening one of them against the other server reports its fields as
unknown.
