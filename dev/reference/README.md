# Comparing the lexers with the real ones

The lexers in `src/highlight` follow the lexers and readers of their
languages. The scripts here check that they agree. Each one runs the
real lexer over a corpus of real files or random texts, and compares
its tokens with tergi's. They need software that the dev image doesn't
have, such as LLVM or Racket, so they run in containers of their own
and aren't part of `cargo test`. The `Reference lexers` workflow in
`.github/workflows/reference.yml` runs them in CI when the lexers
change. It runs them with docker rather than podman, and keeps the
layers of the images in the Actions cache.

To run one, give its name to `run.sh`. You need podman, or docker
with `PODMAN=docker`.

```sh
dev/reference/run.sh python
```

The first run builds the images, which can take a few minutes. Each
image holds the real lexer and the files it's checked on, so later
runs reuse them and need no network. Each run copies those files and
writes tergi's tokens to `/tmp/tergi-reference/<name>`, or to
`$TERGI_REFERENCE_DIR/<name>`.
Checks that generate random texts read `CASES` for how many to make
and `SEED` to choose them, so `CASES=100000 dev/reference/run.sh c`
searches harder.

A check prints up to a few disagreements, with the file and line of
each, and a summary. It fails if there's any disagreement that it
doesn't list as known.

## The checks

Each name is a directory here. The defaults for `CASES` are small, so
that each check runs in a minute or two once its image is built.

- `c`: clang 19's raw lexer on random texts, each read as C23 and as
  C++23.
- `cmake`: CMake's own lexer, with its parser's steps, on random texts
  and the CMake files in CMake's source. Random `message()` calls run
  with `cmake -P` check escapes and variable references.
- `dockerfile`: the Dockerfile parser of BuildKit 0.35.1, and its
  `shell` package, on random texts and the Dockerfiles in BuildKit's
  source and in a few projects, such as the official Python, PHP and
  Node.js images.
- `javascript`: acorn on the files of a few popular npm packages.
- `llvm`: LLLexer from LLVM 23.1.6 on the `.ll` files in LLVM's
  tests.
- `python`: the tokenizer and parser of Python 3.15.2 on CPython's
  source and a few large packages.
- `racket`: Racket 9.3's reader and @-reader on random atoms, random
  texts, and the Racket, Scribble and at-exp files in Racket's source.
- `rust`: rustc's lexer, from the crate `ra-ap-rustc_lexer` 0.183.0,
  and its escapes, from `rustc-literal-escaper` 0.0.8, on random texts
  and the `.rs` files in Rust 1.100.0's source.
- `sh`: bash 5.3, patched to report where its tokens are, on random
  texts, bash's own tests and examples, and the shell scripts in git's
  source and bash-completion.
- `shrubbery`: the shrubbery lexer that DrRacket uses, on random texts
  and the Rhombus files in the Rhombus repository.
- `yaml`: saphyr-parser on the YAML test suite and the YAML files of a
  few large projects. Its property test in `src/highlight/yaml.rs`
  covers random documents.
- `zig`: the tokenizer from Zig 0.17.1 on Zig's own `lib` directory.

The JSON, Markdown, TOML and `.gitignore` lexers have property tests
in `cargo test` that check them against a model, a library or git, so
they don't need a check here.
`design/diffs.md` says what each check found when we wrote the
lexers, and which differences are known.

## How a check works

Each directory here is one check. It has a `Containerfile` for the
image with the real lexer, and two scripts that run in that image.
`run.sh` does four things:

1. It builds the dev image from `dev/Containerfile` and the check's
   image from its `Containerfile`.
2. It runs `prepare <work>`, which copies the corpus from the image
   into the work directory, or generates the random texts there. The
   dev image can't see inside the check's image, so the files have to
   be in the work directory. It writes their paths to
   `<work>/files.txt`, one on each line.
3. It runs the ignored test `highlight::reference::dump_tokens` in the
   dev image. That lexes each file in `files.txt` and writes its
   tokens to `<work>/tokens/<n>.tok`, where `n` is the line of the file
   in `files.txt`, counting from 0. Each line of a `.tok` file is one
   token, as its start, its end and its kind, such as `4 9 Keyword`.
   The offsets are in bytes. tergi joins tokens of the same kind that
   touch, except identifiers.
4. It runs `compare <work>`, which runs the real lexer over the same
   files and checks tergi's tokens against it.

The project and the work directory are mounted at the same paths in
both images, so the paths in `files.txt` work in each. A line of
`files.txt` can start with the name of a language and a tab, such as
`Cpp\tfoo.h`. tergi then lexes the file as that language rather than
guessing from its name. The names are those of `Language` in
`src/highlight/mod.rs`.

To add a check, make a directory with those three files and add its
name to the list in the workflow. Download everything in the
`Containerfile` rather than in `prepare`, so that podman caches it with
the image. Pin the version of everything it downloads, so that a check
only starts failing when tergi changes.
