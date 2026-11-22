// Compares tergi's LLVM IR lexer with LLVM's own lexer, LLLexer.
//
// Usage: lexcheck <work>
//
// It reads the paths in <work>/files.txt and tergi's tokens for the file on
// line n from <work>/tokens/<n>.tok. It runs LLLexer over each file and
// turns its tokens into a class for each byte, using the color rules of
// tergi's lexer. Then it turns tergi's tokens into classes the same way and
// compares the two.
//
// Classes: '.' plain, c comment, s string, e escape, n number, k constant,
// K keyword, t type, o operator, p punctuation, I/i identifier start/rest.
// Marking where each identifier starts checks that tergi splits names the
// way LLVM does, since tergi colors each identifier by its whole name.
//
// The program needs LLLexer's private members, so it's built against a copy
// of LLLexer.h where everything is public.

#include "llvm/AsmParser/LLLexer.h"

#include "llvm/AsmParser/LLToken.h"
#include "llvm/IR/LLVMContext.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/SourceMgr.h"
#include <fstream>
#include <iostream>
#include <map>
#include <sstream>
#include <vector>

using namespace llvm;

static LLVMContext Ctx;

// Set when LLLexer rejects the name of a label, such as the `line` in
// `line: 3`, that we lex on its own to color it like a field.
static bool labelFailed = false;

struct Tok {
  lltok::Kind kind;
  size_t start, end;
};

static bool isWs(char c) {
  return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' ||
         c == '\f';
}

static void fill(std::string &out, size_t s, size_t e, char c) {
  for (size_t i = s; i < e; i++)
    out[i] = c;
}

static void ident(std::string &out, size_t s, size_t e) {
  if (s >= e)
    return;
  out[s] = 'I';
  fill(out, s + 1, e, 'i');
}

// Comments in a gap between tokens. Block comments don't color the
// whitespace at the end of their lines.
static void gap(const std::string &full, size_t s, size_t e,
                std::string &out) {
  size_t i = s;
  while (i < e) {
    char c = full[i];
    if (c == ';') {
      size_t j = i;
      while (j < e && full[j] != '\n' && full[j] != '\r')
        j++;
      fill(out, i, j, 'c');
      i = j;
    } else if (c == '/' && i + 1 < e && full[i + 1] == '*') {
      size_t j = full.find("*/", i + 2);
      j = (j == std::string::npos || j + 2 > e) ? e : j + 2;
      size_t ls = i;
      while (ls < j) {
        size_t le = full.find('\n', ls);
        if (le == std::string::npos || le > j)
          le = j;
        size_t te = le;
        while (te > ls && isWs(full[te - 1]))
          te--;
        fill(out, ls, te, 'c');
        ls = le + 1;
      }
      i = j;
    } else {
      i++;
    }
  }
}

static bool isHex(char c) { return isxdigit((unsigned char)c); }

static void str(const std::string &full, size_t s, size_t e,
                std::string &out) {
  fill(out, s, e, 's');
  for (size_t i = s; i < e; i++) {
    if (full[i] != '\\')
      continue;
    if (i + 1 < e && full[i + 1] == '\\') {
      fill(out, i, i + 2, 'e');
      i++;
    } else if (i + 2 < e && isHex(full[i + 1]) && isHex(full[i + 2])) {
      fill(out, i, i + 3, 'e');
      i += 2;
    }
  }
}

static bool classify(const std::string &full, size_t base, size_t end,
                     std::string &out);

static void label(const std::string &full, size_t s, size_t colon,
                  bool inParens, std::string &out, bool &ok) {
  size_t ls = s;
  while (ls > 0 && full[ls - 1] != '\n')
    ls--;
  bool start = true;
  for (size_t i = ls; i < s; i++)
    if (full[i] != ' ' && full[i] != '\t')
      start = false;
  size_t le = full.find('\n', colon + 1);
  if (le == std::string::npos)
    le = full.size();
  size_t r = colon + 1;
  while (r < le && isWs(full[r]))
    r++;
  bool blank = r == le || full[r] == ';' || !inParens;
  if (start && blank)
    ident(out, s, colon);
  else if (!classify(full, s, colon, out)) {
    ok = false;
    labelFailed = true;
  }
  out[colon] = 'p';
}

