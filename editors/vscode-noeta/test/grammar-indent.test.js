// Regression tests for the interplay between the TextMate grammars and VS Code's auto-indent.
//
// VS Code evaluates language-configuration.json's indentationRules against PROCESSED lines:
// bracket characters inside tokens whose standard token type is Comment/String/RegEx are removed
// before the patterns run (editor/common/languages/supports/indentationLineProcessor.ts), and the
// standard token type of a token is decided by matching each scope name against
// /\b(comment|string|regex|meta\.embedded)\b/ (vscode-textmate BasicScopeAttributesProvider),
// innermost scope first, NotSet inheriting outward.
//
// So if a tier block's closing `}` carries a scope containing the word "comment", the brace is
// invisible to decreaseIndentPattern: pressing Enter after a multi-line `@doc { … }` block walks
// past the (now empty) `}` line and inherits the doc body's indentation. These tests tokenize
// real samples with the shipped grammars and drive a faithful port of VS Code's indent algorithm
// (editor/common/languages/autoIndent.ts) to pin the fixed behavior.
//
// Run: npm test  (plain node; no test framework)

const fs = require('fs');
const path = require('path');
const oniguruma = require('vscode-oniguruma');
const vsctm = require('vscode-textmate');

const EXT_ROOT = path.join(__dirname, '..');
const TAB = '    '; // 4-space indent, matching the samples

// ---------------------------------------------------------------------------
// Tokenization: registry over the shipped grammars, with the tier injections.
// ---------------------------------------------------------------------------

const GRAMMAR_PATHS = {
  'source.noeta': 'syntaxes/noeta.tmLanguage.json',
  'inline.noeta.tier-languages': 'syntaxes/tier-languages.tmLanguage.json',
  'inline.noeta.generated-tiers': 'syntaxes/generated-tiers.tmLanguage.json',
};

async function makeRegistry() {
  const wasm = fs.readFileSync(
    path.join(EXT_ROOT, 'node_modules/vscode-oniguruma/release/onig.wasm')
  ).buffer;
  await oniguruma.loadWASM(wasm);
  return new vsctm.Registry({
    onigLib: Promise.resolve({
      createOnigScanner: (s) => new oniguruma.OnigScanner(s),
      createOnigString: (s) => new oniguruma.OnigString(s),
    }),
    loadGrammar: async (scopeName) => {
      const rel = GRAMMAR_PATHS[scopeName];
      if (!rel) return null; // e.g. text.html.markdown — not bundled, include is inert
      return vsctm.parseRawGrammar(
        fs.readFileSync(path.join(EXT_ROOT, rel), 'utf8'),
        path.join(EXT_ROOT, rel)
      );
    },
    getInjections: (scopeName) =>
      scopeName === 'source.noeta'
        ? ['inline.noeta.tier-languages', 'inline.noeta.generated-tiers']
        : undefined,
  });
}

/** Tokenize `lines` with the noeta grammar; returns per-line arrays of {text, scopes}. */
async function tokenize(registry, lines) {
  const grammar = await registry.loadGrammar('source.noeta');
  let ruleStack = vsctm.INITIAL;
  return lines.map((line) => {
    const res = grammar.tokenizeLine(line, ruleStack);
    ruleStack = res.ruleStack;
    return res.tokens.map((t) => ({
      text: line.slice(t.startIndex, t.endIndex),
      scopes: t.scopes,
    }));
  });
}

// ---------------------------------------------------------------------------
// VS Code semantics ports (kept deliberately close to the upstream sources).
// ---------------------------------------------------------------------------

const STANDARD_TOKEN_TYPE_REGEXP = /\b(comment|string|regex|meta\.embedded)\b/;

/** vscode-textmate's standard token type: innermost scope wins, NotSet inherits outward. */
function standardTokenType(scopes) {
  for (let i = scopes.length - 1; i >= 0; i--) {
    const m = scopes[i].match(STANDARD_TOKEN_TYPE_REGEXP);
    if (!m) continue;
    return m[1] === 'meta.embedded' ? 'other' : m[1];
  }
  return 'other';
}

