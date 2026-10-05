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

[build-dependencies]
roniker = { version = "0.4", features = ["analyze"] }
```

### Step 2: build script

The build script reads your config structs and turns them into LSP state that
can be serialized and embedded into your application:

```rs
let mut analyzer = roniker::RustAnalyzer::with_root_type("crate::config::Configuration");
analyzer.add_file(Path::new("src/config.rs"))?;

let json = serde_json::to_string(&analyzer)?;
let dest = PathBuf::from(std::env::var("OUT_DIR")?).join("rust_analyzer.json");
std::fs::write(&dest, json)?;

println!("cargo:rerun-if-changed=src/config.rs");
```

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
(`ephemeral: bool`) attached to every field whose value doesn't already name its
type; pass `false` to publish only real problems.

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
- diagnostics for unknown fields, missing required fields, and invalid enum
  variants, with code actions to insert the fields that are missing
- go-to-definition back to the Rust source, document symbols, rename, and
  formatting

### Runnable examples

Two examples in this repository do the same thing end to end, if you'd rather
read working code:

```sh
# Builds its types by parsing examples/data/config_types.rs
cargo run --example analyze_lsp --features "analyze,lsp"

# Builds the same kind of types by hand, without the analyze feature
cargo run --example simple_lsp --features "lsp"
```

`examples/data/example.ron` is a configuration file for the first one to open.
