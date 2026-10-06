# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.5.0] - 2026-10-06

### Changed

- Use the typed SDK 0.34.0 provider and stdio JSON/table output instead of manual WIT and the old HTTP facade.
- Fix agent and broker aggregate windows to five minutes, rejecting caller-supplied since values.
- Limit trace and broker-provider row reads to a 24-hour window.
- Bound unique session IDs to 50, turns to one row and aggregate groups to 50 with a default of 20.
- Remove per-actor usage grouping and actor_id from broker-denial groups.
- Clamp loaded-provider rows to the requested window and skip the second query for a future boot.
- Emit table results with exactly one terminal newline, matching JSON output.
- Show agent and broker command words in rendered action help and usage.

## [0.4.0] - 2026-09-23

### Changed

- Remove arbitrary SQL, raw predicates and caller-selected URL, organization and stream.
  The four remaining capabilities generate bounded queries using broker-owner settings
  supplied during invoke. Requires a broker with `dekopon:settings/config@0.1.0`.
- Do not echo upstream error bodies to the model; classify by status instead.

## [0.3.0] - 2026-09-20

### Changed

- Upgrade to provider SDK 0.18.0 and HTTP WIT 1.1.0; caller behavior is unchanged.
