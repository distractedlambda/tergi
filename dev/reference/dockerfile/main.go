// Command dockerfile-check checks tergi's colors for Dockerfiles against
// BuildKit's parser.
//
// Usage:
//
//	dockerfile-check prepare <work>
//	dockerfile-check compare <work>
//
// prepare unpacks the corpus and makes random Dockerfiles in the work
// directory, keeps the ones that BuildKit parses, and lists them in
// <work>/files.txt. compare reads the tokens that tergi wrote to
// <work>/tokens and checks them against what BuildKit makes of each file.
//
// BuildKit's parser only reports lines, not bytes. So compare goes over
// the lines the way parser.Parse does, and keeps track of where each byte
// of an instruction's joined text came from. It checks that it joined
// each instruction the way the parser did, by comparing the result with
// the node's Original text. With that, it can say which bytes the
// parser read as what. It checks that:
//
//   - Parser directives such as `# syntax=x` get their colors, and
//     comment lines get the comment color.
//   - The escape character at the end of each line that goes on gets the
//     escape color.
//   - Instructions that BuildKit knows get the keyword color, and so do
//     `AS` in FROM and `CMD` and `NONE` in HEALTHCHECK. Nothing else gets
//     the keyword or comment color, outside of shell commands.
//   - The name of each flag, such as `--mount`, gets the attribute color.
//   - A command or a list gets JSON's colors exactly when BuildKit reads
//     it as JSON.
//   - The variables that tergi marks in each word that BuildKit expands
//     are the ones that BuildKit's shell package finds there.
//   - The line that ends each here-document starts with its delimiter in
//     the attribute color. In a COPY or an ADD, a body that tergi doesn't
//     lex in another language has the string color, apart from the
//     variables that BuildKit expands, if it expands any.
//
// BuildKit joins the lines of an instruction with nothing between them,
// so a word can go on on the next line. tergi ends words at the end of a
// line instead, so the checks skip words that BuildKit joined over a
// line. When that changes how tergi reads the lines after it, they skip
// the rest of the file. They also skip what follows from the other
// differences that design/diffs.md describes, and count all they skip.
package main

import (
	"bytes"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/moby/buildkit/frontend/dockerfile/instructions"
	"github.com/moby/buildkit/frontend/dockerfile/linter"
	"github.com/moby/buildkit/frontend/dockerfile/parser"
	"github.com/moby/buildkit/frontend/dockerfile/shell"
)

// Only the first few disagreements are printed, so that one bug doesn't
// bury the others.
const show = 40

// The instructions in the dispatch table in parser.go.
var known = map[string]bool{
	"add": true, "arg": true, "cmd": true, "copy": true, "entrypoint": true,
	"env": true, "expose": true, "from": true, "healthcheck": true,
	"label": true, "maintainer": true, "onbuild": true, "run": true,
	"shell": true, "stopsignal": true, "user": true, "volume": true,
	"workdir": true,
}

// The linter that ParseInstructionWithLinter wants, which warns about
// nothing.
var lint = linter.New(&linter.Config{SkipAll: true, Warn: func(string, string, string, string, []parser.Range) {}})

// The same as in parser.go.
var reWhitespace = regexp.MustCompile(`[\t\v\f\r ]+`)

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: dockerfile-check prepare|compare <work>")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "prepare":
		prepare(os.Args[2])
	case "compare":
		compare(os.Args[2])
	default:
		fmt.Fprintln(os.Stderr, "usage: dockerfile-check prepare|compare <work>")
		os.Exit(2)
	}
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, err)
	os.Exit(1)
}

func parses(src []byte) bool {
	_, err := parser.Parse(bytes.NewReader(src))
	return err == nil
}

// A piece of an instruction's joined text, and where it came from.
type piece struct{ at, src, n int }

// The text of an instruction as BuildKit joins it.
type joined struct {
	text   string
	pieces []piece
	// Whether the last line ends with the escape character, which only
	// happens at the end of the file.
	open bool
}

func (j *joined) push(src []byte, start, end int) {
	if end > start {
		j.pieces = append(j.pieces, piece{len(j.text), start, end - start})
		j.text += string(src[start:end])
	}
}

// The source of j.text[a:b], if it all came from one line.
func (j *joined) source(a, b int) (int, int, bool) {
	for _, p := range j.pieces {
		if a >= p.at && b <= p.at+p.n {
			return p.src + a - p.at, p.src + b - p.at, true
		}
	}
	return 0, 0, false
}

