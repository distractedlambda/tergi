"""Random texts for the @-reader.

Each text comes with a letter for how it is read: "a" for `#lang at-exp`,
"i" for a Scribble document, which is text from the start, and "r" for
plain Racket, where `@` is an ordinary character.
"""

import random

R = random.Random()
ch = R.choice

ATOMS = ["1", "-2.5", "1/2", "+i", "#x1F", "1e3", "1@2", "foo", "a-b", "x?", "λ", "a@b", "|a b|", "a\\ b",
         "...", '"s"', '"a\\nb"', '#"by"', '"@{}|"', "#\\a", "#\\space", "#\\(", "#\\@", "#\\|", "#t", "#false",
         "#:k", "#:|a b|", "#:a@b", "x|y|z", "#%app", "#rx\"a|b\"", "a\\|b", "1.", "abc", "q", "@"]
SOUP = ["@", "@;", "{", "}", "[", "]", "|", "|{", "}|", "|<{", "}>|", "|<@", "(", ")", "\"", "#;", ";", "\n",
        " ", "x", "'", ",@", "@|", "\\", "#|", "|#", "1", "#:", "\r\n", "\t", "@'", "@@", "|(", ")|"]
WORDS = ["hello", "x", "1", "a.b", "#t", "(", ")", "[", "]", "\"", "'", ";", "#;", "#|", "|#", "|", "\\", ",", "#:k",
         "  ", " ", "\t", "λ", "-", "..."]

def racket(d):
    r = R.random()
    if d > 4 or r < 0.35: return ch(ATOMS)
    if r < 0.5: return "(" + " ".join(racket(d+1) for _ in range(R.randint(0, 3))) + ")"
    if r < 0.55: return "[" + " ".join(racket(d+1) for _ in range(R.randint(0, 2))) + "]"
    if r < 0.6: return "'" + racket(d+1)
    if r < 0.65: return "#;" + ch(["", " ", "\n"]) + racket(d+1) + " " + racket(d+1)
    if r < 0.68: return "; c " + ch(["", "@x{", "}"]) + "\n" + racket(d+1)
    if r < 0.7: return "#| c @x{ |# " + racket(d+1)
    return at_form(d+1, False)

def body(d, alt):
    if alt is None:
        return "{" + text(d, None) + "}"
    return alt + text(d, alt) + mirror(alt)

def mirror(o):
    m = {"(": ")", ")": "(", "[": "]", "]": "[", "{": "}", "}": "{", "<": ">", ">": "<"}
    return "".join(m.get(c, c) for c in reversed(o))

def at_form(d, in_text, pfx=""):
    s = pfx + "@"
    r = R.random()
    if r < 0.08:
        return s + ";" + ch(["{" + text(d+1, None) + "}", "|<{" + text(d+1, "|<{") + "}>|", " line @x{ \n", "line"]) 
    if R.random() < 0.15: s += ch(["'", "`", ",", "#'", ",@", "''"])
    alt = ch([None, None, None, "|{", "|<{", "|(-{"])
    r = R.random()
    if r < 0.15: return s + body(d+1, alt)
    if r < 0.25: return s + "[" + " ".join(racket(d+1) for _ in range(R.randint(0, 2))) + "]" + (body(d+1, alt) if R.random() < 0.5 else "")
    if r < 0.33: return s + "|" + " ".join(racket(d+1) for _ in range(R.randint(1, 1 if not in_text else 2))) + "|"
    cmd = ch(["foo", "foo", "bar-baz", "x@y", "(f 1)", '"s"', "1", "#:k", "a\\ b", "'q", "|a|"]) if R.random() < 0.95 else at_form(d+1, False)
    s += cmd
    if R.random() < 0.4: s += "[" + " ".join(racket(d+1) for _ in range(R.randint(0, 2))) + "]"
    if R.random() < 0.6: s += body(d+1, alt)
    return s

def text(d, alt):
    out = []
    for _ in range(R.randint(0, 6 if d < 4 else 2)):
        r = R.random()
        if r < 0.4: out.append(ch(WORDS))
        elif r < 0.5: out.append("\n" + ch(["", "  "]))
        elif r < 0.58 and d < 5:
            out.append((alt if alt else "{") + text(d+1, alt) + (mirror(alt) if alt else "}"))
        elif r < 0.62 and alt: out.append(ch(["{", "}", "@x", "@;", "|{"]))
        elif d < 5:
            pfx = alt[:-1] if alt else ""
            if R.random() < 0.15: out.append(pfx + "@\"" + ch(["a", "@", "}"]) + "\"")
            elif R.random() < 0.15: out.append(pfx + "@|" + racket(d+1) + ch(["", " " + racket(d+1)]) + "|")
            else: out.append(at_form(d+1, True, pfx))
    return "".join(out)

def soup():
    return "".join(ch(SOUP + WORDS + ATOMS) for _ in range(R.randint(1, 12)))


def generate(seed, n):
    """Returns n pairs of a letter and a text, made from the seed."""
    R.seed(seed)
    texts = []
    for i in range(n):
        mode = ch("air")
        if mode == "i":
            t = text(0, None)
            if R.random() < 0.3: t = t.replace("}", "", 1) + ch(["}", "{", ""])
        else:
            t = " ".join(racket(0) for _ in range(R.randint(1, 4)))
        if R.random() < 0.25:
            k = R.randint(0, len(t)); t = t[:k] + soup() + t[k:]
        if R.random() < 0.1: t = soup()
        texts.append((mode, t))
    return texts
