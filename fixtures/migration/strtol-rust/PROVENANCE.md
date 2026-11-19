# Dependency-contract fixture

The original probe calls the host C library's actual `strtol`. Its caller requires
whole-string decimal int32 parsing, ASCII leading whitespace and a sign, with
separate invalid-input and range-error outcomes. This is a caller-specific contract,
not the complete strtol API.

The replacement uses Rust's actual standard-library `str::parse::<i32>()` with a
small lexical adapter. Rust parsing directly without the adapter does not implement
the caller's whitespace contract. Fixture probes and the adapter are authored for
this repository; this is not an automatically generated dependency port.

References:
- https://doc.rust-lang.org/std/primitive.i32.html#method.from_str
- https://doc.rust-lang.org/std/num/enum.IntErrorKind.html

The build executor/registry search and ambient dynamic runtime fingerprinting are
outside this first campaign implementation. The agent/client prepares executables;
the server fingerprints their bytes and executes/compares their JSON observations.