static bool classify(const std::string &full, size_t base, size_t end,
                     std::string &out) {
  std::string sub = full.substr(base, end - base);
  SourceMgr SM;
  SM.AddNewSourceBuffer(MemoryBuffer::getMemBuffer(sub, "x", true), SMLoc());
  SMDiagnostic Err;
  StringRef buf = SM.getMemoryBuffer(1)->getBuffer();
  LLLexer L(buf, SM, Err, Ctx);
  std::vector<Tok> toks;
  while (true) {
    lltok::Kind k = L.Lex();
    if (k == lltok::Eof)
      break;
    bool sub = !(base == 0 && end == full.size());
    if (k == lltok::Error && sub &&
        (isalpha((unsigned char)*L.TokStart) || *L.TokStart == '_')) {
      // A field name that isn't a keyword, such as `scope`.
      const char *p = L.TokStart;
      while (isalnum((unsigned char)*p) || *p == '_')
        p++;
      L.CurPtr = p;
      toks.push_back({lltok::kw_x, base + (L.TokStart - buf.data()),
                      base + (p - buf.data())});
      continue;
    }
    if (k == lltok::Error)
      return false;
    size_t s = L.TokStart - buf.data(), e = L.CurPtr - buf.data();
    toks.push_back({k, base + s, base + e});
  }
  if (L.ErrorInfo.Priority != LLLexer::ErrorPriority::None)
    return false;
  bool ok = true;
  size_t prev = base;
  size_t parens = 0;
  for (size_t n = 0; n < toks.size(); n++) {
    Tok t = toks[n];
    gap(full, prev, t.start, out);
    prev = t.end;
    switch (t.kind) {
    case lltok::kw_true:
    case lltok::kw_false:
    case lltok::kw_null:
    case lltok::kw_none:
    case lltok::kw_undef:
    case lltok::kw_poison:
    case lltok::kw_zeroinitializer:
    case lltok::DwarfTag:
    case lltok::DwarfAttEncoding:
    case lltok::DwarfVirtuality:
    case lltok::DwarfLang:
    case lltok::DwarfSourceLangName:
    case lltok::DwarfLangDialect:
    case lltok::DwarfCC:
    case lltok::EmissionKind:
    case lltok::NameTableKind:
    case lltok::FixedPointKind:
    case lltok::DwarfOp:
    case lltok::DIFlag:
    case lltok::DISPFlag:
    case lltok::DwarfMacinfo:
    case lltok::ChecksumKind:
    case lltok::DwarfEnumKind:
      fill(out, t.start, t.end, 'k');
      break;
    case lltok::Type:
      fill(out, t.start, t.end, 't');
      break;
    case lltok::FloatLiteral:
    case lltok::FloatHexLiteral:
    case lltok::APFloat:
    case lltok::APSInt:
      fill(out, t.start, t.end, 'n');
      break;
    case lltok::StringConstant:
      str(full, t.start, t.end, out);
      break;
    case lltok::LocalVar:
    case lltok::LocalVarID:
      ident(out, t.start + 1, t.end);
      break;
    case lltok::GlobalVar:
    case lltok::GlobalID:
    case lltok::ComdatVar:
    case lltok::AttrGrpID:
    case lltok::SummaryID:
      ident(out, t.start, t.end);
      break;
    case lltok::MetadataVar:
      if (t.end < end && full[t.end] == '(')
        fill(out, t.start, t.end, 'K');
      else
        ident(out, t.start, t.end);
      break;
    case lltok::exclaim:
      // `!0` names a node.
      if (n + 1 < toks.size() && toks[n + 1].kind == lltok::APSInt &&
          toks[n + 1].start == t.end && isdigit((unsigned char)full[t.end])) {
        bool digits = true;
        for (size_t i = toks[n + 1].start; i < toks[n + 1].end; i++)
          digits &= isdigit((unsigned char)full[i]) != 0;
        if (digits) {
          ident(out, t.start, toks[n + 1].end);
          prev = toks[n + 1].end;
          n++;
          break;
        }
      }
      out[t.start] = 'p';
      break;
    case lltok::lparen:
      parens++;
      out[t.start] = 'p';
      break;
    case lltok::rparen:
      if (parens > 0)
        parens--;
      out[t.start] = 'p';
      break;
    case lltok::equal:
    case lltok::star:
    case lltok::bar:
      fill(out, t.start, t.end, 'o');
      break;
    case lltok::dotdotdot:
    case lltok::lsquare:
    case lltok::rsquare:
    case lltok::lbrace:
    case lltok::rbrace:
    case lltok::less:
    case lltok::greater:
    case lltok::comma:
    case lltok::colon:
    case lltok::hash:
      fill(out, t.start, t.end, 'p');
      break;
    case lltok::LabelStr:
    case lltok::LabelID:
      label(full, t.start, t.end - 1, parens > 0, out, ok);
      break;
    default:
      fill(out, t.start, t.end, 'K');
      break;
    }
  }
  gap(full, prev, end, out);
  return ok;
}

