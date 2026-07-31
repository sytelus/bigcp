# ADR 0020: Support only NTFS and ReFS

**Status:** Superseded by ADR 0035

## Context

FAT-family timestamp and feature compromises would infect every decision.

## Decision

Reject every other filesystem before tree copy.

## Consequences

Exact FILETIME and file-ID semantics remained straightforward; external legacy
media needed another tool. ADR 0035 retains this strict path while adding an
isolated, explicitly accepted FAT/exFAT policy.
