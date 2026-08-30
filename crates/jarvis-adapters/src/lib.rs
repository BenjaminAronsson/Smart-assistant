#![deny(unsafe_code)]
//! Provider/tool adapters behind application ports: claude-cli, home-assistant,
//! mcp-host, wyoming, embeddings (docs/02 §3).

pub mod app_builder;
pub mod browser;
pub mod caldav;
pub mod claude_cli;
pub mod coding;
pub mod elevenlabs;
pub mod embeddings;
pub mod home_assistant;
pub mod host_env;
pub mod mcp_host;
pub mod media_mpris;
pub mod smtp;
pub mod spotify;
pub mod timer_alert;
pub mod tools;
pub mod web;
pub mod wyoming;
