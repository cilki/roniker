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

let mut analyzer = roniker::RustAnalyzer::with_root_type("Configuration");
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

The type paths come from the file path as well: `add_file` takes the **first**
path component named `src` as the crate root and builds the module path out of
what follows it, dropping a trailing `lib.rs`, `main.rs` or `mod.rs` the way
Rust itself does.

- `src/config.rs` and `src/config/mod.rs` both register the struct of step 0 as
  `crate::config::Configuration`.
- `src/lib.rs` and `src/main.rs` register under `crate::`, so a config type that
  lives next to `fn main` is `crate::Configuration`, not
  `crate::main::Configuration`.
- A path with no `src` component at all gets no prefix, and its types are
  registered under their bare names (`Configuration`).

And "first component named `src`" is not always the crate's own: the absolute
path you were just told to pass is equally happy to match a `src` directory
*above* the crate, so the same `src/config.rs` in a crate at `/home/you/src/app`
registers `crate::app::src::config::Configuration`.

None of which the build script above has to work out, which is why it asks for
`Configuration` and not for a full path: a root type is looked up by any
`::`-delimited tail of a registered path, so the bare type name finds all four
of those spellings. Spell out as much of the path as it takes to be unambiguous
— two registered types sharing a final segment resolve to whichever one was
registered first — and no more. Both examples in this repository name their root
type that way.

