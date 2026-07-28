# Changelog

All notable changes follow Keep a Changelog. The project uses semantic
versioning once its 1.0 release gates are complete.

## [Unreleased]

### Added

- Initial pre-1.0 Windows implementation: safe Win32 boundary, NTFS/ReFS
  preflight, iterative join, bounded workers, atomic temp publication,
  stream/EA/sparse/reparse handling, CRC journal, JSONL audit, JSON report,
  two verification forms, CLI, TUI, and independent sandbox testkit.
- Structurally confined unit/end-to-end tests and maintainer documentation.

### Known pre-1.0 work

- IOCP/no-buffering engine and its sans-I/O model, comprehensive fault/chaos
  harness, elevated filesystem matrix, differential copier, and performance
  evidence remain release gates. See `PLAN_DEVIATIONS.md`.
