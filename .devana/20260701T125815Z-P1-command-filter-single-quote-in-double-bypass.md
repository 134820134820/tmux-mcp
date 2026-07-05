DEVANA-FINDING: v1
DEVANA-STATE: fixed | P1 | high | security=yes
DEVANA-KEY: src/security.rs:810 | command-filter-single-quote-in-double-bypass

# Command filter merges statements when a single quote appears inside a double-quoted string

## Finding

`collect_shell_statements` enters single-quote mode whenever it sees `'`, even
while already inside a double-quoted region. In `security.rs:810` the `'\''`
match arm sets `in_single = true` with no `in_double` guard. In a real shell a
`'` inside `"..."` is a literal character and does **not** start a quoted
region, so the shell keeps parsing `;`, `|`, `&` after the closing `"` as
statement separators. Because the splitter instead treats the stray `'` as an
opening single quote, it swallows the closing `"`, every following separator,
and the rest of the line into one merged statement. `check_command` then runs
the anchored allow/deny regex against that single merged statement, so a
trailing denied command is never screened as its own statement.

## Violated Invariant Or Contract

`check_command` (security.rs:658-665) documents the invariant: every statement
the shell would actually execute must be validated individually, precisely
because "an allowed prefix" must not "smuggle extra statements past the filter."
The function's own doc (security.rs:785-786) states single quotes are literal
"no splitting or substitution inside them" — but that rule must not apply to a
`'` that is itself inside double quotes.

## Oracle

The fix's own test `test_command_filter_blocks_chained_statements`
(security.rs:989+) asserts `true; rm -rf /` is blocked under denylist `^rm `.
A shell splits `echo "'"; rm -rf /` into exactly two statements
(`echo "'"` and `rm -rf /`) the same way it splits `true; rm -rf /`; the
denylist must therefore block the second statement. `check_command_line`
(security.rs:645-653) matches each statement against `^rm ` anchored to the
statement start, so the split boundary is what makes the guard work.

## Counterexample

Denylist mode, pattern `^rm `. User command via `execute-command`:

    echo "'"; rm -rf /

Trace through `collect_shell_statements`:
- `echo ` → buffered.
- `"` → `in_double = true`.
- `'` → hits the `'\''` arm (security.rs:810) → `in_single = true` (the
  `in_double` state is ignored).
- `"`, `;`, ` rm -rf /` → all consumed by the `in_single` fast-path
  (security.rs:800-807) because no further `'` closes the mode.
- End of input → `flush_statement` emits the single statement
  `echo "'"; rm -rf /`.

`check_command_line("echo \"'\"; rm -rf /")` tests `^rm ` against a string that
starts with `echo` → no match → `Ok`. The denied `rm -rf /` executes.

The allowlist direction is worse: with allow pattern `^echo ` the merged
statement starts with `echo ` → matches → the whole line (including
`rm -rf /`) is approved, defeating a default-deny allowlist. Any double-quoted
string containing an apostrophe (`echo "it's done"; rm -rf /`) triggers the
same merge.

## Why It Might Matter

Operators use `command_filter` to contain the agent. A single apostrophe inside
a double-quoted argument — an extremely common, non-adversarial-looking shape —
lets an arbitrary trailing statement bypass both denylist and allowlist without
`raw_mode` or literal `send-keys`. This reopens exactly the class of chained
bypass the new splitter was added to close.

## Proof

Control-flow / dataflow trace: `execute-command` input → `server.rs`
`check_command` → `collect_shell_statements` (single-quote arm at
security.rs:810 sets `in_single` inside a double-quoted region) → separators
after the closing `"` are not flushed → one merged statement → anchored regex
in `check_command_line` misses the trailing command → statement reaches the
`commands.rs` wrapper and executes.

## Counterevidence Checked

- Existing report `command-filter-quote-breakout-bypass` covers an *unbalanced*
  double quote (`x"; rm -rf /`) that relies on the `commands.rs` wrapper's outer
  quote to break out. This finding is distinct: the double quote here is
  balanced, the command is a valid standalone shell line, and the merge is a
  pure parser bug in `collect_shell_statements` that reproduces in the
  non-wrapped `check_command` path with no wrapper interaction.
- The `in_single` fast-path (security.rs:800-807) does check for a closing `'`,
  so a *pair* of single quotes inside double quotes would re-open splitting; but
  an odd apostrophe (the common possessive/contraction case) leaves `in_single`
  stuck true to end of input, and even a pair merges the separators *between*
  the two apostrophes.
- Denylist patterns that are unanchored (e.g. `rm -rf`) would still match the
  merged statement via `is_match`; the bypass requires an anchored pattern
  (`^rm `). Anchored patterns are the documented, tested use case (the fix's own
  test uses `^rm `), and allowlist mode is bypassed regardless of anchoring.
- Strongest reason it might be false: if the shell also treated `'` inside `"`
  as a quote, the merge would match the shell and there would be no bypass. It
  does not — POSIX shells treat `'` as an ordinary character inside double
  quotes — so the splitter and the shell disagree, which is the bug.

## Suggested Next Step

Guard the single-quote arm with `!in_double` (and, symmetrically, only treat
`"` as a toggle when `!in_single`) so quote state mirrors POSIX shell parsing.
Add a test: denylist `^rm `, command `echo "'"; rm -rf /`, expect `Err`.

## Status Notes

- 2026-07-01: open by Devana. Initial report written from static source inspection of the uncommitted `collect_shell_statements` change on branch `devana-fixes`.
- 2026-07-05: fixed. `collect_shell_statements` now treats a single quote inside double quotes as literal text instead of entering single-quote mode. Added denylist and allowlist regression coverage for `echo "'"; rm -rf /`; validated with `cargo test --lib security::tests::test_command_filter`.

DEVANA-KEY: src/security.rs:810 | command-filter-single-quote-in-double-bypass
DEVANA-SUMMARY: fixed | P1 | high | A single quote inside a double-quoted string is now literal to collect_shell_statements, so chained statements after the closing double quote are split and screened by denylist and allowlist filters.
