package main

import (
	"archive/tar"
	"compress/gzip"
	"fmt"
	"io"
	"io/fs"
	"math/rand/v2"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"unicode/utf8"
)

// Where the Containerfile puts the corpus: a tarball for each project,
// and BuildKit's source in a directory.
const corpus = "/opt/corpus"

// Files that tergi reads as Dockerfiles, apart from the ones whose
// extension says they're something else, such as Dockerfile.md.
var (
	dockerfileName = regexp.MustCompile(`(?i)^(docker|container)file([._-].*)?$|\.(docker|container)file$`)
	otherExtension = regexp.MustCompile(`(?i)\.(go|md|txt|json|ya?ml|sh|ps1|bat|py|rs|toml|orig|patch)$`)
)

func isDockerfile(path string) bool {
	name := filepath.Base(path)
	return dockerfileName.MatchString(name) && !otherExtension.MatchString(name)
}

func prepare(work string) {
	dir := filepath.Join(work, "corpus")
	if _, err := os.Stat(dir); err != nil {
		tmp := dir + ".tmp"
		if err := os.RemoveAll(tmp); err != nil {
			fail(err)
		}
		unpack(tmp)
		if err := os.Rename(tmp, dir); err != nil {
			fail(err)
		}
	}
	var paths []string
	err := filepath.WalkDir(dir, func(path string, d fs.DirEntry, err error) error {
		if err == nil && d.Type().IsRegular() {
			paths = append(paths, path)
		}
		return err
	})
	if err != nil {
		fail(err)
	}
	var kept []string
	for _, p := range paths {
		src, err := os.ReadFile(p)
		if err != nil {
			fail(err)
		}
		if utf8.Valid(src) && parses(src) {
			kept = append(kept, p)
		}
	}
	files := len(kept)

	randoms := filepath.Join(work, "random")
	if err := os.RemoveAll(randoms); err != nil {
		fail(err)
	}
	if err := os.MkdirAll(randoms, 0o755); err != nil {
		fail(err)
	}
	seed := envInt("SEED", 1)
	cases := envInt("CASES", 20000)
	r := rand.New(rand.NewPCG(uint64(seed), 0x646f636b))
	for n := range cases {
		src := []byte(dockerfile(r))
		if !parses(src) {
			continue
		}
		p := filepath.Join(randoms, fmt.Sprintf("%d.dockerfile", n))
		if err := os.WriteFile(p, src, 0o644); err != nil {
			fail(err)
		}
		kept = append(kept, p)
	}

	var list strings.Builder
	for _, p := range kept {
		fmt.Fprintf(&list, "Dockerfile\t%s\n", p)
	}
	if err := os.WriteFile(filepath.Join(work, "files.txt"), []byte(list.String()), 0o644); err != nil {
		fail(err)
	}
	fmt.Printf("%d Dockerfiles and %d random texts that BuildKit parses\n", files, len(kept)-files)
}

func envInt(name string, def int) int {
	s := os.Getenv(name)
	if s == "" {
		return def
	}
	n, err := strconv.Atoi(s)
	if err != nil {
		fail(fmt.Errorf("%s isn't a number: %q", name, s))
	}
	return n
}

// Copy the Dockerfiles from the corpus into dir.
func unpack(dir string) {
	write := func(path string, r io.Reader) {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			fail(err)
		}
		out, err := os.Create(path)
		if err != nil {
			fail(err)
		}
		if _, err := io.Copy(out, r); err != nil {
			fail(err)
		}
		if err := out.Close(); err != nil {
			fail(err)
		}
	}
	tarballs, err := filepath.Glob(filepath.Join(corpus, "*.tar.gz"))
	if err != nil {
		fail(err)
	}
	for _, t := range tarballs {
		name := strings.TrimSuffix(filepath.Base(t), ".tar.gz")
		in, err := os.Open(t)
		if err != nil {
			fail(err)
		}
		gz, err := gzip.NewReader(in)
		if err != nil {
			fail(err)
		}
		tr := tar.NewReader(gz)
		for {
			h, err := tr.Next()
			if err == io.EOF {
				break
			} else if err != nil {
				fail(err)
			}
			// Leave out the directory that GitHub puts everything in.
			_, rel, _ := strings.Cut(h.Name, "/")
			if h.Typeflag == tar.TypeReg && isDockerfile(rel) && filepath.IsLocal(rel) {
				write(filepath.Join(dir, name, rel), tr)
			}
		}
		in.Close()
	}
	src := filepath.Join(corpus, "buildkit")
	err = filepath.WalkDir(src, func(path string, d fs.DirEntry, err error) error {
		if err != nil || !d.Type().IsRegular() || !isDockerfile(path) {
			return err
		}
		in, err := os.Open(path)
		if err != nil {
			return err
		}
		defer in.Close()
		rel, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		write(filepath.Join(dir, "buildkit", rel), in)
		return nil
	})
	if err != nil {
		fail(err)
	}
}

// Random Dockerfiles are made of these pieces. They're tricky for the
// lexer, and most of them are as likely as any other.

