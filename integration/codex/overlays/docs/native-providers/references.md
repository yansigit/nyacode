# Native provider reference inventory

This inventory pins public reference repositories to immutable commits for the
native-provider work. It records source locations to inspect; it does not imply
that code is safe to copy or that a provider passed acceptance. Preserve each
repository's license and attribution if any material is reused. No credentials
or token-bearing fixtures were copied into this repository.

The snapshots were resolved and downloaded with `gh api` on 2026-10-10. The
downloaded archives and recursive GitHub tree responses are held outside this
repository under `/tmp/native-provider-refs/`.

| Reference | Pinned commit | License | Useful source locations | Notes |
| --- | --- | --- | --- | --- |
| [lidge-jun/opencodex](https://github.com/lidge-jun/opencodex) | `848ec37ac230f8d93465c2358ba651a7f9f8764f` | MIT | `src/adapters/cursor/`; `src/adapters/anthropic/`; `tests/providers/cursor/`; `tests/claude-integration/` | Detailed Cursor protocol and Claude/OpenAI compatibility reference. It is a large application; use focused source review, not wholesale porting. Exact Cursor paths match the earlier local protocol notes in `/tmp/opencodex.md`. |
| [pleaseai/shunt](https://github.com/pleaseai/shunt) | `e147f9b9c13b7072c04b1330bd1e7fd61d63b4c5` | Apache-2.0 OR MIT | `crates/`; `blueprints/upstream/{anthropic,cursor,antigravity}.md`; `docs/notes/anthropic-thinking-signatures.md`; `docs/research/anthropic-responses-translation-audit.md` | Rust translation/proxy reference. Treat behavior described in blueprints as project-specific, and distinguish its tests from real upstream acceptance. |
| [1jehuang/jcode](https://github.com/1jehuang/jcode) | `29d17a9aee1851d90d8a7a061d4535e2d1864994` | MIT | `crates/jcode-base/src/auth/`; `crates/jcode-base/src/provider/`; `crates/jcode-app-core/src/protocol_tests/`; `crates/jcode-base/src/compaction_tests.rs` | Rust agent with broad auth, provider, persistence, and protocol test surfaces. Several tests use fake credentials/local servers; these are not live-provider proof. |
| [Rahularya01/pi-antigravity](https://github.com/Rahularya01/pi-antigravity) | `d59d22bfebd9e44f2fd4a31bf5954a23fb9b043a` | MIT | `src/auth/{oauth,accounts}.ts`; `src/client/client.ts`; `src/models/{discovery,grouping}.ts`; `src/stream/stream.ts`; `docs/UPSTREAM_DYNAMIC_MODEL_DISCOVERY.md` | Pi inference extension: browser OAuth callback, multi-account selection, Antigravity project/model discovery, model grouping, runtime routing, and streaming. Fixture scripts and opt-in live smoke scripts are distinct evidence levels. |
| [cortexkit/antigravity-auth](https://github.com/cortexkit/antigravity-auth) | `87d3b14e5881615281151810744f892c4a72e5e4` | MIT | `packages/core/src/antigravity/`; `packages/core/src/account-{manager,repository}*.ts`; `packages/core/src/agy-transport*.ts` | Antigravity OAuth, account store, and transport reference. Mostly package-level tests; inspect protocol/provider licensing and upstream terms independently. |
| [earendil-works/pi](https://github.com/earendil-works/pi) | `4130bd5b32dac1d2d84697af1c4b587e133d488c` | MIT | `packages/ai/src/providers/opencode-go.ts`; `packages/ai/src/providers/opencode-go.models.ts`; `packages/ai/src/auth/oauth/anthropic.ts`; `packages/ai/src/api/anthropic-messages.ts` | The source defines a static OpenCode Go catalog and no live `/models` endpoint. Locally installed `@earendil-works/pi-ai` 1.0.0 includes generated `dist/providers/data/opencode-go.json`; its Bun lock entry records integrity `sha512-3/W1vdDaVtpeMd23ElvJC12HLA5yS/BGqqcXF+0SK082dN7cbgNcCwguTBRBC258Ke8SzSvUW1B75iAf8w8IxA==`. The generated catalog is distribution evidence, not tracked source at the pinned commit. |
| [fitchmultz/pi-cursor-sdk](https://github.com/fitchmultz/pi-cursor-sdk) | `eee1ae42d41011ff3edc3994d9b0eed8dcafd85d` | MIT | `src/cursor-provider.ts`; `src/cursor-provider-live-run-drain.ts`; `src/cursor-session-agent-resume.ts`; `src/cursor-native-replay-*`; `docs/cursor-native-tool-replay.md`; `docs/cursor-live-smoke-checklist.md` | Pi extension over `@cursor/sdk` 1.0.37 with local/cloud modes, SDK event lifecycle, tool/MCP bridge, and resume/replay support. It abstracts Cursor wire details; use OpenCodex for raw Connect/protobuf evidence. |
| [yansigit/pi-multiprovider](https://github.com/yansigit/pi-multiprovider) | `73b36cbc9f0a86877c835cb974a5c27c4548ca22` | MIT | `extensions/multiprovider.ts`; `src/multilogin.ts`; `src/auth-store.ts`; tests under `tests/` | Local integration example for multiple credentials and native provider-owned auth stores. This is contextual evidence, not a source of provider wire specifications. |

`OpenCodex` was ambiguous by name. The intended protocol reference here is
`lidge-jun/opencodex`: its pinned tree contains the Cursor adapter paths named
in the existing local Cursor/Antigravity protocol notes. The archived
`thyjeff/opencodex` repository was also inspected
(`bec5875825b8ed709edadaef2367b67ef1c68ae4`, MIT), but its scope is a Codex
App compatibility proxy and it is not the source of those protocol notes.

The checked-in inventory is deliberately metadata and path-level attribution;
the temporary archive directory is not a vendored dependency. When evidence is
used, cite the exact pinned path and license in the implementation change.

The user-confirmed reference set includes lidge-jun/opencodex,
pleaseai/shunt, 1jehuang/jcode, fitchmultz/pi-cursor-sdk, and
Rahularya01/pi-antigravity. cortexkit/antigravity-auth and earendil-works/pi
remain complementary references for reusable account storage and OpenCode Go
or Claude wire examples respectively.