// The source ranges of j.text[a:b], one for each line it touches.
func (j *joined) sources(a, b int) [][2]int {
	var out [][2]int
	for _, p := range j.pieces {
		s, e := max(a, p.at), min(b, p.at+p.n)
		if s < e {
			out = append(out, [2]int{p.src + s - p.at, p.src + e - p.at})
		}
	}
	return out
}

type tok struct {
	s, e int
	kind string
}

// One file, with tergi's tokens and what we found wrong with them.
type file struct {
	path   string
	src    []byte
	kinds  []string
	toks   []tok
	escape byte
	found  []finding
	// Counts of things that couldn't be checked, by why.
	skipped map[string]int
}

type finding struct {
	what string
	at   int
	msg  string
}

func (f *file) report(what string, at int, format string, args ...any) {
	f.found = append(f.found, finding{what, at, fmt.Sprintf(format, args...)})
}

func (f *file) skip(why string) {
	f.skipped[why]++
}

func (f *file) kind(i int) string {
	if i < len(f.kinds) && f.kinds[i] != "" {
		return f.kinds[i]
	}
	return "nothing"
}

// The text of the token at s, or of the byte if it isn't in one.
func (f *file) token(s int) []byte {
	for _, t := range f.toks {
		if t.s <= s && s < t.e {
			return f.src[t.s:t.e]
		}
	}
	return f.src[s : s+1]
}

// Report the first byte in src[s:e] that doesn't have the kind.
func (f *file) want(what string, s, e int, kind string) {
	for i := s; i < e; i++ {
		if f.kind(i) != kind {
			f.report(what, i, "%q should be %s, but %q is %s", f.src[s:e], kind, f.src[i:i+1], f.kind(i))
			return
		}
	}
}

// The variables that tergi marks in src[s:e]: identifiers right after a
// `$` or a `${` with the punctuation color.
func (f *file) vars(s, e int) []string {
	var names []string
	for _, t := range f.toks {
		if t.kind != "Identifier" || t.s < s || t.e > e || t.s == 0 {
			continue
		}
		p := t.s - 1
		if f.src[p] == '{' && p > 0 {
			p--
		}
		if f.src[p] == '$' && f.kinds[t.s-1] == "Punctuation" && f.kinds[p] == "Punctuation" {
			names = append(names, string(f.src[t.s:t.e]))
		}
	}
	return unique(names)
}

func unique(names []string) []string {
	sort.Strings(names)
	return slices.Compact(names)
}

// Every name has a value, so that BuildKit takes every path through
// `${x:-y}` and the like, and doesn't fail on `${x:?}`.
type allSet struct{}

func (allSet) Get(string) (string, bool) { return "v", true }
func (allSet) Keys() []string            { return nil }

// The variables that BuildKit expands in a word.
func expands(lex *shell.Lex, word string) ([]string, bool) {
	r, err := lex.ProcessWordWithMatches(word, allSet{})
	if err != nil {
		return nil, false
	}
	var names []string
	for n := range r.Matched {
		names = append(names, n)
	}
	return unique(names), true
}

func compare(work string) {
	list, err := os.ReadFile(filepath.Join(work, "files.txt"))
	if err != nil {
		fail(err)
	}
	counts := map[string]int{}
	skipped := map[string]int{}
	files, bad, shown := 0, 0, 0
	for n, l := range strings.Split(strings.TrimRight(string(list), "\n"), "\n") {
		path := l[strings.LastIndexByte(l, '\t')+1:]
		f, ok := load(work, n, path)
		if !ok {
			skipped["not UTF-8"]++
			continue
		}
		files++
		f.check()
		sort.SliceStable(f.found, func(a, b int) bool { return f.found[a].at < f.found[b].at })
		if len(f.found) > 0 {
			bad++
		}
		for _, x := range f.found {
			counts[x.what]++
			if shown < show {
				shown++
				line := bytes.Count(f.src[:x.at], []byte("\n")) + 1
				fmt.Printf("%s:%d: %s: %s\n", path, line, x.what, x.msg)
			}
		}
		for why, c := range f.skipped {
			skipped[why] += c
		}
	}
	total := 0
	var parts []string
	for what, c := range counts {
		total += c
		parts = append(parts, fmt.Sprintf("%d %s", c, what))
	}
	sort.Strings(parts)
	fmt.Printf("%d files checked, %d disagreements in %d files", files, total, bad)
	for _, p := range parts {
		fmt.Printf(", %s", p)
	}
	fmt.Println()
	if len(skipped) > 0 {
		var parts []string
		for why, c := range skipped {
			parts = append(parts, fmt.Sprintf("%d %s", c, why))
		}
		sort.Strings(parts)
		fmt.Printf("Not checked: %s\n", strings.Join(parts, ", "))
	}
	if total > 0 {
		os.Exit(1)
	}
}

