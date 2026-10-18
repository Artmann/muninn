# Muninn

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) for commit
messages: `<type>(<optional scope>): <description>`, with a lowercase
imperative description and no trailing period.

- Types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`, `ci`,
  `chore`.
- Scopes, when one fits: `ingest`, `recall`, `delete`, `index`, `store`,
  `embed`, `boot`, `webhook`, `docker`.
- Breaking API changes get a `!` after the type or scope and a
  `BREAKING CHANGE:` footer.

Examples:

```
feat(ingest): replace datapoints by sourceId
fix(recall): skip datapoints whose content is missing
docs: document the delete endpoint
```
