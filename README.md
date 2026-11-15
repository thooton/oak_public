# oak_public

<!-- NOTE: This file MUST NOT be edited by AI agents, EVER! -->

Oak is a minimalist and performant agent harness written in Rust.

Key features include:

- **Minimal toolset and system prompt:** Oak's system prompt and tool list are only 6.0k tokens, compared to Claude Code's 45.9k tokens, OpenCode's 8.7k tokens, and Pi's ____ tokens. Less tokens, less cache input cost, more space for your agents to work.
- **Smart context:** In long-running agent sessions, leaving the full transcript in context can get expensive. But the alternative, session compaction, is often lossy and results in performance degradation. Oak offers an alternative: when the session gets too large, the top half of the session is summarized, while the bottom half is kept verbatim. This enables the model to maintain continuity and performance. In addition, because the summarization is performed in the background, while the session continues execution, the delay traditionally associated with compactions is not experienced.
- **Account rotation:** Add multiple accounts from the same provider, and experience seamless account rotation enabling sessions to run uninterrupted even in the face of usage limits on one account.
- **Designed for performance:** Oak leverages the power of native execution to start instantly, load conversations in a flash, and respond to clicks and keystrokes on the next display refresh. Whatever you're doing, Oak eliminates friction and keeps you in flow.
- **Robust extensibility support:** Oak exposes a simple C API that allows plugins to be developed in any language, installed without Oak recompilation, and dynamically loaded at startup for an easy and straightforward customization experience.
- **Super-agents:** Due to agent verbosity and human attention limits, we can only effectively coordinate a few agents at a time. Instead of manually managing agents, for large projects, use Oak's super-agent capability. Text the super-agent describing what you want, and it will spawn one or more agents to accomplish the task. Change your mind later or decide to add a new feature, and the super-agent will message existing agents or spawn more agents as appropriate. The super-agent only communicates with you when it needs a decision or when a task is complete, and it texts only in short messages that don't take long to read.

... and a lot more!

## Installation

Releases can be found for Windows, Mac, and Linux [on the Releases tab](https://github.com/thooton/oak_public/releases/).

## Contributing

Both AI-generated/AI-assisted and human contributions to the project are welcome! Simply create an issue or pull request.

## Screenshots
