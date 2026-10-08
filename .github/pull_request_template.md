# Pull Request

## Summary

<!-- What does this change do and why? Link any related issue with "Fixes #123". -->

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor / internal cleanup
- [ ] Documentation
- [ ] Build / CI / tooling

## How was this tested?

<!-- Describe the tests you ran and any manual verification. -->

- [ ] `cargo --locked fmt --all -- --check`
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --all-targets --locked`
- [ ] `cargo test --doc --locked`
- [ ] Manual verification on a real Windows Terminal (describe below, if applicable)

## Checklist

- [ ] The change is focused and the commit messages are clear.
- [ ] I added or updated tests for the behavior I changed.
- [ ] I updated documentation (`README`, `docs/`, or comments) where needed.
- [ ] No secrets, machine-specific paths, or build artifacts are included.
- [ ] New `unsafe` blocks have `// SAFETY:` comments.

## User-visible behavior

<!-- Anything users will notice, including configuration or compatibility changes. -->
