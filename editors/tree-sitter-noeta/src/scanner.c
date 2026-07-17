#include "tree_sitter/parser.h"
#include <stdbool.h>

// External scanner for Noeta. Three tokens:
//   * BLOCK_COMMENT   — `/* ... */`, *nestable* (a plain regex token cannot express nesting).
//   * NEWLINE         — an automatic statement terminator. Noeta terminates a statement at a
//                       newline, EXCEPT when the statement is syntactically incomplete: a trailing
//                       operator (handled by the parser — after a dangling operator a terminator is
//                       not valid, so we are never asked) or a *leading* continuation on the next
//                       line (`.`, `|>`, a binary operator), which we detect by peeking here.
//                       No bracket depth is tracked here, deliberately: suppression inside a
//                       multi-line `(...)`/`[...]` falls out of the grammar (a terminator is only
//                       *valid* in statement positions, i.e. inside `{ }` blocks at any depth,
//                       never inside an argument list), which makes termination brace-relative by
//                       construction — the same depth story as the compiler's
//                       `noeta_lexer::newline_boundaries` (terminator-barrier change): `a`
//                       newline `(n)` is two statements at every nesting level, including inside
//                       a bracket-nested closure body, while a newline inside `(...)` relative to
//                       the innermost `{` never terminates. Pinned by test/corpus/termination.txt.
//   * TEXT_BODY       — the verbatim body of a text-tier block (`@doc { … }`): everything up to
//                       the balancing `}`, counted exactly like the compiler lexer's
//                       `matching_brace` — braces nest, `\{`/`\}` are literal braces (not counted)
//                       and `\\` a literal backslash; every other backslash is plain prose. The
//                       closing `}` is left for the grammar.

enum TokenType {
  BLOCK_COMMENT,
  NEWLINE,
  TEXT_BODY,
};

void *tree_sitter_noeta_external_scanner_create(void) { return NULL; }
void tree_sitter_noeta_external_scanner_destroy(void *payload) {}
unsigned tree_sitter_noeta_external_scanner_serialize(void *payload, char *buffer) { return 0; }
void tree_sitter_noeta_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {}

static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }
static void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

// A next-line character that continues the previous statement rather than terminating it —
// mirroring Noeta's leading-continuation rule (a line beginning with `.`, `|>`, or a binary
// operator folds into the statement above). `/` is deliberately excluded: a leading `//` or `/*`
// is a comment, not a division, so a newline before it still terminates.
static bool is_continuation(int32_t c) {
  switch (c) {
    case '.': case '|': case '?': case '+': case '-': case '*':
    case '%': case '<': case '>': case '=': case '!': case '&':
    case '^': case '~':
      return true;
    default:
      return false;
  }
}

bool tree_sitter_noeta_external_scanner_scan(void *payload, TSLexer *lexer,
                                             const bool *valid_symbols) {
  // Verbatim text-tier body — checked before anything else so prose that *looks* like a comment
  // or code stays prose. TEXT_BODY is only grammatically valid right after a text block's `{`,
  // where no terminator is; NEWLINE also being valid means error recovery (everything valid),
  // where emitting a raw token would swallow real code.
  if (valid_symbols[TEXT_BODY] && !valid_symbols[NEWLINE]) {
    unsigned depth = 1;
    bool consumed = false;
    for (;;) {
      if (lexer->eof(lexer)) return false; // unterminated block — let recovery handle it
      int32_t c = lexer->lookahead;
      if (c == '\\') {
        advance(lexer);
        int32_t n = lexer->lookahead;
        if (n == '{' || n == '}' || n == '\\') advance(lexer);
        consumed = true;
        continue;
      }
      if (c == '}') {
        if (depth == 1) break; // the block's own closer — not part of the body
        depth--;
      } else if (c == '{') {
        depth++;
      }
      advance(lexer);
      consumed = true;
    }
    if (!consumed) return false; // empty body — `optional(text_body)` lets `}` close directly
    lexer->result_symbol = TEXT_BODY;
    lexer->mark_end(lexer);
    return true;
  }

  bool saw_newline = false;

  // Consume inline whitespace and newlines, remembering whether a line boundary was crossed.
  for (;;) {
    int32_t c = lexer->lookahead;
    if (c == '\n') { saw_newline = true; skip(lexer); }
    else if (c == ' ' || c == '\t' || c == '\r') { skip(lexer); }
    else break;
  }

  // A nestable block comment may begin here. Mark first so a non-`/*` (`//`, `/`) rolls back.
  if (valid_symbols[BLOCK_COMMENT] && lexer->lookahead == '/') {
    lexer->mark_end(lexer);
    advance(lexer);
    if (lexer->lookahead == '*') {
      advance(lexer);
      unsigned depth = 1;
      while (depth > 0) {
        if (lexer->eof(lexer)) return false; // unterminated
        if (lexer->lookahead == '/') {
          advance(lexer);
          if (lexer->lookahead == '*') { advance(lexer); depth++; }
        } else if (lexer->lookahead == '*') {
          advance(lexer);
          if (lexer->lookahead == '/') { advance(lexer); depth--; }
        } else {
          advance(lexer);
        }
      }
      lexer->result_symbol = BLOCK_COMMENT;
      lexer->mark_end(lexer);
      return true;
    }
    // Not `/*` — a `//` line comment or a `/` operator. Roll back and let the grammar lex it; a
    // NEWLINE (if one is owed) is emitted on the next scan, after the comment.
    return false;
  }

  // Automatic statement terminator.
  if (valid_symbols[NEWLINE] && (saw_newline || lexer->eof(lexer))) {
    if (!lexer->eof(lexer) && is_continuation(lexer->lookahead)) return false;
    lexer->result_symbol = NEWLINE;
    return true;
  }

  return false;
}
