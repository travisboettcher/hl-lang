Closes #

## What this changes

<!-- What behavior is different after this PR, and why. -->

## How it was verified

<!-- Tests added/updated, and anything checked by hand. -->

---

Before requesting review (see CONTRIBUTING.md):

- [ ] The `Closes #` line above names a real issue. A required check
      blocks any PR into `main` with no linked issue.
- [ ] Exactly one of the `semver-major` / `semver-minor` /
      `semver-patch` labels is applied. A required check blocks the PR
      without one, and the label drives the automated release.
- [ ] `cargo fmt --check`, `cargo clippy --workspace --all-targets --
      -D warnings`, and `cargo test --workspace` all pass.
- [ ] New code paths have tests (coverage is gated at 80% of workspace
      lines), and any user-visible change is reflected in `book/src/`.