A root type that resolves to nothing is not fatal on its own: the server falls
back to the type named at the top of the document, so a file written
`Configuration(...)` still gets everything. It's a file whose root value leaves
its name out — `(ephemeral: true)` — that has nothing left to resolve, and then
every type-directed feature goes quiet: no completions, no hover, no
go-to-definition, no rename, no code actions, and no diagnostics at all, not
even the syntax check. Formatting and document symbols carry on working, which
is what makes it look like the server is healthy.

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
- hover documentation pulled from the doc comments on your structs, and
  go-to-definition back to the Rust source — but a *field* resolves only under
  its Rust identifier. A field that serde renames (`rename`, `rename_all`, or a
  name reached through `alias`) answers neither request, even though completions
  and diagnostics accept exactly that spelling, so a document that the server
  itself reports as correct has fields with no hover and no definition until
  [#76](https://git.cilki.net/cilki/roniker/issues/76) is fixed. Enum variants
  are matched through serde and are not affected.
- document symbols, rename, and formatting — what the formatter lays out, and
  what it throws away, is spelled out under [Formatting](#formatting) below
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
    Lastly it is the three ways a tuple or fixed-size array field can go wrong,
    for which see [Tuples and arrays](#tuples-and-arrays) below: the bracket
    spelling (`[u32; 3] is written as a tuple, not an array`), the wrong number
    of elements (`expected 3 elements for [u32; 3], got 2`), and an element
    that doesn't fit the type its position declares, reported on that element.
  - `unknown-type` - a field's declared type was never registered with the
    analyzer, which usually means the build script is missing a source file.
    This is about the *declaration*; a value naming a type nobody registered is
    a `type-mismatch`, as above. A standard-library type is never reported here
    — see [Standard-library types](#standard-library-types) below for the ones
    that are understood without being registered.
- code actions:
  - *Add N required fields* and *Add all N missing fields* on the root value,
    for structs and for enum variants alike. An enum root's titles name the
    variant the fields belong to: *Add all 2 missing fields to Prod*. Each
    field is filled in with a value `ron` will read, which for the types of
    [Standard-library types](#standard-library-types) is the spelling listed
    there: `""` for a `PathBuf`, `1` for a `NonZeroU32`,
    `(secs: 0, nanos: 0)` for a `Duration`
  - *Add N required fields to `path`* for any value nested below it that the
    cursor (or selection) is in — a struct, an enum variant, an element of a
    `Vec`, or the contents of a `Some(...)` — naming it the way the file spells
    it (`server.tls`, `mode::Prod`, `certs[1]`). The fields go into that value's
    own parentheses. Only the required-field form is offered here; there is no
    *Add all* counterpart below the root
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
`Vec` of structs, enums or tuples is walked element by element, so each bad
element is marked where it is written; a `Vec` of primitives is checked as a
whole, and the one message lands on the array (`in Vec: expected u16, got
string`). Either way a non-array written where a `Vec` belongs is reported as
the mismatch it is (`expected Vec<String>, got number`).

### Standard-library types

A configuration type is mostly made of standard-library types, and the analyzer
only ever reads your own source: nothing in `std` is defined in the workspace it
is given, so no build script can register a `PathBuf`. These are understood by
name instead, by the RON spelling serde reads each one from:

- written as a string: `PathBuf`, `Path`, `OsString`, `OsStr`, `IpAddr`,
  `Ipv4Addr`, `Ipv6Addr`, `SocketAddr`, `SocketAddrV4`, `SocketAddrV6`
- written as an integer: the `NonZero` family, under both the `NonZeroU32` and
  the `NonZero<u32>` spelling. A `0` in an *unsigned* one is reported as out of
  range, which is what `ron` does with it; a `0` in a signed one is refused by
  `ron` too but is not reported
- written as a struct of their own: `Duration` as `(secs: 0, nanos: 0)` and
  `SystemTime` as `(secs_since_epoch: 0, nanos_since_epoch: 0)`. The fields
  inside those are not checked
- written as an array of their elements: `VecDeque<T>`, `LinkedList<T>`,
  `BinaryHeap<T>`, alongside the `Vec<T>`, `HashSet<T>` and `BTreeSet<T>`
  already handled
- written as the wrapped value and nothing else: `Cow<T>`, `Cell<T>`,
  `RefCell<T>`, `Mutex<T>`, `RwLock<T>`, alongside `Box<T>`, `Rc<T>` and
  `Arc<T>`

Each is accepted wherever a type can appear — on its own, behind an `Option`,
as a `Vec`'s element, in a position of a tuple — and the module path in front
of one is dropped, so `std::path::PathBuf` is the `PathBuf` it names. Lifetime
arguments are dropped too, since a document never writes one: a
`Cow<'static, str>` field takes the same values a `String` field does.

A type of **your own** that shares a name with one of these keeps its own
meaning: the analyzer is asked first, so a `Duration` your crate defines is
validated against your definition, completed from it, and filled in by the
quick-fix as `Duration()`.

Anything else in `std` is still read as a type the build script should have
registered, and a field declared as one gets `unknown-type`. `Result<T, E>`,
`PhantomData<T>` and the atomics are the ones most likely to come up.

### Tuples and arrays

RON writes a Rust tuple and a fixed-size array **both** as a parenthesized
tuple. Brackets belong to the types whose length the value decides — `Vec`,
slices, `HashSet`, `BTreeSet` — and `ron` refuses them for the other two, so a
`(String, u16)` field is written `("a", 2)` and a `[u32; 3]` field is written
`(1, 2, 3)`, not `[1, 2, 3]`.

The server follows `ron` here. A field declared as either gets its arity and
each of its positions checked against the type that position declares, and a
struct or enum sitting in one of those positions is validated like any other
value. Completion offers the tuple spelling with one tab stop per position, and
the *Add N required fields* quick-fix fills the positions in from their types
(`pair: ("", 0)`, `triple: (0, 0, 0)`).

A composite with no arity for the type name to fix — a slice `[T]`, or an array
whose length is a const generic — is still written as a tuple, but nothing
checks how many elements it has.

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

Both are [#103](https://git.cilki.net/cilki/roniker/issues/103).

A document that doesn't parse — a half-typed `timeout: `, a field name whose
colon isn't typed yet, an unclosed parenthesis, or one that is still only a
comment — is left exactly as it is, so
formatting on save never rewrites a file you are in the middle of editing. Note
that this includes an empty struct written with space inside it, `( )`: the
grammar only knows `()`, so a document holding one is not formatted.

Range formatting ("format selection") is advertised by the server but only
does anything when the selection is exactly one whole value, such as a single
nested struct from its name to its closing parenthesis — and then it lays that
value out as though it were the root, so a nested one loses its indentation.
Any other selection — a few fields, a few elements of an array — doesn't parse
on its own and is left unchanged
([#67](https://git.cilki.net/cilki/roniker/issues/67)). Format the whole
document instead.

### Serde attributes

Names come from serde's view of your types rather than from the Rust
identifiers, so completions, diagnostics and code actions use exactly the names
your application will deserialize:

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

The bullet list above is the whole of what the analyzer reads, which makes the
promise opening this section narrower than it sounds. Four serde attributes
change the names or the shape a config file has to use, and none of them reach
the LSP — on three of them a file your application deserializes without
complaint is reported as broken. Avoid them in config types until
[#72](https://git.cilki.net/cilki/roniker/issues/72) is fixed:

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

### Completions

What gets offered depends on where in the value the cursor sits:

- In a struct's parentheses: that struct's fields, labeled with the name serde
  expects, inserted as `name: `, and documented with the field's declared type
  and doc comment. Where an enum is expected the variants are offered instead,
  and inside a variant's own parentheses, that variant's fields.
- After a field's colon: values derived from the type the field is declared as.
  `true`/`false` for a `bool`, `""` for a `String`, `''` for a `char`, `0` for
  any integer or float, `[]` and `[...]` for a `Vec`, a slice or a set, `(...)`
  for a tuple or a fixed-size array (see [Tuples and
  arrays](#tuples-and-arrays)),   `{}` and `{...}` for a `HashMap` or
  `BTreeMap`, `Some(...)` and `None` for an `Option`, the variants of an enum,
  and for a struct a snippet of the whole value with every field laid out. The
  standard-library types of [Standard-library
  types](#standard-library-types) are offered the value `ron` reads them from,
  with the caret where the value goes: `""` for a `PathBuf`, `"0.0.0.0:0"` for
  a `SocketAddr`, `(secs: 0, nanos: 0)` for a `Duration`.
- Every registered type, as a fallback, when the field's declared type yields
  none of the above — anything the build script was not given the source for.

Fields the value has already written are left out of the field-name
completions. That is the value the cursor is in, at whatever depth: inside a
nested `DatabaseConfig(...)` it is that value's `port` that counts, not the root
value's.

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
