/* Stands in for KWSys's Encoding.h. cmListFileLexer.c only uses it on
   Windows, to open files with wide names. */
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
static wchar_t* cmsysEncoding_DupToWide(const char* s)
{
  size_t n = strlen(s) + 1;
  wchar_t* w = malloc(n * sizeof(wchar_t));
  mbstowcs(w, s, n);
  return w;
}
