# Watchlist

People and projects whose next release could change how we serve models, and
what specifically to look for from each.

```bash
python3 bench/watchlist_scan.py            # what changed since you last looked
python3 bench/watchlist_scan.py --since 2026-08-01
python3 bench/watchlist_scan.py --all      # ignore state, show current state of everything
```

The scan queries the HuggingFace and GitHub APIs concurrently and prints only
what moved, with the `watch_for` line next to each change so you can triage
without opening anything. First run records a baseline and reports nothing —
on a fresh state file every source is "new", which is noise. State lives in
`.last-scan.json` (git-ignored); delete it to re-baseline.

No credentials needed. GitHub rate-limits anonymous callers to 60 requests an
hour, which the current registry fits inside; export `GITHUB_TOKEN` if you add
many more. Never pass a token in argv.

## Adding an entry

```yaml
  - name: short human label
    kind: hf-org | hf-repo | gh-repo | site
    id: z-lab | owner/model | owner/repo | https://...
    why: what this source has already given us
    watch_for: what a new artefact would have to be to matter
```

`watch_for` is the field that makes this useful, and `bench/test_watchlist_scan.py`
enforces that it is substantive. A scan that says "sgl-project/sglang was
pushed to" is worthless — the repo is pushed to daily. A scan that says that
next to *"watch for a tagged release shipping DFlash2, which would let us drop
the source bind-mount"* is a decision. Write the trigger, not the topic.

Keep entries honest about what we actually verified. Several here exist
because we tried the thing and it did not work: `RadixArk` publishes the W4A4
export we cannot run because DFlash2 rejects a quantized `lm_head`, and it is
listed precisely so we notice if a dense-head variant ever appears. That is
more valuable than a list of projects we admire.

## Scope

This tracks *sources of artefacts* — checkpoints, engines, kernels, recipes.
It is not a reading list and not a social graph. If an entry has not produced
something we could deploy, benchmark, or rule out, it does not belong here.

Findings from scanning these sources go in `EXPERIMENTS.md` like any other
result. The watchlist answers "what is new"; the journal answers "did it work".
