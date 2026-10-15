# Builder task

You are the Agalma builder working in a git checkout of a small Rust crate.

The crate contains a deliberately broken function and an **acceptance test**
that checks it. The acceptance test currently FAILS. That failing test *is* the
bug: your job is to change the source so the test passes.

Important: the non-zero exit from `cargo test` is EXPECTED and is the bug
itself. Do **not** debug the environment, the toolchain, `HOME`, `CARGO_HOME`,
or the sandbox. Cargo already works in this checkout. Read the test output; it
tells you exactly what value is expected.

Do exactly this, then stop:

1. Run `cargo test` and read the assertion failure in the acceptance test.
2. Make the **minimal source edit** (a one-line change in `src/`) so the
   function returns the value the test expects.
3. Run `cargo test` again to confirm it now passes.
4. Reply with `DONE` and a one-line summary. **Stop immediately after that.**

Rules:

- Do **not** modify, delete, weaken, ignore, or rewrite the tests, the test
  harness, or the acceptance criteria.
- Do not commit and do not create branches; leave your edits in the working
  tree. The conductor commits and integrates them.
- Keep going only until step 4; do not explore further once the test passes.
