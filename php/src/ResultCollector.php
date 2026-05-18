<?php

declare(strict_types=1);

namespace PhpunitRust;

use PHPUnit\Event\Test\ConsideredRisky;
use PHPUnit\Event\Test\ConsideredRiskySubscriber;
use PHPUnit\Event\Test\Errored;
use PHPUnit\Event\Test\ErroredSubscriber;
use PHPUnit\Event\Test\Failed;
use PHPUnit\Event\Test\FailedSubscriber;
use PHPUnit\Event\Test\Finished;
use PHPUnit\Event\Test\FinishedSubscriber;
use PHPUnit\Event\Test\MarkedIncomplete;
use PHPUnit\Event\Test\MarkedIncompleteSubscriber;
use PHPUnit\Event\Test\Passed;
use PHPUnit\Event\Test\PassedSubscriber;
use PHPUnit\Event\Test\PreparationStarted;
use PHPUnit\Event\Test\PreparationStartedSubscriber;
use PHPUnit\Event\Test\Skipped;
use PHPUnit\Event\Test\SkippedSubscriber;
use PHPUnit\Event\Code\TestMethod;

/**
 * Long-lived collector registered with PHPUnit's Facade once at worker boot.
 * Collects outcomes for the *current* request; reset() must be called by the
 * worker between requests.
 *
 * Because PHP does not support method overloading, we cannot implement
 * multiple typed notify() interfaces on a single class. Instead, each
 * subscriber interface is implemented by a small typed adapter class (defined
 * below) that delegates back to this collector. Call subscribers() to get the
 * eight adapters to pass to Facade::registerSubscribers().
 */
final class ResultCollector
{
    /** @var array<int, array{class:string,method:string,dataset:?string,status:string,message:?string,trace:?string,duration_ms:float}> */
    private array $outcomes = [];

    /** @var array<string, float> Map of TestMethod::id() → start microtime */
    private array $startTimes = [];

    /** @var array<string, string> Map of TestMethod::id() → outcome status already recorded */
    private array $recorded = [];

    public function reset(): void
    {
        $this->outcomes   = [];
        $this->startTimes = [];
        $this->recorded   = [];
    }

    /** @return list<array<string, mixed>> */
    public function outcomes(): array
    {
        return $this->outcomes;
    }

    /**
     * Return the eight typed subscriber adapters to register with Facade.
     *
     * @return list<\PHPUnit\Event\Subscriber>
     */
    public function subscribers(): array
    {
        return [
            new ResultCollectorPreparationStartedSubscriber($this),
            new ResultCollectorPassedSubscriber($this),
            new ResultCollectorFailedSubscriber($this),
            new ResultCollectorErroredSubscriber($this),
            new ResultCollectorSkippedSubscriber($this),
            new ResultCollectorMarkedIncompleteSubscriber($this),
            new ResultCollectorConsideredRiskySubscriber($this),
            new ResultCollectorFinishedSubscriber($this),
        ];
    }

    /** @internal called by the typed subscriber adapters */
    public function onPreparationStarted(PreparationStarted $event): void
    {
        $this->startTimes[$event->test()->id()] = microtime(true);
    }

    /** @internal called by the typed subscriber adapters */
    public function onPassed(Passed $event): void
    {
        $this->record($event->test(), 'pass', null, null);
    }

    /** @internal called by the typed subscriber adapters */
    public function onFailed(Failed $event): void
    {
        $this->record($event->test(), 'fail', $event->throwable()->message(), $event->throwable()->stackTrace());
    }

    /** @internal called by the typed subscriber adapters */
    public function onErrored(Errored $event): void
    {
        $this->record($event->test(), 'error', $event->throwable()->message(), $event->throwable()->stackTrace());
    }

    /** @internal called by the typed subscriber adapters */
    public function onSkipped(Skipped $event): void
    {
        $this->record($event->test(), 'skipped', $event->message(), null);
    }

    /** @internal called by the typed subscriber adapters */
    public function onMarkedIncomplete(MarkedIncomplete $event): void
    {
        $this->record($event->test(), 'incomplete', $event->throwable()->message(), $event->throwable()->stackTrace());
    }

