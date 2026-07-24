// Regression tests for ${…} interpolation highlighting inside strings.
//
// The bug: a ${…} hole in a string rendered in the enclosing string's color. The grammar DID break
// the hole into tokens, but every one of them still carried `string.quoted.double.noeta` as an
// ancestor scope, and the hole used bespoke scope names (meta.interpolation.*,
// punctuation.section.interpolation.*) that no theme targets. So only tokens a theme styled MORE
// specifically than `string` (self, a call name) punched through; the ${ } delimiters, the accessor
// dot, parens, and every bare identifier (`id`, `i`, `items`) fell back to the string color.
//
// The fix: adopt the JavaScript/TypeScript template-string scope convention, which every theme
// already ships rules for —
//   • punctuation.definition.template-expression.begin/end  → colors the ${ and } delimiters
//   • meta.template.expression                              → RESETS the hole's foreground off string
// so the whole hole reads as code regardless of theme. These tests pin (1) the scope names the
// grammar emits and (2) the resolved foreground under a fixture theme carrying exactly those two
// canonical rules — proving the reset actually lands, not just that the scopes are present.
//
// Run: npm test  (plain node; no test framework)

const fs = require('fs');
const path = require('path');
const oniguruma = require('vscode-oniguruma');
const vsctm = require('vscode-textmate');

const EXT_ROOT = path.join(__dirname, '..');
const GRAMMAR_PATH = path.join(EXT_ROOT, 'syntaxes/noeta.tmLanguage.json');

// A minimal theme carrying the two canonical template-string rules plus the ambient `string` rule
// they must override, and the two code rules that should still win inside a hole. Distinct colors
// so each assertion pins exactly which rule resolved.
// vscode-textmate's color map normalizes to uppercase hex, so spell these uppercase.
const STRING = '#AA0000';
const CODE = '#D4D4D4'; // meta.template.expression reset
const DELIM = '#0000FF';
const VAR = '#569CD6';
const FN = '#DCDCAA';
const THEME = {
  name: 'fixture',
  settings: [
    { settings: { foreground: '#ffffff', background: '#000000' } },
    { scope: 'string', settings: { foreground: STRING } },
    { scope: 'meta.template.expression', settings: { foreground: CODE } },
    {
      scope:
        'punctuation.definition.template-expression.begin, punctuation.definition.template-expression.end',
      settings: { foreground: DELIM },
    },
    { scope: 'variable.language', settings: { foreground: VAR } },
    { scope: 'entity.name.function', settings: { foreground: FN } },
  ],
};

async function makeRegistry() {
  const wasm = fs.readFileSync(
    path.join(EXT_ROOT, 'node_modules/vscode-oniguruma/release/onig.wasm')
  ).buffer;
  await oniguruma.loadWASM(wasm);
  return new vsctm.Registry({
    theme: THEME,
    onigLib: Promise.resolve({
      createOnigScanner: (s) => new oniguruma.OnigScanner(s),
      createOnigString: (s) => new oniguruma.OnigString(s),
    }),
    loadGrammar: async (scopeName) =>
      scopeName === 'source.noeta'
        ? vsctm.parseRawGrammar(fs.readFileSync(GRAMMAR_PATH, 'utf8'), GRAMMAR_PATH)
        : null,
  });
}

const FOREGROUND_MASK = 0b00000000011111111100000000000000;
const FOREGROUND_OFFSET = 15;

let failures = 0;
function check(name, actual, expected) {
  const ok = actual === expected;
  if (!ok) failures++;
  console.log(
    `${ok ? 'PASS' : 'FAIL'}  ${name}` +
      (ok ? '' : `  (got ${JSON.stringify(actual)}, want ${JSON.stringify(expected)})`)
  );
}

