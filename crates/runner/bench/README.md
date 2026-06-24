# Benchmark harness

`run.sh` runs Proust + vanilla PHPUnit against the cloned smoke projects
inside Docker containers for each supported PHP version. Lets us measure
correctly across PHP 8.1-8.5 without modifying the host system.

## Prereqs

- Docker on `$PATH`
- `./target/release/proust` built (`cargo build --release` from repo root)
- Smoke projects cloned + composer-installed under `/tmp/proust-smoke/`:
  - `brick-math/` (https://github.com/brick/math)
  - `doctrine-collections/` (https://github.com/doctrine/collections)
  - `guzzle-psr7/` (https://github.com/guzzle/psr7)

## Run

```bash
# Full matrix (PHP 8.1-8.5 × 4 projects × 3 worker counts)
./bench/run.sh > bench/results.md

# Quick smoke (one PHP version, one worker count)
./bench/run.sh --quick
```

Output is a markdown table; pipe to a file and paste into the main README's
Performance section.

## Notes

- Container images are pulled on first run (~80 MB each, 5 versions).
- Vendor directories in the projects must already be composer-installed.
  If a project's vendor was installed against a different PHP version than
  the container's, runtime errors may surface (rare for our target projects).
- `/usr/bin/time -v` (GNU time) is bundled with the official `php:VERSION-cli`
  images so we don't need to install anything extra in the container.
- The script skips a project silently if its path does not exist under
  `/tmp/proust-smoke/` — only the `fixture` project (bundled in the
  repo) is always present.
