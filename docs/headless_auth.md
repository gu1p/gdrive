# Authorizing gdrive on remote machines, and unattended runs

## Default: a browser on the same machine

`gdrive account add` asks for the Client ID and Client Secret and prints an authorization URL.
After you approve in the browser, Google redirects it to `http://localhost:8085`, where gdrive
listens for at most 10 minutes. Use `--port N` to pick another port (Desktop app clients accept
any loopback port; `--port 0` picks a free one). If the port is busy, gdrive stops with an error
that names the port instead of waiting.

## Remote or headless machine: `--manual`

```
gdrive account add --manual
```

1. Enter the Client ID and Client Secret.
2. Open the printed URL in a browser on any machine, sign in and approve.
3. The browser is redirected to `http://localhost:8085/?code=...&state=...` and shows a
   connection error. That is expected.
4. Copy the full address from the address bar and paste it at the `Redirect URL:` prompt.

gdrive checks that the address belongs to this login (`state`), exchanges the code (with PKCE)
and saves the account. The code works once and expires after a few minutes; do not share the
address. `--no-browser` is an alias of `--manual`, and `--port` changes the port in the redirect
URI. An SSH tunnel (`ssh -L 8085:localhost:8085 server`, then `gdrive account add` on the server)
works as well.

## Authorizing an account again: `account reauth`

```
gdrive account reauth [NAME] [--manual] [--port N]
```

Runs the same login for an existing account (the current one when `NAME` is left out) with the
client credentials stored for it, and replaces its tokens. Sign in with the same Google account;
otherwise nothing is changed. If Google does not send a new refresh token, the stored one is kept.

## Unattended runs (cron, CI)

Only `account add` and `account reauth` ask for an authorization. Every other command uses the
stored token and refreshes it when it expires; it never opens a port or waits for a browser. When
the token cannot be refreshed (no refresh token is stored, or Google answers `invalid_grant`
because the token expired or was revoked), the command fails at once with exit status 1:

```
Error: account 'me@example.com' needs to be authorized again: Google rejected the stored refresh token (invalid_grant: Token has been expired or revoked.); run 'gdrive account add' (or 'gdrive account reauth me@example.com') to authorize again
```

A refresh that gets no answer within 60 seconds fails as well. Run `gdrive account reauth`
(with `--manual` on a headless machine) and the next run works again. If your OAuth app is in
the *Testing* publishing status, Google expires its refresh tokens after 7 days; set it to *In
production* to avoid the weekly re-authorization.
