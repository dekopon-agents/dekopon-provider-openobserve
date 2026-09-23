# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- Remove arbitrary SQL, raw predicates and caller-selected URL, organization and stream.
  The four remaining capabilities generate bounded queries using broker-owner settings
  supplied during invoke. Requires a broker with `dekopon:settings/config@0.1.0`.
- Do not echo upstream error bodies to the model; classify by status instead.

## [0.3.0] - 2026-09-20

### Changed

- Upgrade to provider SDK 0.18.0 and HTTP WIT 1.1.0; caller behavior is unchanged.
