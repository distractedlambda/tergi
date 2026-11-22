/* Prints the tokens of CMake's own lexer, cmListFileLexer.c, for each
   file named on a line of standard input. Each file gets a block: "FILE
   path", then a line "type startline startcolumn endline endcolumn
   length" for each token, and "END". The end is where the lexer stands
   after the token. Identifiers and unquoted arguments also get a line
   "T text", because the parser needs their text. A file that can't be
   opened gets "ERROR". */
#include "cmListFileLexer.c"

int main(void)
{
  char path[4096];
  while (fgets(path, sizeof path, stdin)) {
    path[strcspn(path, "\n")] = 0;
    cmListFileLexer* lexer = cmListFileLexer_New();
    cmListFileLexer_BOM bom;
    printf("FILE %s\n", path);
    if (!cmListFileLexer_SetFileName(lexer, path, &bom)) {
      printf("ERROR\n");
      cmListFileLexer_Delete(lexer);
      continue;
    }
    cmListFileLexer_Token* t;
    while ((t = cmListFileLexer_Scan(lexer))) {
      printf("%d %d %d %d %d %d\n", (int)t->type, t->line, t->column,
             lexer->line, lexer->column, (int)t->length);
      if (t->type == cmListFileLexer_Token_Identifier ||
          t->type == cmListFileLexer_Token_ArgumentUnquoted) {
        printf("T ");
        fwrite(t->text, 1, t->length, stdout);
        printf("\n");
      }
    }
    printf("END\n");
    cmListFileLexer_Delete(lexer);
  }
  return 0;
}
