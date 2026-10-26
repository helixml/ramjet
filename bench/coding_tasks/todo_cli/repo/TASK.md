# Add `--priority` filter and `--sort due` to `todo list`

The `todo` CLI (`python3 -m todo list --file tasks.json`) prints open tasks
from a JSON file. Users want to narrow the list by priority and see what is due
soonest. Please add two options to the `list` command.

The task file is a JSON array of objects:

```json
[
  {"id": 3, "title": "Write report", "priority": "high", "due": "2026-03-01", "done": false},
  {"id": 1, "title": "Buy milk"}
]
```

`priority` is one of `low`, `medium`, `high` and may be missing: a task without
`priority` is treated as `medium` everywhere. `due` is an ISO date
`YYYY-MM-DD` and may be missing or `null`. `done` defaults to `false`.

## `--priority LEVEL`

Only list tasks whose (effective) priority equals `LEVEL`. `LEVEL` must be one
of `low`, `medium`, `high`; anything else is a usage error (argparse error,
exit status 2).

## `--sort {id,due}`

`id` is the default and the current behaviour: ascending by `id`.

`due` sorts by due date, earliest first. Tasks without a due date come after
all tasks that have one. Ties (same due date, or both missing) are broken by
ascending `id`.

## Unchanged behaviour

- Completed tasks are hidden unless `--all` is given. `--all`, `--priority`
  and `--sort` can be combined freely.
- Each task is printed on one line, exactly as `format_task` does today, e.g.
  `3. [ ] Write report (priority: high, due: 2026-03-01)` — a missing priority
  is printed as `medium`, a missing due date as `-`, and completed tasks are
  marked `[x]`.
- If no task matches, print exactly `No tasks.`
- `main(argv)` returns the exit status `0` on success.
