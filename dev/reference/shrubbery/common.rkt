#lang racket/base
;; What `prepare` and `compare` both need to know: how tergi finds the
;; language in a `#lang` line, and which languages it reads as shrubbery.

(provide lang-line
         reader
         lang-line-before?)

;; If `s` starts with a `#lang` line, returns the language's name and the
;; position after it, where the language's own lexer takes over. This
;; follows the `#lang` token in tergi's `racket.rs`, which only counts the
;; name when whitespace or the end of the text comes after it.
(define (lang-line s)
  (define m (regexp-match-positions #px"^#lang ([a-zA-Z0-9+_/-]+)(?=\\s|$)" s))
  (if m
      (values (substring s (caadr m) (cdadr m)) (cdar m))
      (values #f #f)))

;; Like `reader` in tergi's `shrubbery.rs`: 'text for the languages that
;; read the whole file as text with @-forms in it, 'shrubbery for the
;; others in shrubbery notation, and #f for everything else.
(define (reader name)
  (define (under base)
    (or (equal? name base)
        (and (> (string-length name) (string-length base))
             (string=? (substring name 0 (add1 (string-length base)))
                       (string-append base "/")))))
  (cond
    [(or (under "rhombus/scribble") (equal? name "shrubbery/text")) 'text]
    [(or (under "rhombus") (under "shrubbery") (under "shplait")) 'shrubbery]
    [else #f]))

;; Whether tergi might find a `#lang` line in a `.rhm` file, and so read
;; it with the Racket lexer first. It looks for one after whitespace and
;; Racket comments. We only skip whitespace and line comments here, and
;; say yes to anything else that could start a comment or a `#lang` line,
;; which is enough to keep such texts out of the random ones.
(define (lang-line-before? s)
  (let loop ([i 0])
    (cond
      [(= i (string-length s)) #f]
      [(let ([c (string-ref s i)]) (or (char-whitespace? c) (char=? c (integer->char #xFEFF))))
       (loop (add1 i))]
      [(char=? (string-ref s i) #\;)
       (define m (regexp-match-positions #rx"\n" s i))
       (and m (loop (caar m)))]
      [else (regexp-match? #rx"^(#[|!]|#lang)" (substring s i))])))