/** IndentationLineProcessor.getProcessedTokens: strip brackets from comment/string/regex tokens. */
function processedLine(tokens) {
  return tokens
    .map((t) => {
      const type = standardTokenType(t.scopes);
      return type === 'comment' || type === 'string' || type === 'regex'
        ? t.text.replace(/[{}()\[\]]/g, '')
        : t.text;
    })
    .join('');
}

const config = JSON.parse(
  fs.readFileSync(path.join(EXT_ROOT, 'language-configuration.json'), 'utf8')
);
const INCREASE = new RegExp(config.indentationRules.increaseIndentPattern);
const DECREASE = new RegExp(config.indentationRules.decreaseIndentPattern);

const leadingWs = (s) => s.match(/^\s*/)[0];
const shift = (ind) => ind + TAB;
const unshift = (ind) => ind.slice(0, Math.max(0, ind.length - TAB.length));

/**
 * A model over raw + processed lines (1-based); `getInheritIndentForLine` and friends ported from
 * vs/editor/common/languages/autoIndent.ts (indentNextLinePattern/unIndentedLinePattern are not
 * configured for noeta, so those branches are omitted).
 */
class Model {
  constructor(raw, processed) {
    this.raw = raw;
    this.processed = processed;
  }
  shouldIncrease(n) {
    return INCREASE.test(this.processed[n - 1]);
  }
  shouldDecrease(n) {
    return DECREASE.test(this.processed[n - 1]);
  }
  rawLine(n) {
    return this.raw[n - 1];
  }

  /** getPrecedingValidLine: nearest preceding line that is not blank (raw). */
  precedingValidLine(lineNumber) {
    for (let last = lineNumber - 1; last >= 1; last--) {
      const text = this.rawLine(last);
      if (/^\s+$/.test(text) || text === '') continue;
      return last;
    }
    return 0;
  }

  /** getInheritIndentForLine. Returns {indentation, action} or null. */
  inheritIndentForLine(lineNumber, honorIntentialIndent = true) {
    if (lineNumber <= 1) return { indentation: '', action: null };
    for (let prior = lineNumber - 1; prior > 0; prior--) {
      if (this.rawLine(prior) !== '') break;
      if (prior === 1) return { indentation: '', action: null };
    }
    const preceding = this.precedingValidLine(lineNumber);
    if (preceding < 1) return { indentation: '', action: null };

    if (this.shouldIncrease(preceding)) {
      return { indentation: leadingWs(this.rawLine(preceding)), action: 'indent' };
    } else if (this.shouldDecrease(preceding)) {
      return { indentation: leadingWs(this.rawLine(preceding)), action: null };
    } else {
      if (preceding === 1 || honorIntentialIndent) {
        return { indentation: leadingWs(this.rawLine(preceding)), action: null };
      }
      for (let i = preceding; i > 0; i--) {
        if (this.shouldIncrease(i)) {
          return { indentation: leadingWs(this.rawLine(i)), action: 'indent' };
        } else if (this.shouldDecrease(i)) {
          return { indentation: leadingWs(this.rawLine(i)), action: null };
        }
      }
      return { indentation: leadingWs(this.rawLine(1)), action: null };
    }
  }
}

/** getIndentForEnter with the cursor at the end of `line` (1-based): the new line's indentation. */
function indentAfterEnter(rawLines, processedLines, line) {
  // The virtual model's `line` is the processed before-cursor text (whole line here).
  const raw = rawLines.slice();
  const processed = processedLines.slice();
  raw[line - 1] = processedLines[line - 1];
  const model = new Model(raw, processed);
  const action = model.inheritIndentForLine(line + 1);
  if (!action) return leadingWs(processedLines[line - 1]);
  let indent = action.indentation;
  if (action.action === 'indent') indent = shift(indent);
  return indent;
}