func load(work string, n int, path string) (*file, bool) {
	src, err := os.ReadFile(path)
	if err != nil {
		fail(err)
	}
	data, err := os.ReadFile(filepath.Join(work, "tokens", fmt.Sprintf("%d.tok", n)))
	if os.IsNotExist(err) {
		// tergi only lexes UTF-8, so it writes no tokens for other files.
		return nil, false
	} else if err != nil {
		fail(err)
	}
	f := &file{path: path, src: src, kinds: make([]string, len(src)+1), skipped: map[string]int{}}
	for _, l := range strings.Split(strings.TrimSpace(string(data)), "\n") {
		fields := strings.Fields(l)
		if len(fields) != 3 {
			continue
		}
		s, err1 := strconv.Atoi(fields[0])
		e, err2 := strconv.Atoi(fields[1])
		if err1 != nil || err2 != nil || s > e || e > len(src) {
			fail(fmt.Errorf("%s: bad token %q", path, l))
		}
		f.toks = append(f.toks, tok{s, e, fields[2]})
		for i := s; i < e; i++ {
			f.kinds[i] = fields[2]
		}
	}
	return f, true
}

// A line of the source. end leaves out the line break, and next is where
// the next line starts.
type line struct{ start, end, next int }

func splitLines(src []byte) []line {
	var lines []line
	for start := 0; start < len(src); {
		next := len(src)
		if i := bytes.IndexByte(src[start:], '\n'); i >= 0 {
			next = start + i + 1
		}
		// trimNewline removes every CR and LF at the end.
		end := start + len(bytes.TrimRight(src[start:next], "\r\n"))
		lines = append(lines, line{start, end, next})
		start = next
	}
	return lines
}

func isComment(b []byte) bool {
	b = bytes.TrimLeftFunc(b, unicode.IsSpace)
	return len(b) > 0 && b[0] == '#'
}

func (f *file) check() {
	res, err := parser.Parse(bytes.NewReader(f.src))
	if err != nil {
		f.report("harness", 0, "BuildKit can't parse it any more: %v", err)
		return
	}
	f.escape = byte(res.EscapeToken)
	e := regexp.QuoteMeta(string(rune(f.escape)))
	// The same as setEscapeToken's.
	continuation := regexp.MustCompile(`([^` + e + `])` + e + `[ \t]*$|^` + e + `[ \t]*$`)
	// The content of a line without the escape character that makes it go
	// on, and where that is, or -1.
	trim := func(start, end int) (int, int) {
		m := continuation.FindSubmatchIndex(f.src[start:end])
		switch {
		case m == nil:
			return end, -1
		case m[2] >= 0:
			return start + m[3], start + m[3]
		default:
			return start, start
		}
	}

	lines := splitLines(f.src)
	var directives parser.DirectiveParser
	nodes := res.AST.Children
	done := false
	for i := 0; i < len(lines); {
		l := lines[i]
		start := l.start
		if i == 0 && bytes.HasPrefix(f.src, []byte("\xef\xbb\xbf")) {
			start += 3
		}
		start = l.end - len(bytes.TrimLeftFunc(f.src[start:l.end], unicode.IsSpace))
		if !done {
			if d, _ := directives.ParseLine(f.src[start:l.end]); d != nil {
				f.directive(start, l.end)
				i++
				continue
			}
			done = true
		}
		if isComment(f.src[start:l.end]) {
			f.comment(start, l.end)
			i++
			continue
		}
		end, esc := trim(start, l.end)
		if esc < 0 && end == start {
			i++
			continue
		}

		var j joined
		var escapes []int
		first := i
		j.push(f.src, start, end)
		i++
		if esc >= 0 {
			escapes = append(escapes, esc)
		}
		for esc >= 0 && i < len(lines) {
			l := lines[i]
			i++
			if isComment(f.src[l.start:l.end]) {
				f.comment(l.start, l.end)
				continue
			}
			if len(bytes.TrimLeftFunc(f.src[l.start:l.end], unicode.IsSpace)) == 0 {
				continue
			}
			end, esc = trim(l.start, l.end)
			j.push(f.src, l.start, end)
			if esc >= 0 {
				escapes = append(escapes, esc)
			}
		}
		j.open = esc >= 0
		if len(nodes) == 0 {
			f.report("harness", lines[first].start, "BuildKit has no node for this instruction")
			return
		}
		node := nodes[0]
		nodes = nodes[1:]
		if node.Original != j.text || node.StartLine != first+1 {
			f.report("harness", lines[first].start, "BuildKit joined line %d as %q, not %q",
				node.StartLine, node.Original, j.text)
			return
		}
		for _, e := range escapes {
			f.want("escape", e, e+1, "Escape")
		}
		i = f.instruction(node, &j, lines, i)
		if i < 0 {
			f.skip("files where a word that matters goes on over a line, after that")
			return
		}
		if node.EndLine != i {
			f.report("harness", lines[first].start, "BuildKit ends this instruction on line %d, not %d",
				node.EndLine, i)
			return
		}
	}
	if len(nodes) > 0 {
		f.report("harness", len(f.src), "BuildKit has %d more nodes", len(nodes))
	}
}

