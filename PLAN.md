# mk — GNU make 3.81-compatible reimplementation in Rust

## Constraints

- **Target**: GNU make 3.81 semantics exactly (the macOS system default)
- **Dependencies**: stdlib only; `libc` if needed for signal/fd work
- **Pattern matching**: hand-rolled, no regex crate
- **Recursion**: mk-only (`$(MAKE)` always refers to mk itself)
- **Error messages**: byte-for-byte compatible with GNU make 3.81 format
- **Platforms**: macOS first; Alpine Linux via Docker harness added later
- **Parallelism**: `-j` caps at nproc by default; output-sync on by default
- **No release builds**: `cargo build` only, never `--release`

## Module layout

```
src/
├── main.rs          // entry, makefile discovery, restart loop
├── args.rs          // hand-rolled argv parser
├── error.rs         // Error + Location types, Display matches GNU make format
└── makefile/
    ├── mod.rs       // Makefile struct, public build API
    ├── reader.rs    // logical-line reader (backslash continuation, comments)
    ├── parse.rs     // line classifier + directive/rule/assignment parsers
    ├── var.rs       // VarTable, VarFlavor, VarOrigin, assignment ops
    ├── expand.rs    // macro expansion engine (lazy/immediate, nested, cycles)
    ├── rule.rs      // Rule, Target, prerequisite lists, archive members
    ├── graph.rs     // dependency graph, topo sort, up-to-date checks
    ├── implicit.rs  // pattern rules, suffix rules, chain rules, built-ins
    ├── vpath.rs     // VPATH variable + vpath directive
    ├── functions.rs // all $(fn ...) implementations
    ├── exec.rs      // sequential recipe execution
    └── jobs.rs      // parallel scheduler, output-sync, jobserver, signals
tests/
├── harness.rs       // shared helpers: run mk, run make, fixture loader
├── compare.rs       // comparison tests: same fixture → diff mk vs make output
└── fixtures/        // one subdir per named case: Makefile + expected_{stdout,stderr,exit}
```

---

## Phase 0 — Scaffolding & Test Infrastructure

**Done when**: `cargo test` runs green; comparison harness can diff mk vs make on a trivial fixture.

- [ ] Create module stubs (empty `mod` declarations, no logic)
- [ ] `error.rs`: `struct Location { file, line }` + `Error` enum; `Display` emits
  `{file}:{line}: *** {msg}.  Stop.` for fatal errors and `{file}:{line}: {msg}` for warnings
- [ ] `tests/harness.rs`:
  - `run_mk(dir, args) -> Output` — builds mk if needed, runs it, captures stdout/stderr/exit
  - `run_make(dir, args) -> Output` — runs system `make` (3.81)
  - `load_fixture(name) -> TempDir` — copies `tests/fixtures/{name}/` into a tempdir
  - `assert_fixture(name, args)` — run mk on fixture, compare against expected files
- [ ] `tests/compare.rs`:
  - `compare_fixture(name, args)` — run both mk and make, assert outputs are identical
  - Start with a single trivial fixture `hello`: one phony target that echoes a string
- [ ] Add fixture `hello/Makefile` and its `expected_stdout`, `expected_stderr`, `expected_exit`
- [ ] `args.rs`: stub that parses nothing but passes `cargo test`

**TDD rule for all subsequent phases**: write the failing test(s) first, then implement until green.

---

## Phase 1 — Parser & Variable System

**Done when**: `mk -p` prints the variable database for a Makefile with variables and simple rules (no execution).

### reader.rs
- [ ] `LogicalLine`: joins `\`-continued physical lines into one, tracks start line number
- [ ] Strips `#` comments (respecting quotes and `\#` escapes)
- [ ] Test: continuation across 3 lines; comment mid-line; `\#` is a literal `#`

