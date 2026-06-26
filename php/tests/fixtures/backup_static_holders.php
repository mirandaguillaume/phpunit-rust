<?php

declare(strict_types=1);

/*
 * Holders for the backupStaticProperties parity tests. Deliberately declared in
 * the GLOBAL namespace (NOT under Proust\*) so they mirror a real project's test
 * classes — which never live in proust's own `Proust\` namespace. That namespace
 * is exactly what TestExecutor::globalStateExcludeList() excludes from the static
 * snapshot, to stop a long-lived worker from restoring proust's own runtime
 * state (e.g. Proust\SharedTransactionalFixture::$sharedFixtureBuilt) mid-batch.
 * Placing the mutated holders here lets the test exercise the real snapshot/
 * restore path instead of silently hitting that worker-safety exclude.
 */

class _ParityStaticHolder
{
    public static int $counter = 0;
}

class _ParityNoBackupStaticHolder
{
    public static int $counter = 0;
}

class _ParityExcludedStaticHolder
{
    // $kept is carved out via #[ExcludeStaticPropertyFromBackup]; $alsoBackedUp is
    // a NON-excluded control that MUST be rolled back — so the test fails both if
    // the exclude is ignored (kept rolled back) AND if backup is dead entirely
    // (alsoBackedUp not rolled back).
    public static int $kept = 0;
    public static int $alsoBackedUp = 0;
}

class _ParityStaticPrecedenceHolder
{
    public static int $counter = 0;
}

class _ParityProustControlHolder
{
    public static int $counter = 0;
}

class _ParityMethodOptInHolder
{
    public static int $counter = 0;
}

class _ParityDocblockHolder
{
    public static int $counter = 0;
}

class _ParityDocblockLegacyHolder
{
    public static int $counter = 0;
}

class _ParityDocblockTrueHolder
{
    public static int $counter = 0;
}

class _ParityStaticObjectHolder
{
    public static ?\stdClass $obj = null;
}

class _ParityBothHolder
{
    public static int $counter = 0;
}

class _ParityThrowsHolder
{
    public static int $counter = 0;
}

class _ParityMethodExcludeHolder
{
    public static int $a = 0;
    public static int $b = 0;
}

class _ParityIsolatedHolder
{
    public static int $counter = 0;
}
