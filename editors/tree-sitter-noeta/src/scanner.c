#include "tree_sitter/parser.h"
#include <stdbool.h>

// External scanner for Noeta. Two tokens:
//   * BLOCK_COMMENT   — `/* ... */`, *nestable* (a plain regex token cannot express nesting).
//   * NEWLINE         — an automatic statement terminator. Noeta terminates a statement at a
//                       newline, EXCEPT when the statement is syntactically incomplete: a trailing
//                       operator (handled by the parser — after a dangling operator a terminator is
//                       not valid, so we are never asked) or a *leading* continuation on the next
//                       line (`.`, `|>`, a binary operator), which we detect by peeking here.

enum TokenType {
  BLOCK_COMMENT,
  NEWLINE,
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
