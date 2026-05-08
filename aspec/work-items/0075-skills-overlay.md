# Work Item: Feature

Title: skills overlay
Issue: issuelink

## Summary:
- The current overlay system supports adding host directories and files to agent containers using flag, env var, or config files. This new type of overlay, which should work similarly to directories, but for agent skills. The overlayengine should gain a new 'skill' overlay type that allows the skills in the global skills directory (.amux/skills/) to be overlaid into an agent container via flag, env var, or config file. When overlaying skills, amux must put them in the correct directory within the agent container depending on which agent is running. Do research on each agent that amux supports, and ensure that the skill folder and file are mounted in the right place within /workspace/{} inside the container.

## User Stories

### User Story 1:
As a: [admin | user | other]

I want to:
description of task

So I can:
description of result


## Implementation Details:
- details


## Edge Case Considerations:
- considerations

## Test Considerations:
- considerations

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