// Check a directive such as `# syntax=x` in src[s:e].
func (f *file) directive(s, e int) {
	re := regexp.MustCompile(`^#\s*([a-zA-Z][a-zA-Z0-9]*)\s*(=)\s*(.+?)\s*$`)
	m := re.FindSubmatchIndex(f.src[s:e])
	if m == nil {
		f.report("harness", s, "can't find the parts of the directive %q", f.src[s:e])
		return
	}
	f.want("directive", s, s+1, "Attribute")
	f.want("directive", s+m[2], s+m[3], "Attribute")
	f.want("directive", s+m[4], s+m[5], "Operator")
	f.want("directive", s+m[6], s+m[7], "String")
}

func (f *file) comment(s, e int) {
	t := bytes.TrimFunc(f.src[s:e], unicode.IsSpace)
	s += bytes.Index(f.src[s:e], t)
	for i := s; i < s+len(t); i++ {
		if f.kind(i) != "Comment" {
			f.report("comment", i, "%q is %s", f.src[s:e], f.kind(i))
			return
		}
	}
}

// What we expect of the bytes of an instruction's joined text.
type expect struct {
	// Ranges that should be keywords. No other byte should be one, except
	// in the ranges that may be keywords.
	keywords [][2]int
	maybe    [][2]int
	// Set when tergi reads the instruction so differently that the rest of
	// the file can't be checked.
	stop bool
	// Ranges where the shell lexer colors the text, which may have
	// keywords and comments of its own.
	shell [][2]int
}

func inside(rs [][2]int, i int) bool {
	for _, r := range rs {
		if i >= r[0] && i < r[1] {
			return true
		}
	}
	return false
}