/** getIndentActionForType when `}` is typed on `line` (whose content is whitespace so far). */
function indentOnTypingCloseBrace(rawLines, processedLines, line) {
  const model = new Model(rawLines, processedLines);
  const around = processedLines[line - 1];
  if (DECREASE.test(around) || !DECREASE.test(around + '}')) return null;
  const r = model.inheritIndentForLine(line, false);
  if (!r) return null;
  return r.action === 'indent' ? r.indentation : unshift(r.indentation);
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

let failures = 0;
function check(name, actual, expected) {
  const ok = actual === expected;
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}` + (ok ? '' : `  (got ${JSON.stringify(actual)}, want ${JSON.stringify(expected)})`));
}

async function main() {
  const registry = await makeRegistry();
  const prep = async (lines) => {
    const tokens = await tokenize(registry, lines);
    return { raw: lines, processed: tokens.map(processedLine), tokens };
  };

  {
    // The reported bug: Enter after the closing `}` of a multi-line `@doc { … }` block must not
    // inherit the doc body's indentation.
    const s = await prep(['@doc {', '    Computes the distance.', '}', '']);
    check(
      'doc `}` stays visible to indent rules (processed line keeps the brace)',
      s.processed[2],
      '}'
    );
    check('Enter after multi-line @doc block → column 0', indentAfterEnter(s.raw, s.processed, 3), '');
    check('Enter after `@doc {` → one level in', indentAfterEnter(s.raw, s.processed, 1), TAB);
    // Prose is still comment-scoped: its text survives processing (only brackets are stripped),
    // and a brace inside prose is stripped.
    const prose = await prep(['@doc {', '    A map is written { like this }.', '}', '']);
    check(
      'prose braces stay invisible to indent rules',
      prose.processed[1],
      '    A map is written  like this .'
    );
    check(
      'Enter after @doc block with prose braces → column 0',
      indentAfterEnter(prose.raw, prose.processed, 3),
      ''
    );
  }

  {
    // Typing `}` on the blank line after the body must outdent to the `@doc {` line's level.
    const s = await prep(['@doc {', '    Computes the distance.', '    ', '']);
    check('typing `}` after doc body outdents to column 0', indentOnTypingCloseBrace(s.raw, s.processed, 3), '');
  }

  {
    // A method @doc inside a struct: Enter after the block lands at member level, not body level.
    const s = await prep([
      'struct Point {',
      '    @doc {',
      '        Distance from origin.',
      '    }',
      '',
    ]);
    check('Enter after nested @doc block → member level', indentAfterEnter(s.raw, s.processed, 4), TAB);
  }

  {
    // The same delimiter fix applies to the well-known-language tier blocks (tier-languages
    // injection grammar): a multi-line `@sql { … }` block.
    const s = await prep(['q = @sql {', '    SELECT 1', '}', '']);
    check('sql tier `}` stays visible to indent rules', s.processed[2], '}');
    check('Enter after @sql block → column 0', indentAfterEnter(s.raw, s.processed, 3), '');
  }

  {
    // Control: ordinary code braces behave the same as before.
    const s = await prep(['fn f() {', '    return 1', '}', '']);
    check('Enter after ordinary fn block → column 0', indentAfterEnter(s.raw, s.processed, 3), '');
    check('Enter after `fn f() {` → one level in', indentAfterEnter(s.raw, s.processed, 1), TAB);
  }

  {
    // The delimiter scopes must never regress to a comment/string standard type, and the body
    // content must keep it (that is what keeps prose braces out of bracket matching).
    const s = await prep(['@doc {', '    Prose.', '}', '']);
    const closing = s.tokens[2].find((t) => t.text === '}');
    check('closing delimiter standard type', standardTokenType(closing.scopes), 'other');
    const prose = s.tokens[1].find((t) => t.text.includes('Prose'));
    check('doc body standard type', standardTokenType(prose.scopes), 'comment');
  }

  console.log(failures === 0 ? '\nAll grammar-indent tests passed.' : `\n${failures} FAILURE(S)`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
