# zombiezen/redo Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog][],
and this project adheres to [Semantic Versioning][].

[Keep a Changelog]: https://keepachangelog.com/en/1.0.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html
[Unreleased]: https://github.com/zombiezen/redo-rs/compare/v0.2.1...HEAD

## [0.2.1][] - 2021-12-04

Version 0.2.1 fixes several reliability issues with redo.

[0.2.1]: https://github.com/zombiezen/redo-rs/releases/tag/v0.2.1

### Fixed

- `redo` and `redo-ifchange` no longer send logs to stderr after they have exited,
  nor do they leave zombie `redo-log` processes.
- Fixed various path manipulation issues.

## [0.2.0][] - 2021-11-19

Version 0.2 contains various stability fixes.
Most of the improvements are to the codebase's maintainability,
but there are some user-visible fixes.

[0.2.0]: https://github.com/zombiezen/redo-rs/releases/tag/v0.2.0

### Added

- `redo --version` displays the program's version.
- Added [Code of Conduct](https://github.com/zombiezen/redo-rs/blob/main/CODE_OF_CONDUCT.md).

### Changed

- Backtraces are no longer shown when `RUST_BACKTRACE=1` is set.

### Fixed

- Reduced likelihood of "database locked" errors
  by acquiring write lock at start of most transactions.
- Fix improperly formatted error messages at exit
  ([#10](https://github.com/zombiezen/redo-rs/issues/10)).

## [0.1.0] - 2021-10-31

Version 0.1 was the first release.
It passes the entire apenwarr/redo test suite.

[0.1.0]: https://github.com/zombiezen/redo-rs/releases/tag/v0.1.0
