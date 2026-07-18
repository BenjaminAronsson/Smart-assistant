# 10 — External references

Carried forward from the v1 baseline (accessed 17 July 2026), plus Rust-stack additions.
Project status, licenses, and provider terms change — recheck before implementation of the
relevant milestone and before any redistribution.

## Protocols and platforms

1. Model Context Protocol — architecture: https://modelcontextprotocol.io/docs/learn/architecture
2. MCP security best practices: https://modelcontextprotocol.io/docs/tutorials/security/security_best_practices
3. Official MCP Rust SDK (rmcp): https://github.com/modelcontextprotocol/rust-sdk
4. Home Assistant Assist / voice control: https://www.home-assistant.io/voice_control/
5. Home Assistant developer voice architecture: https://developers.home-assistant.io/docs/voice/overview/
6. Wyoming protocol and services: https://github.com/OHF-Voice/wyoming
7. Hyprland IPC documentation: https://wiki.hypr.land/IPC/

## Anthropic / model access

8. Claude Code setup and authentication: https://docs.anthropic.com/en/docs/claude-code/setup
9. Claude Code CLI reference (print mode, stream-json): https://docs.anthropic.com/en/docs/claude-code/cli-usage
10. Claude subscription vs API Console billing are separate: https://support.anthropic.com/en/articles/9876003-does-my-claude-ai-subscription-include-api-usage
11. Ollama API and tool calling (future adapter): https://docs.ollama.com/api/introduction
12. llama.cpp server (future adapter): https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md

## Voice engines (license-check model assets separately)

13. Silero VAD: https://github.com/snakers4/silero-vad
14. faster-whisper: https://github.com/SYSTRAN/faster-whisper
15. whisper.cpp: https://github.com/ggml-org/whisper.cpp
16. OHF Piper: https://github.com/OHF-Voice/piper1-gpl
17. openWakeWord: https://github.com/dscripka/openWakeWord

## Comparable systems (evidence base for the ADRs)

18. OpenClaw — gateway architecture and protocol: https://github.com/openclaw/openclaw/blob/main/docs/concepts/architecture.md
19. OpenClaw — overview and security guidance: https://github.com/openclaw/openclaw/blob/main/docs/index.md
20. OpenJarvis repository: https://github.com/open-jarvis/OpenJarvis
21. OpenJarvis research paper: https://arxiv.org/abs/2605.17172
22. Open Interpreter (safety notice): https://github.com/OpenInterpreter/open-interpreter
23. Open WebUI: https://docs.openwebui.com/features/ (license: https://github.com/open-webui/open-webui/blob/main/LICENSE)
24. AnythingLLM: https://github.com/Mintplex-Labs/anything-llm
25. OpenVoiceOS technical manual: https://openvoiceos.github.io/ovos-technical-manual/
26. Rhasspy 3 (archived — lessons only): https://github.com/rhasspy/rhasspy3
27. LiveKit Agents (M7 option): https://docs.livekit.io/agents/
28. Pipecat: https://github.com/pipecat-ai/pipecat
29. Leon: https://github.com/leon-ai/leon

## Rust stack

30. tokio: https://tokio.rs · axum: https://github.com/tokio-rs/axum
31. sqlx: https://github.com/launchbadge/sqlx · pgvector crate: https://github.com/pgvector/pgvector-rust
32. fastembed-rs (CPU embeddings): https://github.com/Anush008/fastembed-rs
33. hyprland-rs: https://github.com/hyprland-community/hyprland-rs
34. schemars: https://github.com/GREsau/schemars
35. cargo-deny: https://github.com/EmbarkStudios/cargo-deny
