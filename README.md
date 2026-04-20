# basalt-plugin-copilot-agent

Basalt plugin: GitHub Copilot agent launcher.

## Installation

Download the latest `.wasm` from [Releases](https://github.com/adevcorn/basalt-plugin-copilot-agent/releases) and place it in `~/.config/basalt/plugins/`.

Or install via the Basalt plugin registry.

## Building from source

```bash
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/copilot_agent.wasm ~/.config/basalt/plugins/
```