// Check an instruction, given its node and joined text, and then the
// bodies of its here-documents, which start on line `next`. Return the
// line after them, or -1 if the rest of the file can't be checked.
func (f *file) instruction(node *parser.Node, j *joined, lines []line, next int) int {
	// When BuildKit joins the instruction's name over a line, tergi reads
	// another name, and so the rest of the instruction differently. It may
	// even find a here-document there that takes up the lines after it.
	if _, _, ok := j.source(nameAt(node.Original, 0)); !ok {
		return -1
	}
	// The same goes for a word that BuildKit joins over a line in an
	// instruction that may have here-documents, because tergi can find
	// other here-documents there.
	if strings.Contains(j.text, "<<") {
		for _, p := range j.pieces[1:] {
			r1, _ := utf8.DecodeLastRuneInString(j.text[:p.at])
			r2, _ := utf8.DecodeRuneInString(j.text[p.at:])
			// An escaped space doesn't end a word either.
			escaped := p.at > 1 && j.text[p.at-2] == '\\'
			if p.at > 0 && (!unicode.IsSpace(r1) || escaped) && !unicode.IsSpace(r2) {
				return -1
			}
		}
	}
	var x expect
	sub := f.node(node, j, 0, &x)
	if x.stop {
		return -1
	}
	// tergi ends words at the end of a line, so it can find a keyword in
	// part of a word that BuildKit joins over a line.
	var split [][2]int
	for _, w := range fieldRanges(j.text) {
		if _, _, ok := j.source(w[0], w[1]); !ok {
			split = append(split, w)
		}
	}
	last := -2
	for i := range j.text {
		if inside(x.shell, i) || inside(split, i) {
			continue
		}
		s, _, ok := j.source(i, i+1)
		if !ok {
			continue
		}
		// One report for each token is enough.
		k := f.kind(s)
		if s == last+1 && f.kind(last) == k {
			last = s
			continue
		}
		last = s
		switch {
		case k == "Keyword" && !inside(x.keywords, i) && !inside(x.maybe, i):
			f.report("extra keyword", s, "%q is a keyword", f.token(s))
		case k == "Comment":
			f.report("extra comment", s, "%q is a comment", f.token(s))
		}
	}
	for _, k := range x.keywords {
		if s, e, ok := j.source(k[0], k[1]); ok {
			f.want("keyword", s, e, "Keyword")
		} else {
			f.skip("keywords split over lines")
		}
	}

	name := strings.ToLower(sub.Value)
	// The shell lexer colors the bodies of a RUN that isn't just one
	// here-document, along with the command, the way the shell reads them.
	// That isn't always the way BuildKit reads them. The shell doesn't see
	// a here-document in a comment, and it may not see one after a
	// backquote. It reads `<<` in words such as `cat<<EOF`, where BuildKit
	// doesn't, and it doesn't split words at spaces such as U+00A0. And
	// when bash can't parse the script, it can read any line as part of
	// something that isn't closed.
	shell := name == "run" && sub.Next != nil && parser.MustParseHeredoc(sub.Next.Value) == nil
	odd := shell && (strings.ContainsAny(sub.Next.Value, "`#") ||
		strings.Count(sub.Next.Value, "<<") != len(node.Heredocs) ||
		strings.ContainsFunc(sub.Next.Value, func(r rune) bool { return r > 0x7f && unicode.IsSpace(r) }) ||
		!bashParses(sub.Next.Value, node.Heredocs))
	for _, doc := range node.Heredocs {
		// And in a body that it expands, a line that ends in `\` goes on
		// on the next line, which then can't end the body.
		joins := false
		var body []line
		for next < len(lines) {
			l := lines[next]
			next++
			t := f.src[l.start:l.end]
			if doc.Chomp {
				t = bytes.TrimLeft(t, "\t")
			}
			if string(t) == doc.Name {
				switch {
				case odd:
					f.skip("here-documents that the shell may read differently")
				case joins:
					f.skip("here-documents that the shell joins to the line before their end")
				default:
					f.want("here-document end", l.end-len(t), l.end, "Attribute")
				}
				break
			}
			joins = shell && doc.Expand && bytes.HasSuffix(f.src[l.start:l.end], []byte("\\"))
			body = append(body, l)
		}
		if name == "copy" || name == "add" {
			f.copyBody(sub, doc, body)
		}
	}
	return next
}

// Whether bash parses the script that BuildKit gives the shell for a RUN
// with here-documents, which dispatchRun in dockerfile2llb's convert.go
// puts together.
func bashParses(cmd string, docs []parser.Heredoc) bool {
	var b strings.Builder
	b.WriteString(cmd)
	for _, d := range docs {
		b.WriteString("\n" + d.Content + d.Name)
	}
	return exec.Command("bash", "-n", "-c", b.String()).Run() == nil
}

// The ranges of the words in s, split at whitespace.
func fieldRanges(s string) [][2]int {
	var out [][2]int
	start := -1
	for i, r := range s {
		switch {
		case unicode.IsSpace(r) && start >= 0:
			out = append(out, [2]int{start, i})
			start = -1
		case !unicode.IsSpace(r) && start < 0:
			start = i
		}
	}
	if start >= 0 {
		out = append(out, [2]int{start, len(s)})
	}
	return out
}

// Where the name of the instruction in text is, if text starts at base in
// the joined text.
func nameAt(text string, base int) (int, int) {
	t := strings.TrimSpace(text)
	ts := base + len(text) - len(strings.TrimLeftFunc(text, unicode.IsSpace))
	return ts, ts + len(reWhitespace.Split(t, 2)[0])
}

