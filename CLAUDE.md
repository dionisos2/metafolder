# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Status

This project is in the **planning/specification phase**. There is currently no Rust code — only a README.md with architectural vision. The next step is to make key design decisions and start implementing.

## Planned Architecture

Four components (see README.md for details):

- **Library (`lib`)** — shared code between CLI and GUI (data structures, metadata logic, file hashing)
- **Server** — generic metadata management API for files
- **CLI** — git-style subcommand tool communicating with the server
- **GUI** — keyboard-driven (vim-style with `:` commands), one/two-panel file display with real-time metadata editing

When implementation starts, use a Cargo workspace with these as separate crates.

## Key Design Decisions (Open)

From the README, these architectural questions are unresolved:

- **Storage**: flat files vs. SQLite (README suggests checking what TagStudio does)
- **Repository model**: git-like repos with a chosen root folder; multiple repos on different drives can be loaded simultaneously
- **File identity**: hash-based UUID to track moved files
- **Operations log**: log file, database, or git repo for undo/rollback
- **Atomicity**: strategy for safe file operations
- **Metadata types and structure**: generic metadata system design
- **Related file groups**: handling sets like manga page folders or config dir collections
- **Virtual mounting**: folder mount based on metadata criteria

## Core Principles

- **Decentralized**: no central database; repositories per disk like git
- **No metadata inside source folders**: all metadata stored in dedicated repo directories
- **File integrity**: hash-based identity, atomic operations
