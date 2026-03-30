<p align="center"><code>npm i -g @openai/codex</code><br />or <code>brew install --cask codex</code></p>
<p align="center"><strong>Codex CLI</strong> is a coding agent from OpenAI that runs locally on your computer.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
If you want Codex in your code editor (VS Code, Cursor, Windsurf), <a href="https://developers.openai.com/codex/ide">install in your IDE.</a>
</br>If you want the desktop app experience, run <code>codex app</code> or visit <a href="https://chatgpt.com/codex?app-landing-page=true">the Codex App page</a>.
</br>If you are looking for the <em>cloud-based agent</em> from OpenAI, <strong>Codex Web</strong>, go to <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## Quickstart

### Installing and running Codex CLI

Install globally with your preferred package manager:

```shell
# Install using npm
npm install -g @openai/codex
```

```shell
# Install using Homebrew
brew install --cask codex
```

Then simply run `codex` to get started.

<details>
<summary>You can also go to the <a href="https://github.com/openai/codex/releases/latest">latest GitHub Release</a> and download the appropriate binary for your platform.</summary>

Each GitHub Release contains many executables, but in practice, you likely want one of these:

- macOS
  - Apple Silicon/arm64: `codex-aarch64-apple-darwin.tar.gz`
  - x86_64 (older Mac hardware): `codex-x86_64-apple-darwin.tar.gz`
- Linux
  - x86_64: `codex-x86_64-unknown-linux-musl.tar.gz`
  - arm64: `codex-aarch64-unknown-linux-musl.tar.gz`

Each archive contains a single entry with the platform baked into the name (e.g., `codex-x86_64-unknown-linux-musl`), so you likely want to rename it to `codex` after extracting it.

</details>

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Team, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## About This Fork

`dzianisv/codex` is a maintained fork of `openai/codex`. It regularly pulls in upstream changes, but it intentionally carries a smaller set of fork-only patches focused on runtime model/provider control, reflection experiments, and keeping the fork's release/CI workflow usable.

If you want the stock OpenAI project, use `openai/codex`. If you want the fork-specific behavior below, use this repository.

### What This Fork Adds Compared to `openai/codex`

- Runtime `/model` switching that applies provider, model, and reasoning-effort changes to the live session instead of forcing a fresh session before the change takes effect.
- Provider-authoritative `/model` discovery, including `models.dev` catalog integration plus fork fixes for GitHub Copilot, Azure alias handling, and safer fallbacks when a provider's `/models` response is incomplete or partially unsupported.
- Interrupted-session recovery for Azure-hosted reasoning turns: follow-up prompts and `codex resume --last` now drop orphan replayed reasoning items after an interrupt, preventing `required following item` request failures from stranding the session.
- Copilot-specific compatibility fixes around Claude-family fallback responses and tool-call wrapper normalization so tool activity is preserved as native Codex items instead of collapsing into plain text.
- An experimental reflection layer, including active-session reload support for reflection config; see [Reflection Layer](./docs/reflection.md) for configuration details, examples, and test coverage.
- Fork maintainer fixes around cached-source installs, npm staging, hosted-runner fallbacks, and other CI/release issues that affect this fork even when they are not part of upstream.

### Maintenance Notes

- Upstream `openai/codex` remains the source of truth for general Codex documentation and newly landed OpenAI-owned features.
- This fork keeps its divergence intentionally narrow: features that land upstream should be removed from the fork-specific patch stack instead of being carried forever.
- The fork is most useful if you need the runtime model-switching and provider-catalog work before upstream picks up equivalent behavior.

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)
- [**Reflection Layer**](./docs/reflection.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