// Check the arguments of a node, whose text starts at `base` in the joined
// text, and return the node of the instruction, which is the one inside
// an ONBUILD.
func (f *file) node(node *parser.Node, j *joined, base int, x *expect) *parser.Node {
	text := node.Original
	t := strings.TrimSpace(text)
	ts := base + len(text) - len(strings.TrimLeftFunc(text, unicode.IsSpace))
	parts := reWhitespace.Split(t, 2)
	name := strings.ToLower(parts[0])
	if known[name] {
		x.keywords = append(x.keywords, [2]int{ts, ts + len(parts[0])})
	}
	if len(parts) < 2 {
		return node
	}
	rest := parts[1]
	restAt := ts + len(t) - len(rest)
	argsAt, flags := extractBuilderFlags(rest, f.escape)
	var got []string
	for _, fl := range flags {
		got = append(got, fl.word)
	}
	if !slices.Equal(got, node.Flags) && !(len(got) == 0 && len(node.Flags) == 0) {
		f.report("harness", f.at(j, restAt), "found the flags %q, but BuildKit has %q", got, node.Flags)
		return node
	}
	for _, fl := range flags {
		// tergi doesn't look at the arguments of an instruction it doesn't
		// know.
		if !known[name] {
			break
		}
		// tergi ends a flag at the end of a line, so it starts the
		// arguments somewhere else.
		if _, _, ok := j.source(restAt+fl.start, restAt+fl.end); !ok {
			f.skip("instructions with a flag that goes on over a line")
			x.shell = append(x.shell, [2]int{base, len(j.text)})
			return node
		}
		raw := rest[fl.start:fl.end]
		n := strings.IndexByte(raw, '=')
		if n < 0 {
			n = len(raw)
		}
		if strings.ContainsAny(raw[:n], "'\"") || strings.ContainsRune(raw[:n], rune(f.escape)) {
			f.skip("flags with quotes or escapes in their names")
			continue
		}
		s, e, _ := j.source(restAt+fl.start, restAt+fl.start+n)
		f.want("flag", s, e, "Attribute")
	}
	args := strings.TrimSpace(rest[argsAt:])
	a := restAt + argsAt + len(rest[argsAt:]) - len(strings.TrimLeftFunc(rest[argsAt:], unicode.IsSpace))
	if args == "" {
		return node
	}
	if s, _, ok := j.source(a, a+1); ok && f.kind(s) == "Attribute" {
		f.report("flag", s, "the arguments start with %q, which is an attribute", f.src[s:s+1])
	}

	switch name {
	case "onbuild":
		if node.Next != nil && len(node.Next.Children) == 1 {
			child := node.Next.Children[0]
			if _, _, ok := j.source(nameAt(child.Original, a)); !ok {
				x.stop = true
				return node
			}
			return f.node(child, j, a, x)
		}
		return node
	case "healthcheck":
		if node.Next == nil {
			return node
		}
		typ := node.Next.Value
		if strings.EqualFold(typ, "cmd") || strings.EqualFold(typ, "none") {
			x.keywords = append(x.keywords, [2]int{a, a + len(typ)})
		}
		if !strings.EqualFold(typ, "cmd") {
			return node
		}
		// parseHealthConfig looks at single bytes to skip the spaces
		// after the type.
		n := len(typ)
		for n < len(args) && unicode.IsSpace(rune(args[n])) {
			n++
		}
		a += n
		args = args[n:]
		if args == "" {
			return node
		}
	}

	_, perr := instructions.ParseInstructionWithLinter(node, lint)
	json := node.Attributes["json"]
	switch name {
	case "run", "cmd", "entrypoint", "healthcheck", "shell", "volume", "copy", "add":
		// When tergi takes an array as JSON and BuildKit doesn't, tergi
		// doesn't color its words as BuildKit reads them, and doesn't look
		// for here-documents in it.
		if f.json(j, a, args, json) && !json {
			x.shell = append(x.shell, [2]int{a, len(j.text)})
			x.stop = strings.Contains(args, "<<")
			return node
		}
	}
	lex := shell.NewLex(rune(f.escape))
	switch name {
	case "run", "cmd", "entrypoint", "healthcheck":
		if !json {
			x.shell = append(x.shell, [2]int{a, len(j.text)})
		}
	case "env", "label":
		if perr != nil || slices.ContainsFunc(node.Flags, func(s string) bool { return strings.HasPrefix(s, "--unset") }) {
			break
		}
		words := parseWords(args, f.escape)
		if len(words) == 0 {
			break
		}
		// tergi only looks for the `=` that says which form this is on
		// the first line.
		if _, _, ok := j.source(a+words[0][0], a+words[0][1]); !ok {
			f.skip("ENV and LABEL whose first word goes on over a line")
			break
		}
		if !strings.Contains(args[words[0][0]:words[0][1]], "=") {
			// The old form, `ENV name value`.
			key := reWhitespace.Split(args, 2)[0]
			f.word(j, lex, a, key)
			v := strings.TrimLeft(args[len(key):], "\t\v\f\r ")
			f.word(j, lex, a+len(args)-len(v), v)
			break
		}
		for _, w := range words {
			word := args[w[0]:w[1]]
			eq := strings.IndexByte(word, '=')
			f.word(j, lex, a+w[0], word[:eq])
			f.word(j, lex, a+w[0]+eq+1, word[eq+1:])
		}
	case "arg":
		if perr != nil {
			break
		}
		for _, w := range parseWords(args, f.escape) {
			word := args[w[0]:w[1]]
			if eq := strings.IndexByte(word, '='); eq >= 0 {
				f.word(j, lex, a+w[0]+eq+1, word[eq+1:])
			}
		}
	case "from":
		words := splitWords(args)
		// tergi colors `AS` even when BuildKit rejects the instruction,
		// for example because the stage's name isn't allowed.
		if len(words) > 1 && strings.EqualFold(args[words[1][0]:words[1][1]], "as") {
			as := [2]int{a + words[1][0], a + words[1][1]}
			if perr == nil {
				x.keywords = append(x.keywords, as)
			} else {
				x.maybe = append(x.maybe, as)
			}
		}
		if perr == nil {
			f.word(j, lex, a+words[0][0], args[words[0][0]:words[0][1]])
		}
	case "copy", "add", "expose", "volume":
		if perr != nil || json {
			break
		}
		for _, w := range splitWords(args) {
			word := args[w[0]:w[1]]
			if parser.MustParseHeredoc(word) == nil {
				f.word(j, lex, a+w[0], word)
			}
		}
	case "user", "workdir", "stopsignal":
		if perr == nil {
			f.word(j, lex, a, args)
		}
	}
	return node
}

