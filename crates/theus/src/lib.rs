//! theus: an interactive Rust terminal client for DeepSeek Harness.
//!
//! Modules: cli (clap surface), config (runtime resolution), headless
//! (theus run), app (the interactive TUI), eventmap (pure event->UI mapping),
//! ui (ratatui rendering), markdown (pulldown-cmark rendering).

pub mod app;
pub mod cli;
pub mod commands;
pub mod config;
pub mod eventmap;
pub mod headless;
pub mod markdown;
pub mod ui;
