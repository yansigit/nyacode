> Nyacode fork: Python distribution names, imports, and exported types remain compatibility interfaces. Published `openai-codex` packages belong to upstream. To run this fork, use the local SDK source with `CodexConfig(codex_bin="/absolute/path/to/nyacode")`. The upstream installation and release examples below are retained as reference; they do not install Nyacode.

# OpenAI Codex Python SDK

Build Python applications that start Codex threads, run turns, stream progress,
and control workspace access.

## Install

Install the SDK:

```bash
pip install openai-codex
```

## Quickstart

The SDK reuses your existing Codex authentication when one is already
available:

```python
from openai_codex import Codex

with Codex() as codex:
    thread = codex.thread_start()
    result = thread.run("Explain this repository in three bullets.")
    print(result.final_response)
```

`thread.run(...)` returns a `TurnResult` containing the final response,
collected items, and token usage.

## Authentication

Existing Codex authentication is reused automatically. To start ChatGPT
browser login explicitly:

```python
from openai_codex import Codex

with Codex() as codex:
    login = codex.login_chatgpt()
    print(login.auth_url)
    print(login.wait().success)
```

For device-code login:

```python
with Codex() as codex:
    login = codex.login_chatgpt_device_code()
    print(login.verification_url, login.user_code)
    login.wait()
```

For API-key login:

```python
with Codex() as codex:
    codex.login_api_key("sk-...")
```

## Built-In Help

Use Python's standard `help(openai_codex)`, `help(Codex)`, or
`python -m pydoc openai_codex` documentation tools.

## Documentation

For SDK development, run `uv sync --only-group dev --frozen` from this
directory. The `.python-version` file selects Python 3.14.8 for tests and
schema generation. The current `datamodel-code-generator` release cannot
run on Python 3.15 ([upstream issue #4589](https://github.com/datamodel-code-generator/datamodel-code-generator/issues/4589)).
This development toolchain pin does not change the SDK's supported runtime
versions.

Prepare the engine with `python3 scripts/engine.py prepare` from the product
repository root before schema generation or SDK tests. The maintenance script uses
schemas under `.engine/workspace` by default; `NYACODE_ENGINE_ROOT` selects another
prepared workspace, and `--schema-dir` overrides either default. Set
`CODEX_EXEC_PATH` to the packaged CLI being qualified when running SDK tests.

- [Getting started](https://github.com/openai/codex/blob/main/sdk/python/docs/getting-started.md)
- [API reference](https://github.com/openai/codex/blob/main/sdk/python/docs/api-reference.md)
- [Untrusted external messages](https://github.com/openai/codex/blob/main/sdk/python/docs/api-reference.md#externalmessage)
- [FAQ](https://github.com/openai/codex/blob/main/sdk/python/docs/faq.md)
- [Examples](https://github.com/openai/codex/blob/main/sdk/python/examples/README.md)

The package is licensed under the
[repository Apache License 2.0](https://github.com/openai/codex/blob/main/LICENSE).
