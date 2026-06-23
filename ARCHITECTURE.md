# Architecture

## Architecture

```
Workspace (Cargo)
  ├─ crates/discovery   PHP test discovery (tree-sitter-php)
  │                     · class graph + transitive-inheritance BFS
  │                     · #[Test], @test, #[DataProvider], @dataProvider,
  │                       #[TestWith], @testWith, #[Group], @group
  │                     · custom-framework TestCase bases
  ├─ crates/runner      proust binary
  │   ├─ phpunit_xml    bootstrap, <testsuites>, <php><const/env/server/ini>,
  │   │                 <groups><exclude>, <listeners>
  │   ├─ provider_enum  pre-fork PHP pass to count provider rows
  │   ├─ fork_pool      pipe-managed N-slot fork pool (CLOEXEC, PDEATHSIG,
  │   │                 process-group kill, class-map temp file)
  │   ├─ runner         work-stealing queue, LPT bin-packing, row split
  │   ├─ mock_bake      PSR-4 resolver + --bake-mocks preprocessing
  │   └─ reporter       TTY progress + summary (mpsc-driven)
  ├─ crates/mock_baker  tree-sitter createMock() → anonymous-class rewriter
  └─ crates/analyzer    static PHP coverage via mago AST
                        · per-test attribution
                        · Clover / JSON output (--features coverage)

PHP master (php/worker_fork.php)
  ├─ Load autoload + bootstrap + project constants ONCE
  ├─ Install SIGTERM/SIGINT/SIGHUP handlers → kill children → exit
  └─ pcntl_fork() × N → children inherit the warmed interpreter via COW

PHP child (one of N)
  ├─ Read newline-delimited BatchPlan JSONs on its stdin pipe
  ├─ For each plan: require_once test file, TestExecutor::runClass(...)
  ├─ Stream TestOutcome JSON lines on its stdout pipe
  ├─ Emit {"batch_done": true} between plans (work-stealing ready signal)
  └─ Exit cleanly on EOF (Rust closed our stdin)
```

The Rust master holds a `VecDeque<BatchPlan>` and one reader thread per
child. Each reader forwards `(slot, TestOutcome | BatchDone | Eof)` over
an `mpsc` channel to the main dispatcher loop, which sends the next plan
to whichever child reported `BatchDone` first. When the queue empties,
idle slots get their stdin pipes closed and the children exit on EOF.

Heavy data-provider methods (≥ 15 enumerated rows) are split into up to 4
stride-partitioned plans, each running on a different worker via the
existing `RowFilter` (`chunk_index % total_chunks`) inside `TestExecutor`.
Plain methods stay in a single class-level plan (splitting them would
multiply the `setUpBeforeClass` cost without paying for itself).
