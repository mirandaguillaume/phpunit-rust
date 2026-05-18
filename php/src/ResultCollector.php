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
use PHPUnit\Event\Value\Test\TestMethod;

/**
 * Long-lived subscriber registered with PHPUnit's Facade once at worker boot.
 * Collects outcomes for the *current* request; reset() must be called by the
 * worker between requests.
 *
 * We implement multiple subscriber interfaces on one object so a single
 * registration suffices. PHPUnit's event dispatcher routes by the typed
 * `notify` parameter, so the right method fires for each event.
 */
final class ResultCollector implements
    PassedSubscriber,
    FailedSubscriber,
    ErroredSubscriber,
    SkippedSubscriber,
    MarkedIncompleteSubscriber,
    ConsideredRiskySubscriber,
    PreparationStartedSubscriber,
    FinishedSubscriber
{
    /** @var array<int, array{class:string,method:string,dataset:?string,status:string,message:?string,trace:?string,duration_ms:float}> */
    private array $outcomes = [];

    /** @var array<string, float> Map of TestMethod::id() → start microtime */
    private array $startTimes = [];

    /** @var array<string, string> Map of TestMethod::id() → outcome status already recorded */
    private array $recorded = [];

    public function reset(): void
    {
        $this->outcomes = [];
        $this->startTimes = [];
        $this->recorded = [];
    }

    /** @return list<array<string, mixed>> */
    public function outcomes(): array
    {
        return $this->outcomes;
    }

    public function notify(/* one of the event types above */ $event): void
    {
        // PHPUnit's dispatcher calls one of the typed notify(...) overloads
        // we declare below by interface. PHP doesn't support real method
        // overloading, so we route by event type here.
        if ($event instanceof PreparationStarted) {
            $this->startTimes[$event->test()->id()] = microtime(true);
            return;
        }
        if ($event instanceof Passed) {
            $this->record($event->test(), 'pass', null, null);
            return;
        }
        if ($event instanceof Failed) {
            $this->record($event->test(), 'fail', $event->throwable()->message(), $event->throwable()->stackTrace());
            return;
        }
        if ($event instanceof Errored) {
            $this->record($event->test(), 'error', $event->throwable()->message(), $event->throwable()->stackTrace());
            return;
        }
        if ($event instanceof Skipped) {
            $this->record($event->test(), 'skipped', $event->message(), null);
            return;
        }
        if ($event instanceof MarkedIncomplete) {
            $this->record($event->test(), 'incomplete', $event->throwable()->message(), $event->throwable()->stackTrace());
            return;
        }
        if ($event instanceof ConsideredRisky) {
            // Risky can fire multiple times per test; only record first.
            if (!isset($this->recorded[$event->test()->id()])) {
                $this->record($event->test(), 'risky', $event->message(), null);
            }
            return;
        }
        if ($event instanceof Finished) {
            // If we got here with no outcome recorded, the test was prepared
            // but never produced an outcome event — synthesize an error.
            $id = $event->test()->id();
            if (!isset($this->recorded[$id]) && $event->test() instanceof TestMethod) {
                $this->record($event->test(), 'error', 'no outcome reported by PHPUnit', null);
            }
            return;
        }
    }

    private function record($test, string $status, ?string $message, ?string $trace): void
    {
        if (!$test instanceof TestMethod) {
            return;
        }
        $id = $test->id();
        if (isset($this->recorded[$id])) {
            return; // first wins (e.g., Failed before Risky)
        }
        $this->recorded[$id] = $status;

        $start = $this->startTimes[$id] ?? microtime(true);
        $duration = (microtime(true) - $start) * 1000.0;

        $dataset = null;
        $testData = $test->testData();
        if ($testData->hasDataFromDataProvider()) {
            $name = $testData->dataFromDataProvider()->dataSetName();
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
