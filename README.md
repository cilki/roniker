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
(`ephemeral: bool`) attached to the fields of the root value, and to those
alone: a nested struct's fields never get one, and neither do the fields of a
root value that is an enum variant. Within the root value, a field is annotated
only when its value names nothing itself — which excludes a named struct
(`server: ServerConfig(...)`) and a bare name such as an enum variant
(`log_level: Info`). Pass `false` to publish only real problems.

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

`auto-format = true` hands the whole file to the server's formatter on every
save. Read [Formatting](#formatting) below before you turn it on.

### Now open a file

Opening a RON file matched by the pattern above gets you:

- completions for field names, enum variants, values, and nested struct types,
  spelled out under [Completions](#completions) below
- hover documentation pulled from the doc comments on your structs
- go-to-definition back to the Rust source, document symbols, rename, and
  formatting — what the formatter lays out, and what it throws away, is spelled
  out under [Formatting](#formatting) below
- diagnostics, each error tagged with a stable code so your editor can filter
  them:
  - `syntax-error` - the file doesn't parse as RON
  - `unknown-field` - the struct has no such field
  - `duplicate-field` - the same field is given twice, reported at the second
    occurrence
  - `missing-required-field` - a field with no default was left out. One
    diagnostic per struct, listing every field that struct is missing, reported
    on the struct's name — or at the value's first character when the struct is
    written without one
  - `unknown-variant` - the value names something the enum the field expects has
    no variant for
  - `type-mismatch` - the value doesn't fit the type the field is declared as.
    That is a primitive of the wrong shape, e.g. a string where a `u16` is
    expected, or an integer the field's type can't hold (`port: -1` for a
    `u16`), which `ron` refuses to deserialize. An integer literal in a float
    field is fine — serde widens it — and is not reported. It is also every way
    a value can contradict a struct or enum it is written for: naming a
    different type (`expected ServerConfig, got ServerMode`), naming a type
    nothing in the workspace defines (`unknown type 'Bogus'`), a literal where
    a struct belongs (`expected ServerConfig, got integer`), a value that names
    no variant at all in an enum field (`unknown variant '42' for enum
    ServerMode`), or data given to a unit variant. Those last two matter if you
    filter by code: a message reading *unknown type* or *unknown variant* is
    still tagged `type-mismatch`, not `unknown-type` or `unknown-variant`.
  - `unknown-type` - a field's declared type was never registered with the
    analyzer, which usually means the build script is missing a source file.
    This is about the *declaration*; a value naming a type nobody registered is
    a `type-mismatch`, as above.
- code actions:
  - *Add N required fields* and *Add all N missing fields* on the root value,
    for structs and for enum variants alike. An enum root's titles name the
    variant the fields belong to: *Add all 2 missing fields to Prod*
  - *Add N required fields to `path`* for any value nested below it — a struct,
    an enum variant, an element of a `Vec`, or the contents of a `Some(...)` —
    naming it the way the file spells it (`server.tls`, `mode::Prod`,
    `certs[1]`). The fields go into that value's own parentheses. Only the
    required-field form is offered here; there is no *Add all* counterpart below
    the root
  - *Remove field '...'*, offered as a quick-fix on `unknown-field` and
    `duplicate-field`, at whatever depth the diagnostic was reported
  - *Make struct name explicit: Configuration* and *Make field type explicit:
    server ServerConfig*, which turn `server: (host: "localhost")` into
    `server: ServerConfig(host: "localhost")`. Each title spells out what it
    will insert. Unlike the missing-field actions these two don't descend: they
    reach the root value and its own fields only, so an unnamed struct nested
    deeper gets neither, and a document whose root value is an enum variant gets
    neither at all

Only the errors carry a code. The informational type annotations of step 3 are
published as diagnostics too and carry none, so their `Information` severity is
all that tells them apart.

Where a mismatch inside a `Vec` is reported depends on the element type. A
`Vec` of structs or enums is walked element by element, so each bad element is
marked where it is written; a `Vec` of primitives is checked as a whole, and the
one message lands on the array (`in Vec: expected u16, got string`). Either way
a non-array written where a `Vec` belongs is reported as the mismatch it is
(`expected Vec<String>, got number`).

### Formatting

Formatting the whole document lays the root value out again from the parse tree:
four spaces per level of nesting, one field, element or entry per line, a
trailing comma after each, `: ` after every field name, and `()`, `[]` and `{}`
kept closed up when they're empty. A tuple struct holding a single value stays
on one line (`max_connections: Some(16)`). Comments keep their place — above the
field they describe, or on the same line as it, or at the top of the file — and
laying out a document that is already in this shape changes nothing.

Two things are not laid out again but **deleted**, which is why the Helix
snippet's `auto-format = true` is worth a second thought if your config files
hold either:

- RON's extension header: `#![enable(implicit_some)]` and friends. It isn't a
  comment, and the formatter only writes out the root value and the comments
  above it, so it goes. With `implicit_some` gone every `Option` written as a
  bare value stops deserializing, and nothing in the editor says so — the
  server accepts that spelling whether the extension is enabled or not.
- Any comment *after* the root value, such as a note at the end of the file.

A document holding no value at all — one that is still only a comment, say — is
emptied rather than left alone. All three are
[#103](https://git.cilki.net/cilki/roniker/issues/103).

Range formatting ("format selection") is advertised by the server but does not
work: it replaces the selection with a shorter string, dropping everything after
the first value in it, which usually leaves the file unparseable
([#67](https://git.cilki.net/cilki/roniker/issues/67)). Format the whole
document instead.

### Serde attributes

Names come from serde's view of your types rather than from the Rust
identifiers, so the LSP accepts exactly what your application will deserialize:

- `#[serde(rename = "...")]` and `#[serde(rename_all = "...")]` decide the name
  completions insert and diagnostics expect. The `rename_all` conventions that
  produce a plain RON identifier are handled: `lowercase`, `UPPERCASE`,
  `PascalCase`, `camelCase`, `snake_case`, and `SCREAMING_SNAKE_CASE`. The
  `rename(deserialize = "...")` form is read too, since that's the direction a
  config file travels.
- `#[serde(alias = "...")]` names are accepted alongside the primary one — on a
  struct field. An alias on an *enum variant* is dropped by the analyzer, so the
  `mode: production` serde accepts for an aliased `Prod` is reported as
  `unknown-variant` and completion never offers it.
- `#[serde(skip)]` and `#[serde(skip_deserializing)]` fields are left out of
  completions and reported as unknown if written. That holds for a struct's
  fields and for a variant's fields; on the *variant itself* the two are
  ignored, so a skipped variant is still completed and still accepted.
- `#[serde(flatten)]` expands the inner struct's fields into the outer one. When
  the flattened type can't be resolved - a `HashMap`, say - unknown-field
  reporting is switched off for that struct, because serde would accept any
  extra key there. Neither applies inside an enum's struct variant, whose fields
  are never flattened: the inner names serde requires come back as
  `unknown-field`, the flattened field itself as `missing-required-field`, and
  the offered *Add 1 required field* inserts that field name — which serde will
  reject. Keep `flatten` out of struct variants until
  [#90](https://git.cilki.net/cilki/roniker/issues/90) is fixed.
- A field stops counting as required if it is an `Option<T>`, carries
  `#[serde(default)]` or `#[serde(default = "path")]`, or its container carries a
  container-level `#[serde(default)]`. A bare `#[derive(Default)]` is *not*
  enough: it decides what `Default::default()` builds, not what serde will
  accept, so a field with no default of its own is still required and leaving it
  out is still reported.

Names containing a hyphen are **not** supported: that means
`rename_all = "kebab-case"`, `rename_all = "SCREAMING-KEBAB-CASE"`, and any
explicit `rename` to a hyphenated name, on fields and on enum variants alike.
RON can only write such a name as a raw identifier (`r#max-size: 3`), which this
server's grammar doesn't recognise, so a correct file comes back with the name
chopped at the hyphen — `unknown-field 'size'` plus
`missing-required-field 'max-size'` — and completions insert the bare
`max-size: `, which doesn't parse at all. Rename those fields to something
RON can spell until [#68](https://git.cilki.net/cilki/roniker/issues/68) is
fixed.

### Completions

What gets offered depends on where in the value the cursor sits:

- In a struct's parentheses: that struct's fields, labeled with the name serde
  expects, inserted as `name: `, and documented with the field's declared type
  and doc comment. Where an enum is expected the variants are offered instead,
  and inside a variant's own parentheses, that variant's fields.
- After a field's colon: values derived from the type the field is declared as.
  `true`/`false` for a `bool`, `""` for a `String`, `0` for any integer or
  float, `[]` and `[...]` for a `Vec` or array, `{}` and `{...}` for a
  `HashMap` or `BTreeMap`, `Some(...)` and `None` for an `Option`, the variants
  of an enum, and for a struct a snippet of the whole value with every field
  laid out.
- Every registered type, as a fallback, when the field's declared type yields
  none of the above — a `PathBuf` field, say, or anything else the build script
  was not given the source for.

Fields the value has already written are left out of the field-name
completions, with one caveat: the names consulted are always the *root* value's,
whichever value the cursor is actually in. So inside a nested value, a field
whose name the root value also uses is never offered — `DatabaseConfig`'s
`port` goes missing as soon as the root value has a `port` of its own — and a
field already written there is offered a second time. See
[#101](https://git.cilki.net/cilki/roniker/issues/101).

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
