# `deep_merge` corrupts the defaults and handles overrides wrongly

`layered.deep_merge(base, override)` combines a default configuration with an
environment-specific override. Since we started merging several environments
in one process, the defaults dictionary itself gets changed and later merges
see values from earlier ones. Users also report that list settings are
concatenated instead of replaced, and that there is no way to remove a key.

Required semantics for `deep_merge(base, override)`:

1. Return a **new** dict. Neither `base` nor `override` may be modified, at any
   depth, and mutating the returned value (including nested dicts and lists in
   it) must not affect either input.
2. When both sides have a dict under the same key, merge them recursively.
3. Any other override value replaces the base value. In particular a list in
   the override **replaces** the base list (no concatenation), and a dict
   replaces a non-dict (and vice versa).
4. An override value of `None` **deletes** the key from the result (at any
   depth). Deleting a key the base does not have is a no-op. `None` values are
   therefore never present in the result when they come from the override,
   including inside a dict that the override introduces under a new key.
5. Keys present only in `base` are kept.

`merge_all(*configs)` folds `deep_merge` left to right and returns `{}` when
called with no arguments; it must not modify its arguments either.

The code lives in `layered/merge.py`.