static std::string readFile(const std::string &path, bool &found) {
  std::ifstream f(path, std::ios::binary);
  found = bool(f);
  std::stringstream ss;
  ss << f.rdbuf();
  return ss.str();
}

// Turns tergi's tokens into classes. Kinds that LLVM IR never uses get '?'.
static std::string tergiClasses(const std::string &tok, size_t size) {
  static const std::map<std::string, char> classes = {
      {"Comment", 'c'}, {"String", 's'},      {"Escape", 'e'},
      {"Number", 'n'},  {"Constant", 'k'},    {"Keyword", 'K'},
      {"Type", 't'},    {"Operator", 'o'},    {"Punctuation", 'p'},
      {"Identifier", 'i'}};
  std::string out(size, '.');
  std::istringstream in(tok);
  size_t s, e;
  std::string kind;
  while (in >> s >> e >> kind) {
    auto it = classes.find(kind);
    char c = it == classes.end() ? '?' : it->second;
    e = std::min(e, size);
    fill(out, s, e, c);
    if (c == 'i' && s < e)
      out[s] = 'I';
  }
  return out;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    std::cerr << "usage: lexcheck <work>\n";
    return 2;
  }
  std::string work = argv[1];
  std::ifstream list(work + "/files.txt");
  if (!list) {
    std::cerr << "can't read " << work << "/files.txt\n";
    return 2;
  }
  const size_t shown = 30;
  size_t n = 0, compared = 0, bytes = 0, differ = 0, lexErrors = 0, nul = 0,
         notUtf8 = 0, labelErrors = 0;
  std::string line;
  for (; std::getline(list, line); n++) {
    // A line can start with a language name and a tab.
    std::string path = line.substr(line.find('\t') + 1);
    bool found;
    std::string text = readFile(path, found);
    if (!found) {
      std::cerr << "can't read " << path << "\n";
      return 2;
    }
    // LLLexer reads a NUL byte as the end of the text.
    if (text.find('\0') != std::string::npos) {
      nul++;
      continue;
    }
    std::string exp(text.size(), '.');
    labelFailed = false;
    if (!classify(text, 0, text.size(), exp)) {
      if (labelFailed) {
        // This is a problem in this program, not in either lexer.
        labelErrors++;
        std::cout << path << ": LLLexer rejects the name of a label\n";
      } else {
        // Many tests feed LLVM broken IR on purpose to check its error
        // messages. There is nothing to compare with in those files.
        lexErrors++;
      }
      continue;
    }
    std::string tok =
        readFile(work + "/tokens/" + std::to_string(n) + ".tok", found);
    if (!found) {
      // tergi writes no tokens for files that aren't UTF-8.
      notUtf8++;
      continue;
    }
    compared++;
    bytes += text.size();
    std::string got = tergiClasses(tok, text.size());
    if (got == exp)
      continue;
    differ++;
    if (differ > shown)
      continue;
    size_t i = 0;
    while (got[i] == exp[i])
      i++;
    size_t ls = text.rfind('\n', i ? i - 1 : 0);
    ls = (i == 0 || ls == std::string::npos) ? 0 : ls + 1;
    size_t le = text.find('\n', i);
    if (le == std::string::npos)
      le = text.size();
    size_t lineNo = 1;
    for (size_t j = 0; j < ls; j++)
      lineNo += text[j] == '\n';
    std::cout << path << ":" << lineNo << ":" << i - ls + 1 << "\n"
              << "  text  " << text.substr(ls, le - ls) << "\n"
              << "  llvm  " << exp.substr(ls, le - ls) << "\n"
              << "  tergi " << got.substr(ls, le - ls) << "\n";
  }
  if (differ > 0)
    std::cout << "Classes: . plain, c comment, s string, e escape, n number, "
                 "k constant, K keyword, t type, o operator, p punctuation, "
                 "I/i identifier start/rest\n";
  if (differ > shown)
    std::cout << "... and " << differ - shown << " more files\n";
  std::cout << n << " files: " << compared << " compared (" << bytes
            << " bytes), " << differ << " disagree.\n"
            << "Skipped " << lexErrors << " where LLLexer reports an error, "
            << nul << " with NUL bytes and " << notUtf8
            << " that aren't UTF-8.\n";
  if (labelErrors > 0)
    std::cout << labelErrors << " files have labels this program can't lex.\n";
  if (compared == 0) {
    std::cout << "No files were compared.\n";
    return 1;
  }
  return differ > 0 || labelErrors > 0 ? 1 : 0;
}
