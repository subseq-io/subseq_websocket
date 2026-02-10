# Agent Guidelines (subseq_websocket)

This file stores durable, repo-specific guardrails for subseq_websocket.

## Browser Authentication Contract
- Do not rely on custom Authorization headers for browser websocket handshakes.
- Support protocol-native auth for browser clients: URL token query and auth-init frame paths.
- Keep authenticated channel dispatch blocked until auth succeeds.

## Integration Contract
- Preserve compatibility for services that consume user-targeted websocket events.
- Keep auth extraction and actor binding behavior explicit and testable in handshake/runtime flows.