### var.rs
- [ ] `VarFlavor`: `Recursive` (`=`), `Simple` (`:=`), `Conditional` (`?=`), `Append` (`+=`), `Shell` (future)
- [ ] `VarOrigin`: `Default`, `Environment`, `File`, `CommandLine`, `Override`
- [ ] `Var { flavor, origin, raw: String, override_flag: bool }`
- [ ] `VarTable`: `HashMap<String, Var>` with correct precedence: CommandLine > Override > File > Environment > Default
- [ ] `set(name, flavor, origin, raw, override_flag)`: enforces precedence rules
- [ ] `undefine(name, origin)`: respects same precedence rules
- [ ] `export` / `unexport` tracking per variable
- [ ] Tests: each flavor; override blocks lower-priority reset; undefine respects precedence; `?=` no-ops if already set

### expand.rs
- [ ] `expand(raw: &str, vars: &VarTable, auto: &AutoVars) -> String`
- [ ] Handles `$(NAME)`, `${NAME}`, `$X` (single char)
- [ ] Nested: `$($(INNER))`
- [ ] Recursive vars expand at use-time; simple vars expand at definition-time (caller's responsibility to store already-expanded value)
- [ ] Cycle detection: track expansion stack, emit `*** Recursive variable 'X' references itself` and stop
- [ ] Automatic variables: `$@`, `$<`, `$^`, `$+`, `$?`, `$*`, `$|`
- [ ] `D`/`F` variants: `$(@D)`, `$(@F)`, `$(<D)`, `$(<F)`, `$(^D)`, `$(^F)`, `$(*D)`, `$(*F)`
- [ ] Tests: each automatic variable; nested expansion; cycle detection error text; `D`/`F` variants

### parse.rs
- [ ] Line classifier: given a `LogicalLine` → `LineKind` enum (Assignment, RuleHeader, Recipe, Directive, Blank)
- [ ] Assignment parser: handles all flavors, `override`, `target: var = val` (target-specific)
- [ ] Rule header parser: targets, `:` vs `::`, normal prerequisites, `|` order-only prerequisites
- [ ] Recipe line: tab-prefix required; extract `@`, `-`, `+` modifiers
- [ ] Directives: `include`, `-include` / `sinclude`, `define`/`endef`, `export`/`unexport`, `undefine`
- [ ] Conditionals: `ifeq`/`ifneq`/`ifdef`/`ifndef`/`else`/`endif`, full nesting, conditional `else ifeq`
- [ ] `-I` flag: maintain an include search path; `include foo` searches `-I` dirs if not found locally
- [ ] Tests: each directive; nested conditionals (depth 3); `define` block with blank lines; `ifeq` with empty string; `-include` of missing file is silent

---

## Phase 2 — Rules & Dependency Graph

**Done when**: `mk` can build targets from explicit rules sequentially with correct dependency ordering and up-to-date checks.

### rule.rs
- [ ] `Prereq { name: String, order_only: bool }`
- [ ] `RecipeLine { text: String, silent: bool, ignore_err: bool, always_run: bool }`
- [ ] `Rule { targets, prereqs, recipe_lines, double_colon: bool }`
- [ ] Archive member target: parse `lib(member)` syntax in target names
- [ ] Single-colon semantics: multiple definitions for same target merge prereqs; last recipe wins
- [ ] Double-colon semantics: stored as separate `Rule` entries, all run independently
- [ ] Tests: prereq merge; double-colon independence; order-only not used for up-to-date check; archive member parse

### graph.rs
- [ ] `BuildGraph`: maps target name → list of applicable rules + known mtime
- [ ] Topological sort (Kahn's algorithm); cycle detection emits `*** Circular X <- Y dependency dropped`
- [ ] `needs_rebuild(target) -> bool`: target missing OR any normal prereq newer than target
- [ ] Order-only prereqs: must exist (built if missing) but mtime not compared
- [ ] `.PHONY` targets: always `needs_rebuild = true`
- [ ] `.PRECIOUS` targets: never deleted on error/interrupt
- [ ] `.INTERMEDIATE` targets: deleted after build; not deleted if already exists before build starts
- [ ] `.SECONDARY` targets: not auto-deleted (like `.PRECIOUS` but considered intermediate for chain rules)
- [ ] `.DEFAULT` rule: recipe to run when no other rule matches
- [ ] `MAKECMDGOALS`: set to space-separated list of goals from command line
- [ ] Tests: cycle detection message; order-only rebuild logic; phony always rebuilds; intermediate cleanup; `.DEFAULT` fires

### exec.rs (sequential only for now)
- [ ] Execute a recipe: fork/exec each line via `$SHELL -c` (default `SHELL=/bin/sh`)
- [ ] `SHELL` variable overrides the shell binary
- [ ] Apply `@` (silent), `-` (ignore error), `+` (always run in dry-run) modifiers
- [ ] `-n` dry-run: print but skip (except `+` lines)
- [ ] `-s` silent: suppress all printing
- [ ] `-i` / `.IGNORE` target: ignore all errors
- [ ] `-k` keep-going: record error, continue with independent targets
- [ ] `-t` touch: touch targets instead of executing
- [ ] `-q` question: exit 1 if anything out of date, no output, no execution
- [ ] `$(MAKE)` in recipes: replaced with the path to the mk binary itself
- [ ] `MAKEFLAGS` exported to child processes: encode current flags
- [ ] `MAKE_RESTARTS`: set to restart count in environment
- [ ] Tests: `@` suppresses echo; `-` continues after non-zero exit; dry-run skips; touch mode; question mode exit code

### Makefile self-rebuild
- [ ] After parsing, check if any read makefile has an explicit rule; if so, attempt to rebuild it
- [ ] If any makefile was rebuilt, restart parsing from scratch (increment `MAKE_RESTARTS`)
- [ ] `-include`d files: if missing and no rule to build them, silently skip (do not restart)
- [ ] Test: `Makefile` has a rule to regenerate itself from `Makefile.src`; verify restart happens once

---

## Phase 3 — Implicit Rules & vpath

**Done when**: `mk` compiles a multi-file C project using only built-in rules.

### implicit.rs
- [ ] `PatternRule { pattern: String, prereq_patterns, recipe_lines }` — `%` is the stem wildcard
- [ ] `match_pattern(target, pattern) -> Option<String>` (returns stem): hand-rolled, no regex
- [ ] Suffix rule translation: `.c.o:` → `%.o: %.c` at parse time
- [ ] `.SUFFIXES` management: default list; user can prepend/replace; `-r` clears
- [ ] Static pattern rules: `$(TARGETS): %.o: %.c`
- [ ] Chain rules (multi-step implicit): find paths through implicit rule graph (limit depth to avoid infinite search)
- [ ] Rule search order: explicit > pattern rules (in order defined) > suffix rules > built-ins
- [ ] Built-in rules: C (`$(CC) $(CFLAGS) -c -o $@ $<`), C++ (`$(CXX) $(CXXFLAGS) -c -o $@ $<`), assembler; standard library archive rules
- [ ] Built-in variables: `CC=cc`, `CXX=g++`, `AR=ar`, `ARFLAGS=rv`, `MAKE`, `CFLAGS=`, `CXXFLAGS=`, `LDFLAGS=`, `LDLIBS=`
- [ ] Archive member rules: `lib(member.o)` targets; `$(AR) $(ARFLAGS) lib member.o` recipe
- [ ] `-r` flag: disable built-in rules and built-in variable defaults
- [ ] Tests: `.c` → `.o`; `.cc` → `.o`; chain `.y` → `.c` → `.o`; suffix rule translation; rule priority; archive member

### vpath.rs
- [ ] `VPATH` variable: colon-separated (or space-separated) search dirs for all files
- [ ] `vpath pattern dirs` directive: per-pattern search path
- [ ] `vpath pattern` (no dirs): clears that pattern's search path
- [ ] `vpath` (no args): clears all vpath entries
- [ ] Search logic: only used when file does not exist in current dir; found path stored as prereq's actual path; `$<` etc. reflect the found path
- [ ] Tests: basic VPATH; pattern-specific vpath; clearing; current-dir takes priority

---

## Phase 4 — GNU Functions

**Done when**: all `$(fn ...)` calls in the test suite pass comparison tests against real make.

For each function: write the comparison test first, then implement.

**Text functions**
- [ ] `subst from,to,text`
- [ ] `patsubst pattern,replacement,text` — `%` stem in both pattern and replacement
- [ ] `strip text` — collapse whitespace, trim
- [ ] `findstring find,text`
- [ ] `filter pattern...,text`
- [ ] `filter-out pattern...,text`
- [ ] `sort list` — lexicographic sort + dedup
- [ ] `word n,text`
- [ ] `words text`
- [ ] `wordlist s,e,text`
- [ ] `firstword text`
- [ ] `lastword text`

**Filename functions**
- [ ] `dir names`
- [ ] `notdir names`
- [ ] `suffix names`
- [ ] `basename names`
- [ ] `addsuffix suf,names`
- [ ] `addprefix pre,names`
- [ ] `join list1,list2`
- [ ] `wildcard pattern` — glob expansion
- [ ] `realpath names` — resolve symlinks + normalize
- [ ] `abspath names` — normalize without resolving symlinks

**Control / meta**
- [ ] `if condition,then[,else]`
- [ ] `or conditions...`
- [ ] `and conditions...`
- [ ] `foreach var,list,text`
- [ ] `call var,params...` — user-defined parameterized macros
- [ ] `value var` — return unexpanded definition
- [ ] `flavor var` — returns `"undefined"`, `"recursive"`, or `"simple"`
- [ ] `origin var` — returns `"undefined"`, `"default"`, `"environment"`, `"file"`, `"command line"`, `"override"`, `"automatic"`
- [ ] `eval text` — parse string as makefile input at runtime
- [ ] `shell cmd` — run command, capture stdout, replace newlines with spaces

**Diagnostic**
- [ ] `error text` — emit `*** text.  Stop.` to stderr, exit 2
- [ ] `warning text` — emit `file:line: text` to stderr, continue
- [ ] `info text` — emit `text` to stdout, continue

**Tests**: one comparison test per function; edge cases: empty input, `%` in patsubst with no match, `$(shell)` non-zero exit, `$(eval)` defining a rule, `$(foreach)` empty list.

---

## Phase 5 — Second Expansion, Target-Specific Vars & Remaining GNU-isms

**Done when**: autoconf-generated Makefiles parse and evaluate correctly (variables, conditionals, functions — no execution bugs).

### .SECONDEXPANSION
- [ ] When `.SECONDEXPANSION` is seen, all subsequent prerequisites undergo a second round of expansion at build time (not parse time)
- [ ] `$$` in prerequisites is a literal `$` after first expansion, which then expands at build time
- [ ] Automatic variables (`$$@`, `$$<`, etc.) available during second expansion
- [ ] Tests: `$$(@F)` in prereqs; `$$(VAR)` that changes between targets; interaction with pattern rules

### Target-specific variables
- [ ] `target: VAR = value` — sets VAR only for that target's recipe and its prerequisites (recursively)
- [ ] Inheritance: prereqs see the target-specific vars of their dependents during that build
- [ ] `override` works in target-specific context
- [ ] Tests: inheritance through 3-level prereq chain; override in target-specific; target-specific shadows global

### Remaining special targets
- [ ] `.EXPORT_ALL_VARIABLES`: export every variable to sub-make environments
- [ ] `.NOTPARALLEL`: disable `-j` for this invocation
- [ ] `.POSIX`: enable strict POSIX mode (error on undefined variables etc.) — can be minimal
- [ ] `.IGNORE`, `.SILENT` as targets (affect all rules) vs. as special targets

### Remaining variables
- [ ] `MAKEFILES` env var: space-separated list of makefiles included before the main one (silently, no error if missing)
- [ ] `MAKEFILE_LIST`: updated as each file is read (including includes)
- [ ] `CURDIR`: set after any `-C` change; not overridable by the Makefile
- [ ] `MAKELEVEL`: incremented for each recursive `$(MAKE)` invocation
- [ ] `MAKE_RESTARTS`: set to restart count
- [ ] `-C dir` / `--directory`: `chdir` before doing anything; print `Entering/Leaving directory` messages

### `-p` (print database)
- [ ] Print all variables (with origin), all rules (explicit and implicit), in GNU make 3.81 format
- [ ] Comparison test: `mk -p` output matches `make -p` output on a representative Makefile

---

## Phase 6 — Parallel Execution & Signal Handling

**Done when**: `mk -j4` builds a multi-target project correctly and `Ctrl-C` cleans up.

### jobs.rs
- [ ] `Scheduler`: runs up to N recipes in parallel; each recipe is a `std::process::Child`
- [ ] Default N: `std::thread::available_parallelism()` (nproc); `-j` alone uses nproc; `-j N` uses N
- [ ] `.NOTPARALLEL`: fall back to sequential (`-j1`) for this invocation
- [ ] Output sync (default on): buffer each recipe's stdout+stderr; flush atomically when recipe finishes; prefix with target name if output from multiple targets interleaves
- [ ] `--output-sync=none` flag to disable output buffering (raw interleave)

### Jobserver protocol
- [ ] On startup with `-j N`: create a pipe; write N-1 tokens (single bytes) into the write end
- [ ] Before starting a recipe: read one token from the pipe (blocks if at capacity)
- [ ] After recipe finishes: write one token back
- [ ] Encode `--jobserver-fds=R,W` in `MAKEFLAGS` exported to child mk processes
- [ ] Child mk: detect `--jobserver-fds` in `MAKEFLAGS`; use parent's pipe instead of creating its own
- [ ] Tests: parent + child mk share job limit; total parallel processes never exceeds N

### Signal handling
- [ ] Each recipe subprocess runs in its own process group (`setpgid`)
- [ ] SIGINT / SIGTERM / SIGHUP: send SIGTERM to all active process groups; wait for them; delete targets currently being built unless `.PRECIOUS`; exit with signal status
- [ ] `libc` crate for `signal()`, `setpgid()`, `kill()`, `waitpid()`
- [ ] Tests: interrupt mid-build leaves `.PRECIOUS` targets, removes non-precious partial outputs; SIGINT exits non-zero

---

## Phase 7 — Integration & Compatibility Testing

**Done when**: mk successfully builds zlib and a small autoconf project; Docker harness runs on Alpine.

### Comparison test suite (macOS)
- [ ] Fixtures covering every phase: one per major feature
- [ ] `tests/compare.rs` runs all fixtures through both mk and make, asserts identical stdout+stderr+exit
- [ ] Fixture generator: script that produces Makefiles exercising known edge cases and records `make` output as the expected baseline

### Real-project tests
- [ ] **zlib**: `./configure && mk` — basic C project with autoconf Makefile
- [ ] **musl libc**: tests archive member rules and complex implicit rules
- [ ] **binutils** (or similar): tests VPATH, complex pattern rules, recursive make

### Alpine Linux Docker harness
- [ ] `Dockerfile` in repo: `FROM alpine`, installs `build-base` (has real make for comparison), copies mk source, runs `cargo test`
- [ ] `Makefile` target `docker-test`: builds image, runs tests, pipes results out
- [ ] Verify signal handling and process group behavior on Linux (different from macOS in subtle ways)

---

## Feature exclusions (explicitly out of scope)

These are GNU make 3.82+ or 4.x features and will not be implemented:

- `::=` POSIX simple assignment (4.0+)
- `!=` shell assignment (4.0+)
- `.ONESHELL` (3.82+)
- `.RECIPEPREFIX` (3.82+)
- `$(file ...)` function (4.0+)
- `private` variable modifier (4.2+)
- `$(let ...)`, `$(intcmp ...)`, `$(bitand ...)` (4.4+)
- `$(guile ...)` (4.0+)
- SCCS / RCS integration
- Internationalization (`LANG`, `LC_*`)
- Dynamically loadable objects (`load` directive)