// The source offset of position a in the joined text, for reports.
func (f *file) at(j *joined, a int) int {
	for _, p := range j.pieces {
		if a < p.at+p.n {
			return p.src + max(a-p.at, 0)
		}
	}
	if len(j.pieces) > 0 {
		p := j.pieces[len(j.pieces)-1]
		return p.src + p.n
	}
	return 0
}

// Check that the arguments at a in the joined text get JSON's colors when
// BuildKit reads them as JSON, and don't otherwise. Return whether tergi
// colors them as JSON.
func (f *file) json(j *joined, a int, args string, json bool) bool {
	if args[0] != '[' {
		return false
	}
	s, _, _ := j.source(a, a+1)
	got := f.kind(s) == "Punctuation"
	if got == json {
		return got
	}
	// tergi only looks at the first line, so it reads an array that's
	// fine on its first line as JSON. That includes a line that goes on at
	// the end of the file, since more lines could follow.
	if !json && (len(j.sources(a, a+len(args))) > 1 || j.open) {
		f.skip("JSON arrays that BuildKit rejects after their first line")
		return got
	}
	if json {
		f.report("JSON", s, "%q is JSON, but tergi colors it as something else", args)
	} else {
		f.report("JSON", s, "%q isn't JSON, but tergi colors it as JSON", args)
	}
	return got
}

// Check the variables in a word that BuildKit expands, which starts at a
// in the joined text.
func (f *file) word(j *joined, lex *shell.Lex, a int, word string) {
	if word == "" {
		return
	}
	want, ok := expands(lex, word)
	if !ok {
		f.skip("words that BuildKit can't expand")
		return
	}
	s, e, ok := j.source(a, a+len(word))
	if !ok {
		f.skip("words split over lines")
		return
	}
	if got := f.vars(s, e); !slices.Equal(got, want) {
		f.report("variables", s, "%q has the variables %q, but tergi marks %q", word, want, got)
	}
}

// Check the body of a here-document in a COPY or an ADD, which is the
// content of a file.
func (f *file) copyBody(node *parser.Node, doc parser.Heredoc, body []line) {
	// tergi lexes the body in another language when it's the only one and
	// the name of the file says what language that is. These are the names
	// it doesn't know, or that it doesn't look at.
	var args []string
	for n := node.Next; n != nil; n = n.Next {
		args = append(args, n.Value)
	}
	dest := args[len(args)-1]
	base := dest[strings.LastIndexByte(dest, '/')+1:]
	plain := !strings.ContainsAny(dest, "$'\"") && !strings.ContainsRune(dest, rune(f.escape))
	if len(node.Heredocs) == 1 && plain && (strings.Contains(base, ".") ||
		slices.Contains([]string{"Dockerfile", "Containerfile", "dockerfile", "containerfile",
			"Pipfile", "PKGBUILD", "APKBUILD"}, base)) {
		f.skip("here-documents that may be in another language")
		return
	}
	var ranges [][2]int
	var content strings.Builder
	for _, l := range body {
		s := l.start
		if doc.Chomp {
			s = l.end - len(bytes.TrimLeft(f.src[l.start:l.end], "\t"))
		}
		ranges = append(ranges, [2]int{s, l.end})
		content.Write(f.src[s:l.next])
	}
	if !doc.Expand {
		for _, r := range ranges {
			f.want("here-document", r[0], r[1], "String")
		}
		return
	}
	// The same as dispatch in dockerfile2llb's convert.go.
	lex := shell.NewLex('\\')
	lex.SkipProcessQuotes = true
	want, ok := expands(lex, content.String())
	if !ok {
		f.skip("here-documents that BuildKit can't expand")
		return
	}
	var got []string
	for _, r := range ranges {
		got = append(got, f.vars(r[0], r[1])...)
	}
	if got = unique(got); !slices.Equal(got, want) {
		at := len(f.src)
		if len(ranges) > 0 {
			at = ranges[0][0]
		}
		f.report("variables", at, "the here-document has the variables %q, but tergi marks %q", want, got)
	}
}

