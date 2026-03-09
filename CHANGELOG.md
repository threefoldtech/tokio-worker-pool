# Changelog

## [0.1.0] - 2026-03-09

### Added
- Formal project README with concepts, usage patterns, shutdown recipes, and
  internals documentation
- `Makefile` entrypoints for build, test, verification, stress runs, and
  release automation
- Criterion benchmark target and stress example for worker-pool validation

### Changed
- Worker pool internals now use native async trait return-position impl trait
  instead of `async-trait`
- Worker lifecycle and shutdown behavior are documented as part of the public
  project interface

### Fixed
- Shutdown deadlocks caused by buffered worker slots retained during drain
- Lost wakeup race while waiting for all workers to exit
- Worker exit race during shutdown when availability was published concurrently
  with pool drain
- Worker count accounting during panic unwinding
