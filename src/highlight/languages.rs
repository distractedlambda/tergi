//! The languages we can highlight, and how to tell which one a file is in.
//!
//! Each grammar crate ships its parser and the queries that its authors
//! wrote for `tree-sitter highlight`. Some grammars build on others. The
//! TypeScript queries only add to the JavaScript ones, and the C++ queries
//! add to the C ones. Their `tree-sitter.json` lists the base queries
//! first, because the last pattern that matches a node wins.

use std::sync::OnceLock;

use tree_sitter::Language;
use tree_sitter_highlight::HighlightConfiguration;
use tree_sitter_language::LanguageFn;

use super::NAMES;

pub struct Lang {
    /// The first name is the language's own. Injections and Markdown code
    /// blocks find a language by any of these names, or by an extension.
    names: &'static [&'static str],
    extensions: &'static [&'static str],
    /// Whole file names, for files without a telling extension.
    file_names: &'static [&'static str],
    /// Programs that a `#!` line can name to run the file.
    interpreters: &'static [&'static str],
    load: fn() -> Queries,
    config: OnceLock<Option<HighlightConfiguration>>,
}

/// A grammar and its queries. Each query can be made of several pieces,
/// which are joined in order.
struct Queries {
    language: LanguageFn,
    highlights: String,
    injections: String,
    locals: String,
}

impl Queries {
    fn new(language: LanguageFn, highlights: &[&str]) -> Queries {
        Queries {
            language,
            highlights: highlights.join("\n"),
            injections: String::new(),
            locals: String::new(),
        }
    }

    fn injections(self, injections: &[&str]) -> Queries {
        Queries {
            injections: injections.join("\n"),
            ..self
        }
    }

    fn locals(self, locals: &[&str]) -> Queries {
        Queries {
            locals: locals.join("\n"),
            ..self
        }
    }
}

impl Lang {
    pub fn name(&self) -> &'static str {
        self.names[0]
    }

    /// The parser and compiled queries. Compiling the queries takes a
    /// while, so we do it the first time a file needs them. `None` means
    /// the queries don't compile, which the tests rule out.
    pub fn config(&self) -> Option<&HighlightConfiguration> {
        self.config
            .get_or_init(|| {
                let q = (self.load)();
                let mut config = HighlightConfiguration::new(
                    Language::new(q.language),
                    self.name(),
                    &q.highlights,
                    &q.injections,
                    &q.locals,
                )
                .ok()?;
                config.configure(&NAMES.iter().map(|(name, _)| name).collect::<Vec<_>>());
                Some(config)
            })
            .as_ref()
    }
}

/// Find the language of a file from its path, or failing that from a `#!`
/// line at its start.
pub fn detect(path: &str, first_line: &str) -> Option<&'static Lang> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let by_name = || LANGS.iter().find(|l| l.file_names.contains(&name));
    let by_extension = || {
        let (_, ext) = name.rsplit_once('.')?;
        LANGS
            .iter()
            .find(|l| l.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
    };
    by_name()
        .or_else(by_extension)
        .or_else(|| by_interpreter(first_line))
}

/// Find a language by a name that an injection query or a Markdown code
/// block gives, such as `javascript`, `js` or `Rust`.
pub fn named(name: &str) -> Option<&'static Lang> {
    LANGS.iter().find(|l| {
        l.names
            .iter()
            .chain(l.extensions)
            .any(|n| n.eq_ignore_ascii_case(name))
    })
}

/// Look at a line like `#!/usr/bin/env -S python3 -u` or `#!/bin/sh`.
fn by_interpreter(first_line: &str) -> Option<&'static Lang> {
    let rest = first_line.strip_prefix("#!")?;
    let mut words = rest.split_whitespace();
    let mut program = words.next()?.rsplit('/').next()?;
    if program == "env" {
        program = words.find(|w| !w.starts_with('-'))?;
    }
    // Allow a version, as in python3 or python3.12.
    let bare = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    LANGS
        .iter()
        .find(|l| l.interpreters.contains(&program) || l.interpreters.contains(&bare))
}

const fn lang(
    names: &'static [&'static str],
    extensions: &'static [&'static str],
    load: fn() -> Queries,
) -> Lang {
    Lang {
        names,
        extensions,
        file_names: &[],
        interpreters: &[],
        load,
        config: OnceLock::new(),
    }
}

impl Lang {
    const fn files(mut self, file_names: &'static [&'static str]) -> Lang {
        self.file_names = file_names;
        self
    }

    const fn run_by(mut self, interpreters: &'static [&'static str]) -> Lang {
        self.interpreters = interpreters;
        self
    }
}

