# ADR 0017: EA transfer through buffered backup handles

**Status:** Accepted

## Context

EAs need an official API and overlapped/no-buffering handles are incompatible with BackupRead.

## Decision

Use separate synchronous buffered BackupRead/BackupWrite handles and copy only BACKUP_EA_DATA.

## Consequences

EA work is off the common zero-EA copy path and preserves opaque payloads.
