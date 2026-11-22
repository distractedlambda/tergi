#!/usr/bin/env python3
"""Patches bash 5.3 so that it reports where the tokens it reads are.

With TERGI_TOKENS set to a path, the patched bash writes a line there for
each token that read_token returns, as its start, its end and its name,
such as `4 6 IF` or `9 10 ';'`. The offsets are in bytes of the script.
It also reports each comment as COMMENT, the body of each here-document
as HEREDOC_BODY, and the line that ends one as HEREDOC_END.

Bash reads the script a line at a time into shell_input_line. The patch
counts the bytes that yy_getc reads from the file, remembers where the
current line started, and adds shell_input_line_index to that. Bash also
parses strings that aren't in the file, such as the text of an
arithmetic `for` after it read it, and the patch reports nothing while it
does. It doesn't report a token that starts in such a string either.

Usage: instrument.py <bash source directory>
"""

import sys

root = sys.argv[1]


def edit(path, old, new):
    s = open(path).read()
    assert s.count(old) == 1, (path, old)
    open(path, "w").write(s.replace(old, new))


parse_y = f"{root}/parse.y"

edit(parse_y, "static int read_token (int);", r"""static int read_token (int);
static int read_token_1 (int);
static int tergi_reading_file (void);
static const char *tergi_name (int);

/* The number of bytes read from the script, where the line in
   shell_input_line starts, and where the token being read starts. */
long tergi_consumed = 0;
static long tergi_line_offset = 0;
static long tergi_no_start = -1;
static long *tergi_start = &tergi_no_start;
/* Set when bash pushes text back into shell_input_line, which moves the
   positions until it reads the next line. */
static int tergi_lost = 0;
#define TERGI_POS() (tergi_line_offset + (long)shell_input_line_index)

void
tergi_lose_line (void)
{
  tergi_lost = 1;
}

void
tergi_emit (long s, long e, const char *name)
{
  static FILE *out = NULL;
  static int opened = 0;

  if (!opened)
    {
      char *p = getenv ("TERGI_TOKENS");
      opened = 1;
      out = p ? fopen (p, "w") : NULL;
    }
  if (out && s >= 0 && tergi_reading_file () && !tergi_lost)
    {
      fprintf (out, "%ld %ld %s\n", s, e, name);
      fflush (out);
    }
}""")

edit(parse_y, """static int
yy_getc (void)
{
#ifndef __MSYS__
  return (*(bash_input.getter)) ();""", """static int
yy_getc (void)
{
#ifndef __MSYS__
  int c = (*(bash_input.getter)) ();
  if (c != EOF && bash_input.type == st_bstream)
    tergi_consumed++;
  return c;""")

edit(parse_y, """static int
yy_ungetc (int c)
{""", """static int
yy_ungetc (int c)
{
  if (bash_input.type == st_bstream)
    tergi_consumed--;""")

edit(parse_y, """      i = truncating = 0;
      shell_input_line_terminator = 0;""", """      i = truncating = 0;
      shell_input_line_terminator = 0;
      if (tergi_reading_file ())
        {
          tergi_line_offset = tergi_consumed;
          tergi_lost = 0;
        }""")

# Wrap read_token, so that the tokens of the conditional commands that
# parse_cond_command reads are reported too. Tokens that bash made up
# rather than read, such as the one that starts a command substitution,
# have no position.
edit(parse_y, """static int
read_token (int command)
{
  int character;""", """static int
read_token (int command)
{
  int pending, r;
  long start, *outer;

  pending = token_to_read;
  start = -1;
  outer = tergi_start;
  tergi_start = &start;
  r = read_token_1 (command);
  tergi_start = outer;
  if (pending == 0 && command != RESET && r > 0 && r != COND_CMD
      && r != DOLPAREN && r != DOLBRACE && r != yacc_EOF)
    tergi_emit (start, TERGI_POS (), tergi_name (r));
  return r;
}

static int
read_token_1 (int command)
{
  int character;""")

edit(parse_y, """  while ((character = shell_getc (1)) != EOF && shellblank (character))
    ;
""", """  while ((character = shell_getc (1)) != EOF && shellblank (character))
    ;
  *tergi_start = tergi_reading_file () && !tergi_lost ? TERGI_POS () - 1 : -1;
""")