static LANGS: [Lang; 36] = [
    lang(
        &["bash", "sh", "shell", "zsh"],
        &["sh", "bash", "zsh", "ksh"],
        || {
            Queries::new(
                tree_sitter_bash::LANGUAGE,
                &[tree_sitter_bash::HIGHLIGHT_QUERY],
            )
        },
    )
    .files(&[
        ".bashrc",
        ".bash_profile",
        ".bash_aliases",
        ".profile",
        ".zshrc",
        ".zprofile",
        ".zshenv",
    ])
    .run_by(&["sh", "bash", "zsh", "dash", "ksh", "ash"]),
    lang(&["c"], &["c", "h"], || {
        Queries::new(tree_sitter_c::LANGUAGE, &[tree_sitter_c::HIGHLIGHT_QUERY])
    }),
    lang(
        &["cpp", "c++"],
        &[
            "cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++", "ipp", "tpp",
        ],
        || {
            Queries::new(
                tree_sitter_cpp::LANGUAGE,
                &[
                    tree_sitter_c::HIGHLIGHT_QUERY,
                    tree_sitter_cpp::HIGHLIGHT_QUERY,
                ],
            )
        },
    ),
    lang(&["c_sharp", "csharp", "c#"], &["cs", "csx"], || {
        Queries::new(
            tree_sitter_c_sharp::LANGUAGE,
            &[tree_sitter_c_sharp::HIGHLIGHTS_QUERY],
        )
    }),
    lang(&["css"], &["css"], || {
        Queries::new(
            tree_sitter_css::LANGUAGE,
            &[tree_sitter_css::HIGHLIGHTS_QUERY],
        )
    }),
    lang(&["diff"], &["diff", "patch"], || {
        Queries::new(
            tree_sitter_diff::LANGUAGE,
            &[tree_sitter_diff::HIGHLIGHTS_QUERY],
        )
    }),
    lang(&["elixir"], &["ex", "exs"], || {
        Queries::new(
            tree_sitter_elixir::LANGUAGE,
            &[tree_sitter_elixir::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_elixir::INJECTIONS_QUERY])
    })
    .run_by(&["elixir"]),
    lang(&["go", "golang"], &["go"], || {
        Queries::new(
            tree_sitter_go::LANGUAGE,
            &[tree_sitter_go::HIGHLIGHTS_QUERY],
        )
    }),
    lang(&["haskell"], &["hs"], || {
        Queries::new(
            tree_sitter_haskell::LANGUAGE,
            &[tree_sitter_haskell::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_haskell::INJECTIONS_QUERY])
        .locals(&[tree_sitter_haskell::LOCALS_QUERY])
    })
    .run_by(&["runghc", "runhaskell"]),
    lang(&["html"], &["html", "htm", "xhtml"], || {
        Queries::new(
            tree_sitter_html::LANGUAGE,
            &[tree_sitter_html::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_html::INJECTIONS_QUERY])
    }),
    // Git's own config files are close enough to INI.
    lang(&["ini", "gitconfig"], &["ini"], || {
        Queries::new(
            tree_sitter_ini::LANGUAGE,
            &[tree_sitter_ini::HIGHLIGHTS_QUERY],
        )
    })
    .files(&[".gitconfig", ".gitmodules", ".editorconfig"]),
    lang(&["java"], &["java"], || {
        Queries::new(
            tree_sitter_java::LANGUAGE,
            &[tree_sitter_java::HIGHLIGHTS_QUERY],
        )
    }),
    lang(&["javascript", "jsx"], &["js", "mjs", "cjs", "jsx"], || {
        Queries::new(
            tree_sitter_javascript::LANGUAGE,
            &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            ],
        )
        .injections(&[tree_sitter_javascript::INJECTIONS_QUERY])
        .locals(&[tree_sitter_javascript::LOCALS_QUERY])
    })
    .run_by(&["node", "nodejs"]),
    lang(&["json", "jsonc"], &["json", "jsonc"], || {
        Queries::new(
            tree_sitter_json::LANGUAGE,
            &[tree_sitter_json::HIGHLIGHTS_QUERY],
        )
    }),
    lang(&["lua"], &["lua"], || {
        Queries::new(
            tree_sitter_lua::LANGUAGE,
            &[tree_sitter_lua::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_lua::INJECTIONS_QUERY])
        .locals(&[tree_sitter_lua::LOCALS_QUERY])
    })
    .run_by(&["lua", "luajit"]),
    lang(&["make", "makefile"], &["mk", "mak", "make"], || {
        Queries::new(
            tree_sitter_make::LANGUAGE,
            &[tree_sitter_make::HIGHLIGHTS_QUERY],
        )
    })
    .files(&["Makefile", "makefile", "GNUmakefile"])
    .run_by(&["make"]),
    lang(&["markdown"], &["md", "markdown", "mkd"], || {
        Queries::new(tree_sitter_md::LANGUAGE, &[&markdown_highlights()])
            .injections(&[&markdown_injections()])
    }),
    // The Markdown grammar parses the text inside blocks with this one.
    lang(&["markdown_inline"], &[], || {
        Queries::new(
            tree_sitter_md::INLINE_LANGUAGE,
            &[tree_sitter_md::HIGHLIGHT_QUERY_INLINE],
        )
        .injections(&[tree_sitter_md::INJECTION_QUERY_INLINE])
    }),
    lang(&["nix"], &["nix"], || {
        Queries::new(
            tree_sitter_nix::LANGUAGE,
            &[tree_sitter_nix::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_nix::INJECTIONS_QUERY])
    }),
    lang(&["ocaml"], &["ml"], || {
        Queries::new(
            tree_sitter_ocaml::LANGUAGE_OCAML,
            &[tree_sitter_ocaml::HIGHLIGHTS_QUERY],
        )
    })
    .run_by(&["ocaml"]),
    lang(&["ocaml_interface"], &["mli"], || {
        Queries::new(
            tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE,
            &[tree_sitter_ocaml::HIGHLIGHTS_QUERY],
        )
    }),
    // The crate leaves out the query that parses the HTML around the PHP
    // code, so we copy it here.
    lang(&["php"], &["php", "phtml"], || {
        Queries::new(
            tree_sitter_php::LANGUAGE_PHP,
            &[tree_sitter_php::HIGHLIGHTS_QUERY],
        )
        .injections(&[
            tree_sitter_php::INJECTIONS_QUERY,
            "((text) @injection.content \
             (#set! injection.language \"html\") \
             (#set! injection.combined))",
        ])
    })
    .run_by(&["php"]),
    lang(&["proto", "protobuf"], &["proto"], || {
        Queries::new(
            tree_sitter_proto::LANGUAGE,
            &[tree_sitter_proto::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_proto::INJECTIONS_QUERY])
    }),
    lang(&["python"], &["py", "pyi", "pyw"], || {
        Queries::new(
            tree_sitter_python::LANGUAGE,
            &[tree_sitter_python::HIGHLIGHTS_QUERY],
        )
    })
    .run_by(&["python"]),
    lang(&["racket"], &["rkt", "rktl", "rktd"], || {
        Queries::new(
            tree_sitter_racket::LANGUAGE,
            &[tree_sitter_racket::HIGHLIGHTS_QUERY],
        )
        .locals(&[tree_sitter_racket::LOCALS_QUERY])
    })
    .run_by(&["racket"]),
    lang(&["regex"], &[], || {
        Queries::new(
            tree_sitter_regex::LANGUAGE,
            &[tree_sitter_regex::HIGHLIGHTS_QUERY],
        )
    }),
    lang(&["ruby"], &["rb", "rake", "gemspec", "ru"], || {
        Queries::new(
            tree_sitter_ruby::LANGUAGE,
            &[tree_sitter_ruby::HIGHLIGHTS_QUERY],
        )
        .locals(&[tree_sitter_ruby::LOCALS_QUERY])
    })
    .files(&["Rakefile", "Gemfile"])
    .run_by(&["ruby"]),
    lang(&["rust"], &["rs"], || {
        Queries::new(
            tree_sitter_rust::LANGUAGE,
            &[tree_sitter_rust::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_rust::INJECTIONS_QUERY])
    }),
    lang(&["scala"], &["scala", "sc", "sbt"], || {
        Queries::new(
            tree_sitter_scala::LANGUAGE,
            &[tree_sitter_scala::HIGHLIGHTS_QUERY],
        )
        .locals(&[tree_sitter_scala::LOCALS_QUERY])
    })
    .run_by(&["scala"]),
    lang(&["swift"], &["swift"], || {
        Queries::new(
            tree_sitter_swift::LANGUAGE,
            &[tree_sitter_swift::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_swift::INJECTIONS_QUERY])
        .locals(&[tree_sitter_swift::LOCALS_QUERY])
    }),
    lang(&["toml"], &["toml"], || {
        Queries::new(
            tree_sitter_toml_ng::LANGUAGE,
            &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
        )
    })
    .files(&["Cargo.lock", "uv.lock", "poetry.lock", "Pipfile"]),
    lang(&["typescript"], &["ts", "mts", "cts"], || {
        Queries::new(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ],
        )
        .injections(&[tree_sitter_javascript::INJECTIONS_QUERY])
        .locals(&[
            tree_sitter_javascript::LOCALS_QUERY,
            tree_sitter_typescript::LOCALS_QUERY,
        ])
    })
    .run_by(&["ts-node"]),
    lang(&["tsx"], &["tsx"], || {
        Queries::new(
            tree_sitter_typescript::LANGUAGE_TSX,
            &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ],
        )
        .injections(&[tree_sitter_javascript::INJECTIONS_QUERY])
        .locals(&[
            tree_sitter_javascript::LOCALS_QUERY,
            tree_sitter_typescript::LOCALS_QUERY,
        ])
    }),
    lang(
        &["xml", "svg"],
        &["xml", "svg", "xsd", "xsl", "xslt", "plist", "rss", "atom"],
        || {
            Queries::new(
                tree_sitter_xml::LANGUAGE_XML,
                &[tree_sitter_xml::XML_HIGHLIGHT_QUERY],
            )
        },
    ),
    lang(&["yaml", "yml"], &["yaml", "yml"], || {
        Queries::new(
            tree_sitter_yaml::LANGUAGE,
            &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
        )
    }),
    lang(&["zig"], &["zig", "zon"], || {
        Queries::new(
            tree_sitter_zig::LANGUAGE,
            &[tree_sitter_zig::HIGHLIGHTS_QUERY],
        )
        .injections(&[tree_sitter_zig::INJECTIONS_QUERY])
    }),
];

