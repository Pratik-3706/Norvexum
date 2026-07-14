---
name: planning
description: Guidelines for creating implementation plans and tracking tasks using task lists to ensure structured and transparent execution of complex requests.
trigger_patterns:
  - "plan"
  - "complex task"
  - "architecture"
  - "refactor"
---

# Planning and Task Tracking

When a user requests a complex change, major feature addition, or architectural refactor on a **large or complex project**, you must stop and create a structured plan before executing any code changes.

**CRITICAL RULE:** Only use this structured planning and task tracking workflow for big projects! For small tweaks, minor bug fixes, or simple inquiries, DO NOT create an `implementation_plan.md` or `task.md`. Just do the work directly.

## 1. Create an Implementation Plan
Before writing or modifying any code, create a file named `implementation_plan.md` in the workspace. This document should serve as a technical design document for the user to review.

Format the plan as follows:
- **Goal:** A brief summary of the objective.
- **Proposed Changes:** Break down the changes by component or file. Be specific about what will be modified, created, or deleted.
- **Open Questions:** List any ambiguities or design decisions that require user input.

**Crucial Step:** Once the plan is created, explicitly ask the user for approval. **DO NOT proceed with execution until the user approves the plan.**

## 2. Create a Task List
Once the user approves the implementation plan, create a file named `task.md` in the workspace to serve as your checklist.

Use the following format to track progress:
- `[ ]` Uncompleted tasks
- `[/]` In-progress tasks
- `[x]` Completed tasks

Example:
```markdown
- `[x]` Set up database schema
- `[/]` Implement authentication middleware
- `[ ]` Create frontend login page
```

## 3. Execution & Tracking
As you execute the plan:
1. Update `task.md` continuously. Change `[ ]` to `[/]` when starting a task, and to `[x]` when finishing it.
2. If you discover unforeseen issues, update the `implementation_plan.md` and ask for user approval if the scope significantly changes.
3. Keep the user informed of your progress by referencing the task list.
