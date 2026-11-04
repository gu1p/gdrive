# Remote and headless setup

This page explains how to use gdrive on a machine without a browser: a server you reach over SSH, a
container, a NAS, a cron job.

You need a Desktop app client first. See [Create Google API credentials](create_google_api_credentials.md).

## Why the normal login fails on a server

`gdrive account add` starts a small web server on the machine where gdrive runs. It listens on
`127.0.0.1`, port 8085. You open the login URL in a browser and approve access. Google then sends the
browser to `http://localhost:8085/?code=...`. gdrive reads the code from that request and saves the
account.

If the browser runs on your laptop and gdrive runs on a server, `localhost` is your laptop. Nothing
listens there, so the browser shows "127.0.0.1 refused to connect" or "This site can't be reached", and
gdrive on the server keeps waiting.

## Pick a method

| Method | What you need | gdrive version |
|---|---|---|
| [A. Export and import](#a-export-an-account-and-import-it-on-the-server) | gdrive on a machine with a browser | 3.x and 4.0.0 |
| [B. SSH tunnel](#b-ssh-tunnel-for-the-loopback-redirect) | SSH from the machine with the browser to the server | 3.x and 4.0.0 |
| [C. Manual login](#c-manual-login-account-add---manual) | Any browser, on any device | 4.0.0 and later |
| [Curl the redirect](#curl-the-redirect-gdrive-3x) | A second shell on the server | 3.x |

For scheduled jobs, read [D. Unattended use](#d-unattended-use-cron-systemd-ci) as well.

## A. Export an account and import it on the server

Sign in on a machine that has a browser, then move the account to the server.

On your local machine:

```sh
gdrive account add
gdrive account list
gdrive account export <ACCOUNT_NAME>
```

The account name is the email address of the Google account. The export writes an archive to the current
directory, for example `gdrive_export-jane_doe_gmail_com.tar`. Characters other than letters and digits
become `_`. The archive is a tar file, although the help text says zip.

Copy it to the server and import it:

```sh
scp gdrive_export-jane_doe_gmail_com.tar user@server:
ssh user@server
gdrive account import gdrive_export-jane_doe_gmail_com.tar
```

If the server has no current account yet, the import switches to the new account. Otherwise switch
yourself:

```sh
gdrive account switch <ACCOUNT_NAME>
```

Then delete the archive on both machines. It contains your client secret and your refresh token. Anyone
who has it can read and change your whole Drive.

Both machines now share one refresh token. If you remove the app's access in your Google account, both
stop working.

## B. SSH tunnel for the loopback redirect

Forward port 8085 from your local machine to the server. The browser's request to `localhost:8085` then
reaches gdrive on the server. This method comes from upstream PR
[#132](https://github.com/glotlabs/gdrive/pull/132) by TombLL.

1. On your local machine, open the tunnel and keep it open:

   ```sh
   ssh -N -L 8085:127.0.0.1:8085 user@server
   ```

   `-N` opens no remote shell; the command only forwards the port. Use `127.0.0.1` as the target, not
   `localhost`. gdrive listens on IPv4 only, and on many servers `localhost` resolves to the IPv6 address
   `::1` first.

2. In a second terminal, log in to the server and add the account:

   ```sh
   ssh user@server
   gdrive account add
   ```

   Enter the client ID and secret. gdrive prints a URL.

3. Open the URL in the browser on your local machine. Approve access. The browser goes to
   `http://localhost:8085/...`. The tunnel carries the request to gdrive on the server, and gdrive prints
   `Saved account credentials in ...`.

4. Close the tunnel with `Ctrl+C`.

You can also do it in one session: run `ssh -L 8085:127.0.0.1:8085 user@server`, without `-N`, and run
`gdrive account add` in that shell.

If port 8085 is already in use on your local machine, `ssh` warns that it cannot listen on the port. In
gdrive 4.0.0, pick another port and use the same number on both sides, because the port is part of the
redirect address:

```sh
ssh -N -L 9085:127.0.0.1:9085 user@server   # local machine
gdrive account add --port 9085              # server
```

## C. Manual login: `account add --manual`

Available from gdrive 4.0.0. No tunnel and no second machine with gdrive. The browser can be on any
device, including a phone.

```sh
gdrive account add --manual
```

1. Enter the client ID and secret. gdrive prints the login URL.
2. Open the URL in any browser and approve access.
3. Google sends the browser to an address that starts with `http://localhost:8085/?code=`. The page does
   not load. That is expected.
4. Copy the full address from the browser's address bar.
5. Paste it into gdrive and press Enter. gdrive takes the code from the address, exchanges it for tokens
   and saves the account.

Notes:

- Paste the address within a few minutes. The code in it is short-lived and works only once.
- The address contains a one-time code for your account. Do not paste it anywhere else.
- `--manual` works with `--port N`. The port only changes the address you copy.

### `--port N`

Available from gdrive 4.0.0. gdrive listens on port `N` instead of 8085, and the redirect address uses
the same port. Use it when port 8085 is taken, for example by another program, or when you forward a
different port through SSH (see [method B](#b-ssh-tunnel-for-the-loopback-redirect)).

```sh
gdrive account add --port 9085
```

Desktop app clients accept any loopback port, so you do not change anything in the Google Cloud console.

### Curl the redirect (gdrive 3.x)

gdrive 3.x has no `--manual`. You can get the same result with a second shell on the server. This trick
comes from upstream issues [#161](https://github.com/glotlabs/gdrive/issues/161) and
[#30](https://github.com/glotlabs/gdrive/issues/30).

1. On the server, run `gdrive account add` and leave it waiting.
2. Open the URL in a local browser and approve access. The browser fails to load the address at the end.
3. Copy that address from the address bar.
4. In a second shell on the server, request it. Quote it, because it contains `&`:

   ```sh
   curl 'http://127.0.0.1:8085/?code=...&scope=...'
   ```

   Replace the host part with `127.0.0.1` if the copied address says `localhost`.

5. The first shell prints `Saved account credentials in ...`.

## D. Unattended use: cron, systemd, CI

### What changes in 4.0.0

In gdrive 3.9.1, any command could start the browser login when the refresh token stopped working. A
cron job then printed a login URL and waited on port 8085 forever. The next run failed because the port
was still taken (upstream [#147](https://github.com/glotlabs/gdrive/issues/147)).

From gdrive 4.0.0, only `gdrive account add` and `gdrive account reauth` start a login. Every other
command fails fast instead: it prints an error that starts with `Error:`, tells you to run
`gdrive account reauth`, and exits with a non-zero status. Nothing waits on port 8085.

### Sign in again with `account reauth`

Available from gdrive 4.0.0. When a scheduled job reports that the account needs a new login, run this
once, interactively, as the user that runs the job:

```sh
gdrive account reauth
```

It runs the Google sign-in again for an existing account and keeps the account's settings. If Google does
not send a new refresh token, gdrive keeps the stored one instead of overwriting it with nothing (upstream
[#150](https://github.com/glotlabs/gdrive/issues/150)). On a server without a browser, add `--manual`. It
takes `--port` too. Run `gdrive account reauth --help` for the full list of options.

### Keep the token valid

A refresh token stops working when:

- the app is in **Testing**: tokens expire seven days after you sign in. Publish the app, then sign in
  once more (see [step 7 of the credentials guide](create_google_api_credentials.md#7-choose-testing-or-in-production));
- the token has not been used for six months;
- you remove the app's access at <https://myaccount.google.com/permissions>;
- the Google account has more than 100 refresh tokens for the same client. Each `gdrive account add` on
  another machine creates a new token. Exporting and importing an account reuses the existing one.

Google also deletes OAuth clients that were not used for six months. You get an email first and can
restore a deleted client for 30 days.

### Run jobs as the right user

gdrive reads its accounts from `~/.config/gdrive3/` of the user that runs it. Run the job as the user
that added the account, with `HOME` set. A root cron job does not see the accounts of another user.

The current account (`gdrive account current`) applies to every command. To use several accounts in one
job, run `gdrive account switch <ACCOUNT_NAME>` before each part. There is no per-command account option
yet.

### Check the exit status

Every gdrive command exits with a non-zero status when it fails. Check it in scripts:

```sh
#!/bin/sh
set -eu
gdrive files upload --parent <FOLDER_ID> /var/backups/db.tar.gz
```

A cron entry that mails you when the job fails:

```sh
0 3 * * * /usr/local/bin/backup-to-drive.sh > /var/log/backup-to-drive.log 2>&1 || mail -s "Drive backup failed" you@example.com < /var/log/backup-to-drive.log
```

### Service accounts

Service accounts are not supported yet. They are planned (see the [roadmap](triage/README.md), milestone 3).
