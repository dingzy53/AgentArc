# Embedded pricing snapshot

`pricing_snapshot.json` is a compacted snapshot of LiteLLM's
[`model_prices_and_context_window.json`](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json),
used as an offline fallback. Provider-prefixed ids are collapsed to their bare
model name when no unprefixed entry exists, because agent transcripts usually
record only the model id.

Refresh it at runtime with `agentarc pricing update` (cached in
`~/.cache/agentarc/pricing.json`); the embedded file is only the fallback when no
cache exists.