    /** @internal called by the typed subscriber adapters */
    public function onConsideredRisky(ConsideredRisky $event): void
    {
        // Risky can fire multiple times per test; only record first.
        if (!isset($this->recorded[$event->test()->id()])) {
            $this->record($event->test(), 'risky', $event->message(), null);
        }
    }

    /** @internal called by the typed subscriber adapters */
    public function onFinished(Finished $event): void
    {
        // If we got here with no outcome recorded, the test was prepared
        // but never produced an outcome event — synthesize an error.
        $id = $event->test()->id();
        if (!isset($this->recorded[$id]) && $event->test() instanceof TestMethod) {
            $this->record($event->test(), 'error', 'no outcome reported by PHPUnit', null);
        }
    }

    private function record(\PHPUnit\Event\Code\Test $test, string $status, ?string $message, ?string $trace): void
    {
        if (!$test instanceof TestMethod) {
            return;
        }
        $id = $test->id();
        if (isset($this->recorded[$id])) {
            return; // first wins (e.g., Failed before Risky)
        }
        $this->recorded[$id] = $status;

        $start    = $this->startTimes[$id] ?? microtime(true);
        $duration = (microtime(true) - $start) * 1000.0;

        $dataset  = null;
        $testData = $test->testData();
        if ($testData->hasDataFromDataProvider()) {
            $name    = $testData->dataFromDataProvider()->dataSetName();
            $dataset = is_int($name) ? "#{$name}" : $name;
        }

        $this->outcomes[] = [
            'class'       => $test->className(),
            'method'      => $test->methodName(),
            'dataset'     => $dataset,
            'status'      => $status,
            'message'     => $message,
            'trace'       => $trace,
            'duration_ms' => $duration,
        ];
    }
}

// ---------------------------------------------------------------------------
// Typed subscriber adapters — one per PHPUnit subscriber interface.
// PHP cannot have two methods with the same name, so we cannot implement
// multiple notify(SpecificType) interfaces on a single class. These small
// adapters solve that by delegating to ResultCollector's on*() methods.
// ---------------------------------------------------------------------------

final class ResultCollectorPreparationStartedSubscriber implements PreparationStartedSubscriber
{
    public function __construct(private readonly ResultCollector $collector) {}
    public function notify(PreparationStarted $event): void { $this->collector->onPreparationStarted($event); }
}

final class ResultCollectorPassedSubscriber implements PassedSubscriber
{
    public function __construct(private readonly ResultCollector $collector) {}
    public function notify(Passed $event): void { $this->collector->onPassed($event); }
}

final class ResultCollectorFailedSubscriber implements FailedSubscriber
{
    public function __construct(private readonly ResultCollector $collector) {}
    public function notify(Failed $event): void { $this->collector->onFailed($event); }
}

final class ResultCollectorErroredSubscriber implements ErroredSubscriber
{
    public function __construct(private readonly ResultCollector $collector) {}
    public function notify(Errored $event): void { $this->collector->onErrored($event); }
}

final class ResultCollectorSkippedSubscriber implements SkippedSubscriber
{
    public function __construct(private readonly ResultCollector $collector) {}
    public function notify(Skipped $event): void { $this->collector->onSkipped($event); }
}

final class ResultCollectorMarkedIncompleteSubscriber implements MarkedIncompleteSubscriber
{
    public function __construct(private readonly ResultCollector $collector) {}
    public function notify(MarkedIncomplete $event): void { $this->collector->onMarkedIncomplete($event); }
}

final class ResultCollectorConsideredRiskySubscriber implements ConsideredRiskySubscriber
{
    public function __construct(private readonly ResultCollector $collector) {}
    public function notify(ConsideredRisky $event): void { $this->collector->onConsideredRisky($event); }
}

final class ResultCollectorFinishedSubscriber implements FinishedSubscriber
{
    public function __construct(private readonly ResultCollector $collector) {}
    public function notify(Finished $event): void { $this->collector->onFinished($event); }
}