var names = []string{
	"FROM", "from", "RUN", "run", "Run", "CMD", "ENTRYPOINT", "COPY", "copy",
	"ADD", "ENV", "env", "ARG", "LABEL", "EXPOSE", "VOLUME", "USER",
	"WORKDIR", "STOPSIGNAL", "HEALTHCHECK", "SHELL", "ONBUILD", "MAINTAINER",
	"FORM", "RUNX",
}

var words = []string{
	"a", "b c", "/app", "./src", "build", "stage-1", "x:1.2", "true", "8080/tcp",
	"80-90", "AS", "as", "CMD", "cmd", "NONE", "&&", ";", "|", "if", "then",
	"fi", "echo", "$x", "${x}", "${x:-y}", "${x:+$y}", "${x-$y}", "${x#*/}",
	"${x%%.*}", "${x/a/b}", "${x//a}", "${x:?}", "${x?m}", "$1", "$12", "$@",
	"$$", "${é}", "$é", "$", "${", "}", "{", "!", `"q $x"`, `'s $x'`,
	`"a \" $b"`, `"a`, `b"`, `'a`, `b'`, `"`, `'`, "=", "a=b", "k=v", `k="v w"`,
	"k=$v", "k=", "=v", `"k"=v`, "$k=v", "--mount=type=cache,target=/x",
	"--from=build", "--from=$s", "--platform=$P", "--", "--link", "-x",
	`--chown="a b"`, "--x='y z'", "--interval=5s", "--unset=a", `["a", "b"]`,
	`["a",`, `"b"]`, `["é\n"]`, `[1]`, "[]", "[", "]", ",", "<<EOF",
	"<<-EOF", `<<"EOF"`, "<<'E'", "<< EOF", "cat<<EOF", "2<<EOF", `<<E"O"F`,
	"<<E\\OF", "<<-'EOF'", "#", "# c", "#!", "é", "\u00a0", "à", "`", "\\",
	"x\\", "\\\\", "\\$x", "`$x", "\\\"", "``", "\t", "\v", "\r",
}

var bodies = []string{
	"echo $x", "#!/bin/sh", "#!/usr/bin/env python3", "\techo hi", "$x",
	"${y:-z}", "\\$x", "'$q'", `"$z"`, "a \\", "FROM x", "RUN y", "# c", "",
	"\tEOF", "EOF ", "EO", "E", "print(1)", "if x; then", "`", "x`",
}

var terminators = []string{"EOF", "EOF", "\tEOF", "E", "EOF\r"}

func pick(r *rand.Rand, xs []string) string {
	return xs[r.IntN(len(xs))]
}

func dockerfile(r *rand.Rand) string {
	var b strings.Builder
	escape := "\\"
	if r.IntN(5) == 0 {
		escape = "`"
	}
	if r.IntN(3) == 0 {
		b.WriteString(pick(r, []string{"\xef\xbb\xbf", ""}))
		for range r.IntN(3) {
			b.WriteString(pick(r, []string{
				"# syntax=docker/dockerfile:1\n", "#check = skip=all\n",
				" # Syntax = a b \n", "# foo=bar\n", "#escape=" + escape + "\n",
			}))
		}
		if escape == "`" && r.IntN(2) == 0 {
			b.WriteString("# escape=`\n")
		}
	}
	for range 1 + r.IntN(6) {
		switch r.IntN(8) {
		case 0:
			b.WriteString(pick(r, []string{"# c\n", "  # x y\n", "#\n", "\n"}))
			continue
		case 1:
			b.WriteString(pick(r, []string{"\n", "  \n", "\t\n"}))
			continue
		}
		start := b.Len()
		if r.IntN(4) == 0 {
			b.WriteString(pick(r, []string{" ", "\t"}))
		}
		name := pick(r, names)
		b.WriteString(name)
		if name == "ONBUILD" && r.IntN(2) == 0 {
			b.WriteString(" " + pick(r, names))
		}
		for n := range r.IntN(9) {
			switch k := r.IntN(12); {
			case k < 8:
				b.WriteString(pick(r, []string{" ", " ", " ", "  ", "\t"}))
			case k < 9:
			default:
				// Words can go on over a line, but tergi ends them at the
				// end of the line. The check skips the rest of a file where
				// that happens to the name of an instruction, so a space
				// keeps it from happening there.
				sep := pick(r, []string{" ", " ", ""})
				if n == 0 {
					sep = " "
				}
				b.WriteString(sep + escape + pick(r, []string{"", " "}) + "\n")
				switch r.IntN(6) {
				case 0:
					b.WriteString(pick(r, []string{"# c\n", "  # c " + escape + "\n"}))
				case 1:
					b.WriteString(pick(r, []string{"\n", "  \n"}))
				}
				b.WriteString(pick(r, []string{"", "  ", "\t"}))
			}
			b.WriteString(pick(r, words))
		}
		b.WriteString(pick(r, []string{"\n", "\n", "\n", "\n", "\r\n", " " + escape + "\n"}))
		// Give each here-document a body, so that most files parse.
		for range strings.Count(b.String()[start:], "<<") {
			for range r.IntN(4) {
				b.WriteString(pick(r, bodies) + "\n")
			}
			b.WriteString(pick(r, terminators) + pick(r, []string{"\n", "\n", ""}))
		}
	}
	return b.String()
}