edit(parse_y, """      character = discard_until ('\\n');
      if (character == EOF)
	{
	  parser_state &= ~PST_COMMENT;
	  EOF_Reached = 1;
	  return (yacc_EOF);		/* XXX */
	}
      shell_getc (0);""", """      character = discard_until ('\\n');
      tergi_emit (*tergi_start, character == EOF ? tergi_consumed : TERGI_POS (), "COMMENT");
      if (character == EOF)
	{
	  parser_state &= ~PST_COMMENT;
	  EOF_Reached = 1;
	  return (yacc_EOF);		/* XXX */
	}
      shell_getc (0);
      *tergi_start = tergi_reading_file () && !tergi_lost ? TERGI_POS () - 1 : -1;""")

names = """IF THEN ELSE ELIF FI CASE ESAC FOR SELECT WHILE UNTIL DO DONE FUNCTION
COPROC COND_START COND_END COND_ERROR IN BANG TIME TIMEOPT TIMEIGN WORD
ASSIGNMENT_WORD REDIR_WORD NUMBER ARITH_CMD ARITH_FOR_EXPRS COND_CMD AND_AND
OR_OR GREATER_GREATER LESS_LESS LESS_AND LESS_LESS_LESS GREATER_AND
SEMI_SEMI SEMI_AND SEMI_SEMI_AND LESS_LESS_MINUS AND_GREATER
AND_GREATER_GREATER LESS_GREATER GREATER_BAR BAR_AND""".split()
cases = "".join(f'    case {n}: return "{n}";\n' for n in names)
with open(parse_y, "a") as f:
    f.write(r"""
static int
tergi_reading_file (void)
{
  return bash_input.type == st_bstream && pushed_string_list == NULL;
}

/* The name of a token, or the character in quotes, with N for a newline. */
static const char *
tergi_name (int t)
{
  static char buf[16];

  switch (t)
    {
""" + cases + r"""    }
  if (t > 0 && t < 256)
    snprintf (buf, sizeof buf, "'%c'", t == '\n' ? 'N' : t);
  else
    snprintf (buf, sizeof buf, "%d", t);
  return buf;
}
""")

make_cmd_c = f"{root}/make_cmd.c"

edit(make_cmd_c, """void
make_here_document (REDIRECT *temp, int lineno)
{""", """extern long tergi_consumed;
extern void tergi_emit (long, long, const char *);
extern void tergi_lose_line (void);

void
make_here_document (REDIRECT *temp, int lineno)
{
  long tergi_body = tergi_consumed, tergi_line = tergi_consumed;""")

edit(make_cmd_c, """	  if (STREQN (line, redir_word, redir_len) && line[redir_len] == '\\n')
	    break;""", """	  if (STREQN (line, redir_word, redir_len) && line[redir_len] == '\\n')
	    {
	      tergi_emit (tergi_line, tergi_consumed, "HEREDOC_END");
	      break;
	    }""")

edit(make_cmd_c, """      if (*line == 0)
	continue;

      if (STREQN (line, redir_word, redir_len) && line[redir_len] == '\\n')
	break;""", """      if (*line == 0)
	{
	  tergi_line = tergi_consumed;
	  continue;
	}

      if (STREQN (line, redir_word, redir_len) && line[redir_len] == '\\n')
	{
	  tergi_emit (tergi_line, tergi_consumed, "HEREDOC_END");
	  break;
	}""")

# A line such as `EOF)` ends a here-document inside a command
# substitution, and bash pushes the rest of the line back.
edit(make_cmd_c, """	  shell_ungets (line + redir_len);""", """	  tergi_emit (tergi_body, tergi_line, "HEREDOC_BODY");
	  tergi_lose_line ();
	  shell_ungets (line + redir_len);""")

edit(make_cmd_c, """      FASTCOPY (line, document + document_index, len);
      document_index += len;
    }
""", """      FASTCOPY (line, document + document_index, len);
      document_index += len;
      tergi_line = tergi_consumed;
    }
  tergi_emit (tergi_body, tergi_line, "HEREDOC_BODY");
""")