/// The Markdown query colors a whole fenced code block as literal text,
/// then resets the code inside with `@none`, so that the language the
/// block names can color it. But when the reset and the code's first
/// highlight start at the same byte, tree-sitter-highlight starts the
/// reset second, and it hides that highlight. So we leave out both
/// patterns. The fences keep their own color.
fn markdown_highlights() -> String {
    tree_sitter_md::HIGHLIGHT_QUERY_BLOCK
        .replace("  (fenced_code_block)\n] @text.literal", "] @text.literal")
        .replace("(code_fence_content) @none", "")
}

/// The block grammar keeps the punctuation inside a code block or a line
/// of text, such as `=`, `*` and backticks, as children of the node that
/// it hands to another grammar. An injection leaves out the children of
/// its node, unless it says otherwise, so the other grammar would see the
/// text with those bytes cut out. Including them also hands it the `> `
/// that continues a quote, which does less harm.
fn markdown_injections() -> String {
    tree_sitter_md::INJECTION_QUERY_BLOCK.replace(
        "@injection.content",
        "@injection.content\n  (#set! injection.include-children)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_loads() {
        for l in &LANGS {
            let q = (l.load)();
            if let Err(e) = HighlightConfiguration::new(
                Language::new(q.language),
                l.name(),
                &q.highlights,
                &q.injections,
                &q.locals,
            ) {
                panic!("{}: {e}", l.name());
            }
        }
    }

    #[test]
    fn markdown_queries_still_have_the_patterns_we_change() {
        let q = markdown_highlights();
        assert!(!q.contains("fenced_code_block)"), "{q}");
        assert!(!q.contains("code_fence_content"), "{q}");
        let q = markdown_injections();
        assert_eq!(
            q.matches("include-children").count(),
            q.matches("@injection.content").count(),
        );
        assert!(q.contains("code_fence_content"), "{q}");
        assert!(q.contains("markdown_inline"), "{q}");
    }

    #[test]
    fn finds_languages() {
        let found = |path: &str, first: &str| detect(path, first).map(Lang::name);
        assert_eq!(found("src/main.rs", ""), Some("rust"));
        assert_eq!(found("a/B.JAVA", ""), Some("java"));
        assert_eq!(found("Makefile", ""), Some("make"));
        assert_eq!(found("x/Cargo.lock", ""), Some("toml"));
        assert_eq!(found("script", "#!/bin/sh"), Some("bash"));
        assert_eq!(
            found("script", "#!/usr/bin/env python3.12 -u"),
            Some("python")
        );
        assert_eq!(
            found("script", "#!/usr/bin/env -S node --flag"),
            Some("javascript")
        );
        assert_eq!(found("notes.txt", ""), None);
        assert_eq!(found("script", "#!/usr/bin/env frobnicate"), None);
        assert_eq!(named("JS").map(Lang::name), Some("javascript"));
        assert_eq!(
            named("markdown_inline").map(Lang::name),
            Some("markdown_inline")
        );
        assert_eq!(named("py").map(Lang::name), Some("python"));
        assert_eq!(named("klingon").map(Lang::name), None);
    }
}
