# Browser sign-in and OAuth callbacks

This module owns browser sign-in startup, loopback callbacks, optional OAuth state
protection, and the attempt lifecycle. Plugins supply provider-specific
authorization parameters and callback paths, handle code exchange and account
verification, and own credentials. Outbound API requests use the host HTTP feature.

## Commands

These Tauri commands are available to the main webview only. Invoke `begin` with
these named arguments:

```js
const { id, callbackUrl } = await invoke("oauth_callback_begin", {
  authorizationUrl: "https://provider.example/authorize?client_id=buzz&response_type=code",
  callbackPath: "/oauth2redirect/provider",
  useState: true, // optional; this is the default
});
```

| Command | Contract |
| --- | --- |
| `oauth_callback_begin({ authorizationUrl, callbackPath, useState? })` | Validates the request, generates an attempt ID, binds `127.0.0.1` on an ephemeral port, inserts the callback URL as `redirect_uri` and optional state, and launches the external browser through the existing opener. Returns `{ "id": "…", "callbackUrl": "http://127.0.0.1:{port}{path}" }`. Launch failure cleans up before rejecting. |
| `oauth_callback_wait(id)` | Consumes the matching attempt's result once. Resolves with `{ "parameters": [["name", "value"], …] }`, retaining decoded order and duplicates. Valid provider errors resolve with their parameters; transport failure, timeout, or cancellation rejects with a generic message. |
| `oauth_callback_cancel(id)` | Closes and removes the matching attempt. Safe to repeat; a stale ID does not cancel another attempt. |

The authorization URL must use HTTPS, with no embedded credentials or fragment.
The callback path must be a canonical absolute path without a query or fragment.
Native code preserves other authorization parameters, including duplicates, while
adding the generated callback URL under `redirect_uri`. Caller-supplied
`state` or `redirect_uri` parameters are rejected. The attempt ID is a lifecycle
handle, separate from state; it is not included in the callback URL.
`callbackUrl` is the exact value inserted in
the authorization request. Plugins using standard OAuth must retain it and send
it unchanged as `redirect_uri` during token exchange, as required by
[RFC 6749 §4.1.3](https://www.rfc-editor.org/rfc/rfc6749.html#section-4.1.3).
Returning it does not transfer browser startup or state verification to the plugin.

Only one listener can be pending at a time. Callers must serialize their complete
begin/wait/cancel sequence: starting another attempt after a listener finishes
can replace its unconsumed result.

## State protection

By default, native code generates a separate cryptographically random 256-bit,
single-use state value and sends it in the authorization URL. Both success and
provider-error callbacks must contain exactly one matching state. Missing,
duplicate, or mismatched state receives a generic invalid-callback response and
leaves the legitimate attempt pending within its original timeout. Callback HTML
and error messages never expose state or other callback values.

`useState: false` omits state from the authorization URL and disables native state
validation entirely. This is an explicit custom-protocol compatibility exception,
not caller-owned state or an automatic fallback.

## Cleanup contract

Callers must await `begin` even if canceled during startup. Native startup can
already have opened the browser; once it returns, retain the ID and immediately
cancel if the caller was already aborted, without waiting for a callback. Otherwise,
cancel on abort and in final cleanup on every exit, including success and failure.
A rejected `begin` leaves no new attempt for the caller to clean up. Cancellation
is idempotent and affects only the matching ID; stale cleanup cannot close a newer
attempt.

The listener and expected state are cleared on completion, cancellation, timeout,
browser-launch failure, or the start of a main-document load. Loads in other
webviews leave the attempt intact. Listener cleanup does not close the external
browser or cancel separate host HTTP requests.

## Callback handling and limits

- Accepts GET requests using HTTP/1.0 or HTTP/1.1, with exactly one matching
  `Host` header and the exact selected path. Rejects `Origin` headers and URI
  fragments. Supports query responses, not POST/form responses.
- Requires one nonempty `code` of at most 4,096 decoded bytes, or an `error`
  parameter, after any enabled state validation. Preserves state, error details,
  and extension parameters in the returned result. Invalid callbacks leave the
  listener available.
- Bounds paths to 1,024 bytes, request reads to 8,192 bytes, each connection to
  five seconds, and the overall attempt to ten minutes.
- Returns static HTML with `no-store`, a restrictive Content Security Policy,
  and `no-referrer`.

## Standards scope

External browser authorization and the loopback HTTP redirect pattern follow
[RFC 8252 §§6, 7.3 and 8.3](https://www.rfc-editor.org/rfc/rfc8252.html#section-6):
an ephemeral port, binding only to loopback, and a listener limited to the attempt.
Caller-selected paths support the registered redirect requirement in
[§8.4](https://www.rfc-editor.org/rfc/rfc8252.html#section-8.4): a registered path
stays fixed while the loopback port may vary. A custom protocol can instead select
its own random path.

Success and error query parameters follow the response shape in
[RFC 6749 §§4.1.2 and 4.1.2.1](https://www.rfc-editor.org/rfc/rfc6749.html#section-4.1.2).
Default native state protection follows
[RFC 8252 §8.9](https://www.rfc-editor.org/rfc/rfc8252.html#section-8.9) and the
state-based CSRF defense in
[RFC 9700 §2.1](https://www.rfc-editor.org/rfc/rfc9700.html#section-2.1).
The explicit state-disabled exception does not provide that defense.

This is not a complete OAuth client or a claim of full OAuth conformance. PKCE,
redirect registration, and provider-specific semantics remain plugin concerns.
IPv6 fallback is not implemented; request bounds and rejection of `Origin`
headers are local policies.
