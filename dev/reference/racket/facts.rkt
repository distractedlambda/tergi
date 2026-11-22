#lang racket/base
;; Reads the files that the list file on the command line names, and
;; prints one line for each with what the reader found there.
;;
;; Each line of the list is a letter, a tab and a path. The letter says
;; how to read the file:
;;
;;   r  plain Racket, the whole file
;;   p  plain Racket, after the `#lang` line
;;   a  Racket with @-forms, as in `#lang at-exp`, after the `#lang` line
;;   i  a Scribble document, which is text with @-forms in it, after the
;;      `#lang` line
;;   n  one atom on each line, which is a number or a symbol
;;
;; For n, the output is "NUMS" and a space, then a 1 for each atom that
;; the reader reads as a number or fails to read, and a 0 for each that
;; it reads as a symbol. For the others, it's "ERR" if the reader fails,
;; or "OK" and then, separated by commas, each atom as "kind pos span".
;; Positions count characters from 1, and a CRLF counts as one. A hash
;; table or a prefab structure is an atom of the kind "hash" or "prefab"
;; as well, because the keys in it have no positions.
(require (only-in "at-reader.rkt" make-at-readtable read-syntax-inside)
         racket/port racket/string racket/extflonum)

(define (walk stx out)
  (define e (syntax-e stx))
  (define (atom kind) (out kind (syntax-position stx) (syntax-span stx)))
  (cond
    ;; Text in an @-form body. The reader makes a newline and the
    ;; indentation after it out of the source text too, and marks them
    ;; with the 'scribble property instead.
    [(and (string? e) (or (syntax-property stx 'tergi-text)
                          (let ([p (syntax-property stx 'scribble)])
                            (or (eq? p 'indentation) (and (pair? p) (eq? (car p) 'newline))))))
     (atom "text")]
    [(pair? e) (let loop ([e e]) (cond [(pair? e) (walk (car e) out) (loop (cdr e))]
                                        [(syntax? e) (walk e out)]
                                        [else (void)]))]
    [(null? e) (void)]
    [(vector? e) (for ([x e]) (walk x out))]
    [(box? e) (walk (unbox e) out)]
    ;; The key of a prefab structure isn't syntax either.
    [(prefab-struct-key e) (atom "prefab") (for ([x (cdr (vector->list (struct->vector e)))]) (when (syntax? x) (walk x out)))]
    ;; The keys of a hash table aren't syntax objects, so they have no
    ;; position. The values are.
    [(hash? e) (atom "hash") (for ([v (in-hash-values e)]) (when (syntax? v) (walk v out)))]
    [(number? e) (atom "number")]
    [(extflonum? e) (atom "number")]
    [(string? e) (atom "string")]
    [(bytes? e) (atom "string")]
    [(char? e) (atom "char")]
    [(boolean? e) (atom "bool")]
    [(keyword? e) (atom "keyword")]
    [(symbol? e) (atom "symbol")]
    [(regexp? e) (atom "string")]
    [(byte-regexp? e) (atom "string")]
    [else (atom "other")]))

(define (read-all in out)
  (let loop ()
    (define s (read-syntax 'x in))
    (unless (eof-object? s) (walk s out) (loop))))

(define (atoms mode path)
  (call-with-input-file path
    (lambda (in)
      (port-count-lines! in)
      (unless (eqv? mode #\r)
        (read-line in 'linefeed))
      (with-handlers ([(λ (e) #t) (λ (e) #f)])
        (define acc '())
        (define (out k p s) (set! acc (cons (format "~a ~a ~a" k p s) acc)))
        (case mode
          [(#\r #\p)
           (parameterize ([read-accept-reader #f] [read-accept-lang #f])
             (read-all in out))]
          [(#\i)
           (walk (read-syntax-inside 'x in) out)]
          [(#\a)
           (parameterize ([current-readtable (make-at-readtable #:datum-readtable 'dynamic
                                                                #:command-readtable 'dynamic)]
                          [read-accept-reader #f] [read-accept-lang #f])
             (read-all in out))])
        (reverse acc)))))

(define (numbers path)
  (for/list ([l (call-with-input-file path port->lines)])
    (define r (with-handlers ([exn:fail:read? (lambda (e) 'error)])
                (read (open-input-string l))))
    (cond [(eq? r 'error) "1"]
          [(or (number? r) (extflonum? r)) "1"]
          [(symbol? r) "0"]
          [else (error 'numbers "~s reads as ~s" l r)])))

(for ([line (call-with-input-file (vector-ref (current-command-line-arguments) 0) port->lines)])
  (define mode (string-ref line 0))
  (define path (substring line 2))
  (displayln
   (if (eqv? mode #\n)
       (string-append "NUMS " (string-append* (numbers path)))
       (let ([result (atoms mode path)])
         (if result (string-join (cons "OK" result) ",") "ERR")))))
