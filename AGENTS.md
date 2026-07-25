# Repository Guidelines

## Project Structure & Module Organization
- Core service lives in `src/`, with `main.rs` bootstrapping Axum, `server.rs` handling routes, `transform.rs` for resize/encode logic, `thumbnail.rs` for FFmpeg thumbnails, and `cache.rs` for dual-cache orchestration.
- Runtime assets such as cached originals and processed derivatives land in `cache/original` and `cache/processed`; clean them via `make clean` if you need a fresh slate.
- Scripts: `build.sh` ensures meson/ninja availability for AVIF, while the `Makefile` mirrors common workflows (`make build`, `make docker-build`).

## Build, Test, and Development Commands
- `./build.sh [run|--release]` compiles with AVIF prerequisites wired up; prefer this wrapper when meson/ninja live outside your PATH.
- `cargo run --release` runs the service directly (requires FFmpeg + dav1d deps satisfied).
- `make run` boots the binary with sensible defaults; `make docker-compose-up` mirrors production wiring via Compose.
- `cargo test` runs unit tests (add more coverage alongside new modules); `make test-health` pings `/health` for a quick smoke check.

## Coding Style & Naming Conventions
- Rust 2021 + async-first patterns; keep modules focused and avoid God files bigger than ~400 lines.
- Run `cargo fmt --all` and `cargo clippy --all-targets --all-features` before pushing; fix clippy warnings instead of allowing them.
- Use snake_case for functions/modules, SCREAMING_SNAKE_CASE for env vars (`CACHE_TTL_SECS`), and prefer descriptive struct names like `CacheEntry` over abbreviations.

## Testing Guidelines
- Place happy-path and error-case tests beside the code they cover inside the same module using `#[cfg(test)]` blocks; integration tests can live under `tests/` if they need the full pipeline.
- Test names follow `action_expectedResult` (e.g., `fill_transform_preserves_aspect`).
- Validate caching logic by asserting `X-Cache` headers and TTL behavior; include regression cases for semaphore limits in `thumbnail`.

## Commit & Pull Request Guidelines
- Follow the existing Conventional Commits style (`feat:`, `fix:`, `chore:`) visible in `git log`.
- PRs must describe behavior changes, reference issue IDs when available, and include manual/automated test evidence (command output snippets or screenshots for HTTP probes).
- Add configuration notes (env vars, cache paths, FFmpeg limits) to the PR body whenever they change runtime expectations.
