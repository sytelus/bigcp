# ADR 0020: Support only NTFS and ReFS

**Status:** Accepted

## Context

FAT-family timestamp and feature compromises would infect every decision.

## Decision

Reject every other filesystem before tree copy.

## Consequences

Exact FILETIME and file-ID semantics remain straightforward; external legacy media needs another tool.
