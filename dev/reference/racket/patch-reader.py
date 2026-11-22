"""Changes the @-reader so that it tells where each piece of text is.

Reads scribble/reader.rkt from at-exp-lib on stdin and writes the changed
copy to stdout. Each change must apply exactly once, so that a different
version of the reader fails loudly instead of being checked wrongly.
"""

import sys

CHANGES = [
    # The reader joins neighboring pieces of text into one string, which
    # loses where each came from. Never join them.
    (
        """      (if (and (string? 1st) (not (eol-syntax? stx0))
               (string? 2nd) (not (eol-syntax? stx)))""",
        """      (if #f""",
    ),
    # Mark the syntax objects made from the text of an @-form body, so
    # that facts.rkt can tell text from strings in the Racket code.
    (
        """      (define (make-stx sexpr)
        (datum->syntax #f
          (if (bytes? sexpr) (bytes->string/utf-8 sexpr) sexpr)
          (vector source-name line col pos (span-from pos))
          orig-stx))""",
        """      (define (make-stx sexpr)
        (syntax-property
         (datum->syntax #f
          (if (bytes? sexpr) (bytes->string/utf-8 sexpr) sexpr)
          (vector source-name line col pos (span-from pos))
          orig-stx)
         'tergi-text #t))""",
    ),
    # `command-bar` gives a `|...|` symbol a span in bytes, but spans count
    # characters, so a symbol with a non-ASCII character in it would cover
    # what comes after it. This is a bug in the reader.
    (
        "(add1 (bytes-length (car m)))",
        "(add1 (string-length (bytes->string/utf-8 (car m))))",
    ),
]

text = sys.stdin.read()
for old, new in CHANGES:
    if text.count(old) != 1:
        sys.exit(f"patch-reader.py: this text isn't in the reader once:\n{old}")
    text = text.replace(old, new)
sys.stdout.write(text)
