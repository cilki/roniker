# roniker

This is a library that builds custom LSPs for applications configured with
[RON](https://github.com/ron-rs/ron). Good LSP support can make configuring your
application significantly easier.

You point roniker at the Rust structs your application already deserializes its
config into, and it serves a language server that knows them. Opening a config
file in your editor then gets you:

- completions for field names, enum variants, values, and nested struct types
- hover documentation pulled from the doc comments on your structs, and (in
  debug builds) go-to-definition back to the Rust source
- diagnostics for syntax errors, unknown or duplicate fields, missing required
  fields, unknown enum variants, and type mismatches, each with a stable code
  your editor can filter on
- quick-fixes that fill in missing fields, fix misspelled names, and remove
  unknown fields
- document symbols, rename, and a formatter that keeps your comments and blank
  lines

Common standard-library types like `PathBuf`, `SocketAddr`, `Duration` and the
`NonZero` integers work without any extra setup.

## Getting started

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

You should only include the crate with exactly one of these features:

- `analyze` - read type definitions out of Rust source files. Needed by the
  build script.
- `lsp` - serve the language server. Needed by your application.

```toml
[dependencies]
# This is what serves the LSP
roniker = { version = "0.4", features = ["lsp"] }
serde_json = "1"

[build-dependencies]
# This is what analyzes the config structs
roniker = { version = "0.4", features = ["analyze"] }
serde_json = "1"
```

### Step 2: build script

The build script reads your config structs and turns them into LSP state that
can be embedded into your application:

```rs
let config = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("src/config.rs");

let mut analyzer = roniker::RustAnalyzer::with_root_type("Configuration");
analyzer.add_file(&config)?;

let json = serde_json::to_string(&analyzer)?;
let dest = PathBuf::from(std::env::var("OUT_DIR")?).join("rust_analyzer.json");
std::fs::write(&dest, json)?;

println!("cargo:rerun-if-changed=src/config.rs");
```

Give `add_file` an **absolute** path, or go-to-definition won't work.

> Go-to-definition is only available in debug builds. A release build doesn't
> record source paths at all, so your machine's absolute paths never end up in
> the shipped binary.

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

The second argument to `serve` turns on inline type annotations for the root
value's fields. Pass `false` to publish only real problems.

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

## Runnable examples

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

Open `examples/data/example.ron` with `analyze_lsp`, or
`examples/data/simple.ron` with `simple_lsp`.
