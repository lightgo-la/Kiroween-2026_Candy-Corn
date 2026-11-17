# Compiler Rules

Candy-Corn parses the `- ` bullets of this file directly into the constraint AST (ConstraintSubAST) and injects them
(with the `--steering` CLI option, or loaded automatically when the GUI starts).

## Constraints
- Do not infer anything not in the data
- Do not change the field names or types of the output schema

## Schema protection rules (explanation for people, not bullets)
Required fields of the output schema cannot be empty, and the linter removes duplicate and blank fields.