// The words of s as parseStringsWhitespaceDelimited splits it.
func splitWords(s string) [][2]int {
	var words [][2]int
	at := 0
	for _, m := range reWhitespace.FindAllStringIndex(s, -1) {
		words = append(words, [2]int{at, m[0]})
		at = m[1]
	}
	return append(words, [2]int{at, len(s)})
}

type flag struct {
	word       string
	start, end int
}

// extractBuilderFlags from split_command.go, which also returns where
// each flag is, and where the arguments after them start.
func extractBuilderFlags(line string, escape byte) (int, []flag) {
	const (
		inSpaces = iota
		inWord
		inQuote
	)
	var words []flag
	phase := inSpaces
	var sb strings.Builder
	quote := byte(0)
	blankOK := false
	start := 0
	var ch byte
	for pos := 0; pos <= len(line); pos++ {
		if pos != len(line) {
			ch = line[pos]
		}
		if phase == inSpaces {
			if pos == len(line) {
				break
			}
			if unicode.IsSpace(rune(ch)) {
				continue
			}
			if ch != '-' || pos+1 == len(line) || line[pos+1] != '-' {
				return pos, words
			}
			phase = inWord
			start = pos
		}
		if (phase == inWord || phase == inQuote) && pos == len(line) {
			if w := sb.String(); w != "--" && (blankOK || len(w) > 0) {
				words = append(words, flag{w, start, pos})
			}
			break
		}
		if phase == inWord {
			if unicode.IsSpace(rune(ch)) {
				w := sb.String()
				phase = inSpaces
				if w == "--" {
					return pos, words
				}
				if blankOK || len(w) > 0 {
					words = append(words, flag{w, start, pos})
				}
				sb.Reset()
				blankOK = false
				continue
			}
			if ch == '\'' || ch == '"' {
				quote = ch
				blankOK = true
				phase = inQuote
				continue
			}
			if ch == escape {
				if pos+1 == len(line) {
					continue
				}
				pos++
				ch = line[pos]
			}
			// BuildKit writes each byte as a rune of its own.
			sb.WriteRune(rune(ch))
			continue
		}
		if phase == inQuote {
			if ch == quote {
				phase = inWord
				continue
			}
			if ch == escape {
				if pos+1 == len(line) {
					phase = inWord
					continue
				}
				pos++
				ch = line[pos]
			}
			sb.WriteRune(rune(ch))
		}
	}
	return len(line), words
}

// Where parseWords from line_parsers.go finds the words of rest.
func parseWords(rest string, escape byte) [][2]int {
	const (
		inSpaces = iota
		inWord
		inQuote
	)
	var words [][2]int
	phase := inSpaces
	quote := rune(0)
	start := 0
	esc := rune(escape)
	var ch rune
	var width int
	for pos := 0; pos <= len(rest); pos += width {
		if pos != len(rest) {
			ch, width = utf8.DecodeRuneInString(rest[pos:])
		}
		if phase == inSpaces {
			if pos == len(rest) {
				break
			}
			if unicode.IsSpace(ch) {
				continue
			}
			phase = inWord
			start = pos
		}
		if (phase == inWord || phase == inQuote) && pos == len(rest) {
			words = append(words, [2]int{start, pos})
			break
		}
		if phase == inWord {
			if unicode.IsSpace(ch) {
				phase = inSpaces
				words = append(words, [2]int{start, pos})
				continue
			}
			if ch == '\'' || ch == '"' {
				quote = ch
				phase = inQuote
			}
			if ch == esc {
				if pos+width == len(rest) {
					continue
				}
				pos += width
				ch, width = utf8.DecodeRuneInString(rest[pos:])
			}
			continue
		}
		if phase == inQuote {
			if ch == quote {
				phase = inWord
			}
			if ch == esc && quote != '\'' {
				if pos+width == len(rest) {
					phase = inWord
					continue
				}
				pos += width
				ch, width = utf8.DecodeRuneInString(rest[pos:])
			}
		}
	}
	return words
}
