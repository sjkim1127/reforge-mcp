# musl abs source fixture

This is the unchanged `src/stdlib/abs.c` from musl's GitHub mirror:

- Repository: https://github.com/ifduyue/musl
- Pinned commit: `b1efda5b91735e33635376ca11c9b497a1c66e39`
- Source: https://github.com/ifduyue/musl/blob/b1efda5b91735e33635376ca11c9b497a1c66e39/src/stdlib/abs.c
- Official source: https://git.musl-libc.org/cgit/musl/tree/src/stdlib/abs.c
- License: MIT; the upstream COPYRIGHT file is included unchanged.

This validates one real standard-library function, not a complete musl port or
an external-project acceptance corpus. The host's system stdlib header is used;
Clang records its include dependencies and their hashes. Builtin replacement is
disabled so the original harness calls the compiled musl definition.

Defined-input cases include zero, both signs, near-boundary values, INT_MAX and
INT_MIN+1. INT_MIN is tested separately and must block acceptance because its
negation is undefined for signed int.