async function main() {
  const registry = await makeRegistry();
  const colorMap = registry.getColorMap();
  const grammar = await registry.loadGrammar('source.noeta');

  // Resolve, for a single line, the list of {text, scopes, color} for non-whitespace tokens.
  function analyze(line) {
    const one = grammar.tokenizeLine(line, vsctm.INITIAL);
    const two = grammar.tokenizeLine2(line, vsctm.INITIAL);
    const out = [];
    const t = two.tokens;
    for (let i = 0; i < t.length; i += 2) {
      const start = t[i];
      const end = i + 2 < t.length ? t[i + 2] : line.length;
      const color = colorMap[(t[i + 1] & FOREGROUND_MASK) >>> FOREGROUND_OFFSET];
      out.push({ start, end, color, text: line.slice(start, end) });
    }
    // Resolve by character index (an exact position avoids indexOf() matching a duplicate word,
    // e.g. the literal "total:" before the ${total(…)} call). `at(sub, off)` locates a distinctive
    // anchor and steps `off` chars in — anchor on `${…` for hole identifiers that also occur in the
    // surrounding literal text.
    const at = (sub, off = 0) => line.indexOf(sub) + off;
    const scopesAtIdx = (idx) => {
      const tok = one.tokens.find((x) => x.startIndex <= idx && idx < x.endIndex);
      return tok ? tok.scopes : [];
    };
    const colorAtIdx = (idx) => {
      const tok = out.find((x) => x.start <= idx && idx < x.end);
      return tok ? tok.color : null;
    };
    const scopesAt = (needle) => scopesAtIdx(line.indexOf(needle));
    const colorOf = (needle) => colorAtIdx(line.indexOf(needle));
    return { at, scopesAt, colorOf, scopesAtIdx, colorAtIdx };
  }

  {
    // Image #1: "Order #${self.id} awaiting payment" — self.id is a field access in the hole.
    const a = analyze('    x = "Order #${self.id} awaiting payment"');

    // (1) Scope names follow the template-string convention.
    check(
      'opening ${ carries template-expression.begin scope',
      a.scopesAt('${').includes('punctuation.definition.template-expression.begin.noeta'),
      true
    );
    check(
      'hole region carries meta.template.expression scope',
      a.scopesAt('self').includes('meta.template.expression.noeta'),
      true
    );
    check(
      'hole content carries meta.embedded scope',
      a.scopesAt('self').includes('meta.embedded.line.noeta'),
      true
    );

    // (2) Resolved foreground: the hole reads as code, the literal text stays string.
    check('literal string text resolves to string color', a.colorOf('Order #'), STRING);
    check('${ delimiter resolves to delimiter color, not string', a.colorOf('${'), DELIM);
    check('self resolves to variable color', a.colorOf('self'), VAR);
    // `id` gets no scope of its own — the regression was it inheriting the string color. It must
    // now resolve to the meta.template.expression reset color instead.
    check('bare identifier `id` resolves to code color, not string', a.colorOf('id'), CODE);
    check('trailing literal text stays string color', a.colorOf('awaiting'), STRING);
  }

  {
    // Image #2: "total: ${total(items)}" — a call inside the hole; the closing } must be a delimiter.
    const a = analyze('    x = "total: ${total(items)}"');
    check('call name resolves to function color', a.colorAtIdx(a.at('${total', 2)), FN);
    check('call argument `items` resolves to code color, not string', a.colorOf('items'), CODE);
    check('closing } resolves to delimiter color, not string', a.colorOf('}'), DELIM);
    check(
      'closing } carries template-expression.end scope',
      a.scopesAt('}').includes('punctuation.definition.template-expression.end.noeta'),
      true
    );
  }

  {
    // Image #2: "item ${i} has a negative price" — the whole-hole-is-one-identifier case that
    // previously rendered entirely as string.
    const a = analyze('    x = "item ${i} has a negative price"');
    check('single bare identifier `i` resolves to code color, not string', a.colorAtIdx(a.at('${i}', 2)), CODE);
    check('text after the hole stays string color', a.colorOf('has a negative'), STRING);
  }

  {
    // Backtick template strings and single-quoted strings share the same #interpolation rule.
    const bt = analyze('    x = `sum is ${n}`');
    check('backtick-string hole content resolves to code color', bt.colorAtIdx(bt.at('${n}', 2)), CODE);
    const sq = analyze("    x = 'v=${v}'");
    check('single-quoted-string hole content resolves to code color', sq.colorAtIdx(sq.at('${v}', 2)), CODE);
  }

  {
    // An escaped opener \${ is a literal, NOT a hole: it must stay string-colored throughout.
    const a = analyze('    x = "literal \\${not} a hole"');
    check('escaped \\${ does not open a hole (not stays string)', a.colorOf('not'), STRING);
  }

  console.log(failures === 0 ? '\nAll grammar-interpolation tests passed.' : `\n${failures} FAILURE(S)`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
