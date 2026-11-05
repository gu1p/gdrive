# Create Google API credentials for gdrive

gdrive has no built-in client ID. You create your own OAuth client in a Google Cloud project, and gdrive
uses it to sign in to your Google account. It is free and takes about ten minutes.

You need:

- A Google account to own the Cloud project. It can be a different account from the one whose Drive you
  want to use.
- A web browser.

Google moved the consent screen settings in 2025. They used to be under **APIs & Services > OAuth consent
screen**. They now live in their own section, **Google Auth Platform**, with the pages **Branding**,
**Audience**, **Data Access** and **Clients**. Older guides and screenshots show the old layout.

This guide was checked against Google's documentation on 2026-11-04. The sources are listed at the
[end of the page](#sources).

## Overview

1. [Create a project](#1-create-a-project)
2. [Enable the Google Drive API](#2-enable-the-google-drive-api)
3. [Set up Google Auth Platform](#3-set-up-google-auth-platform)
4. [Add test users](#4-add-test-users)
5. [Add the Drive scopes](#5-add-the-drive-scopes)
6. [Create a Desktop app client](#6-create-a-desktop-app-client)
7. [Choose Testing or In production](#7-choose-testing-or-in-production)
8. [Add the account to gdrive](#8-add-the-account-to-gdrive)

Then see [Verification](#verification-you-do-not-need-it) and [Troubleshooting](#troubleshooting).

## 1. Create a project

1. Open [Create a project](https://console.cloud.google.com/projectcreate) in the Google Cloud console
   (**Menu > IAM & Admin > Create a Project**).
2. Enter a **Project name**, for example `my-files-cli`. The project ID is derived from it. You cannot
   change the project ID later.
3. **Location**: for a personal Google account, leave it at **No organization**.
4. Click **Create**.
5. Check that the new project is selected in the project picker at the top of the console. All the next
   steps apply to the selected project.

## 2. Enable the Google Drive API

1. Open the [Google Drive API](https://console.cloud.google.com/apis/library/drive.googleapis.com) page
   (**Menu > APIs & Services > Library**, then search for `Google Drive API`).
2. Click **Enable**.

## 3. Set up Google Auth Platform

1. Open [Google Auth Platform > Branding](https://console.cloud.google.com/auth/branding)
   (**Menu > Google Auth Platform > Branding**).
2. If the page says Google Auth Platform is not configured yet, click **Get started**. A short wizard opens.
3. **App Information**
   - **App name**: pick a neutral, unique name, for example `jane-files-cli` or your project ID.
     Do not use Google product names. Google rejects names such as `gdrive`, `Google Drive` or
     `Drive CLI` with "app name does not comply with requirements" (see [issue #160](https://github.com/glotlabs/gdrive/issues/160)).
   - **User support email**: your own address.
   - Click **Next**.
4. **Audience**
   - Choose **External** if you use a personal Gmail account.
   - Choose **Internal** only if your account is in a Google Workspace organization and the project belongs
     to that organization. Internal apps need no test users and no verification, and only members of the
     organization can sign in.
   - Click **Next**.
5. **Contact Information**: enter an email address. Google sends notices about the project there, for
   example before it deletes an unused client. Click **Next**.
6. **Finish**: tick **I agree to the Google API Services: User Data Policy**. Click **Continue**, then
   **Create**.

## 4. Add test users

Skip this step if you chose Internal.

A new External app starts in the **Testing** publishing status. In Testing, only the accounts on the test
user list can sign in.

1. Open [Google Auth Platform > Audience](https://console.cloud.google.com/auth/audience).
2. Under **Test users**, click **Add users**.
3. Enter the Google account whose Drive gdrive will use. Add more accounts if you need them (up to 100).
4. Click **Save**.

## 5. Add the Drive scopes

gdrive asks for these two scopes when you sign in:

- `https://www.googleapis.com/auth/drive` (see, edit, create and delete all your Drive files)
- `https://www.googleapis.com/auth/drive.metadata.readonly` (see metadata of your Drive files)

Declare them on your app so the configuration matches what gdrive requests:

1. Open [Google Auth Platform > Data Access](https://console.cloud.google.com/auth/scopes).
2. Click **Add or remove scopes**.
3. Filter for `Google Drive API` and select the two scopes above. If they are not listed, paste them into
   **Manually add scopes** and click **Add to table**.
4. Confirm the selection in the panel, then click **Save**.

Both scopes are *restricted* scopes. After this step the console may say **Needs verification** and offer
**Prepare for verification**. For personal use you do not need to act on it. See
[Verification](#verification-you-do-not-need-it).

## 6. Create a Desktop app client

1. Open [Google Auth Platform > Clients](https://console.cloud.google.com/auth/clients).
2. Click **Create client**.
3. **Application type**: choose **Desktop app**. Do not choose Web application.
4. **Name**: anything. It is only shown in the console.
5. Leave **Use this client for an AI-powered agent** unticked.
6. Click **Create**.
7. The console shows the **Client ID** and **Client secret**. Copy both now, or click **Download JSON**.
   Since June 2025 the console masks the secret after you close this dialog, and you cannot view it again.
   If you lose it, create a new client.

Treat the client secret like a password. Do not commit it to a repository.

### Why Desktop app

`gdrive account add` starts a small web server on `127.0.0.1` port 8085. After you approve access, Google
sends your browser to `http://localhost:8085/?code=...`, and gdrive reads the code from that request.
This is the *loopback* redirect.

- A **Desktop app** client accepts loopback redirects on any port. You do not register anything. A Desktop
  client has no redirect URI field at all.
- A **Web application** client accepts only redirect URIs that you registered, character for character.
  With a Web client you get `Error 400: redirect_uri_mismatch`
  ([issue #109](https://github.com/glotlabs/gdrive/issues/109)). Registering the address on a Web client
  is not a real fix. gdrive 3.9.1 redirects to `http://127.0.0.1:8085`, gdrive 4.0.0 to
  `http://localhost:8085`, and `--port` changes the port. Each change breaks a Web client again. Create a
  Desktop app client instead.

The old "out-of-band" flow, where Google showed a code for you to copy into the terminal, is gone. Google
blocked it for all clients on 31 January 2023. Guides that use it, and gdrive 2.x, no longer work. To sign
in on a machine without a browser, see [Remote and headless setup](remote_and_headless.md).

## 7. Choose Testing or In production

An External app is either in **Testing** or **In production**. You can switch on the **Audience** page.

|  | Testing | In production, unverified |
|---|---|---|
| Who can sign in | Only the test users (up to 100) | Any Google account, up to 100 users in total over the project's lifetime |
| Refresh token | Expires seven days after you sign in | Does not expire on a schedule |
| Warning screen at sign-in | Yes | Yes, "Google hasn't verified this app" |
| What you must set up | Nothing more | A home page and a privacy policy link on the Branding page |

**Testing** is enough to try gdrive. But every seven days gdrive fails with `Token has been expired or
revoked`, and you have to sign in again. That does not work for scheduled jobs.

**In production** keeps you signed in. To publish:

1. Open [Branding](https://console.cloud.google.com/auth/branding).
2. Fill in **App home page** and **Application privacy policy link**, and add their domain under
   **Authorized domains**. Since August 2026 the **Publish app** button stays greyed out until these are
   set, also for personal apps (reported by rclone users, see [Sources](#sources)). For personal use, a
   page you control is enough. For example, a GitHub Pages site at `https://<your-user>.github.io/` with
   one paragraph that says the app is your personal Drive client and that your data stays on your own
   machines. The authorized domain is then `<your-user>.github.io`.
3. Click **Save**.
4. Open [Audience](https://console.cloud.google.com/auth/audience) and click **Publish app**, then
   **Confirm**. The status changes to **In production**.

If you signed in to gdrive while the app was in Testing, sign in once more after publishing. A token from
the Testing period still expires after seven days.

- gdrive 4.0.0: `gdrive account reauth`
- gdrive 3.9.x: `gdrive account remove <ACCOUNT_NAME>`, then `gdrive account add`

## 8. Add the account to gdrive

```sh
gdrive account add
```

1. Paste the **Client ID** and **Client secret** when gdrive asks for them.
2. gdrive prints a URL. Open it in your browser.
3. Choose the Google account. In Testing, it must be on the test user list.
4. Google shows **Google hasn't verified this app**. This is expected: it is your own app. Click
   **Advanced**, then **Go to `<your app name>` (unsafe)**.
5. Google lists what the app may access. If it shows checkboxes, select all of them. gdrive needs full
   Drive access. Click **Continue**.
6. The browser opens `http://localhost:8085/...`. gdrive receives the code, prints
   `Saved account credentials in ...`, and is ready to use.

If the browser runs on a different machine from gdrive, step 6 fails with "127.0.0.1 refused to connect"
or "This site can't be reached". See [Remote and headless setup](remote_and_headless.md).

Then try:

```sh
gdrive account current
gdrive files list
```

More commands are in [Basic usage](basic_usage.md).

## Verification: you do not need it

Google verifies apps that are offered to the public. gdrive is different: each user creates a private app
for their own account.

- Google's rule: an app for personal use with fewer than 100 users does not need verification. Its users
  click through the unverified-app warning.
- An unverified app in production is limited to 100 users in total. That is plenty for one person, a
  family or a small team.
- The Drive scopes gdrive uses are restricted. A public app that requests restricted scopes needs
  restricted-scope verification. If it stores or sends that data through its own servers, it also needs a
  yearly security assessment. This is why the fork ships no shared client ID, and why you create your own.
- Ignore **Needs verification** and **Prepare for verification** in the console. They do not block
  personal use.

## Troubleshooting

| Error text | Cause | Fix |
|---|---|---|
| `Error 400: redirect_uri_mismatch` | The client is a Web application client (or another type that is not Desktop app). | Create a **Desktop app** client ([step 6](#6-create-a-desktop-app-client)). Run `gdrive account add` again with the new ID and secret. Do not register redirect URIs. |
| `Access blocked: <app> has not completed the Google verification process` and `Error 403: access_denied` | The app is in Testing and you signed in with an account that is not a test user. | Add that account under **Audience > Test users** ([step 4](#4-add-test-users)), or publish the app ([step 7](#7-choose-testing-or-in-production)). |
| `This app is blocked` and "This app tried to access sensitive info in your Google Account. To keep your account safe, Google blocked this access." | Google blocks an unverified app that requests restricted scopes. Typical cases: the built-in client of gdrive 2.x; a Google Workspace admin who does not allow unverified apps; your project has used up its 100 users. | Use gdrive 3 or later with your own Desktop app client. On a Workspace account, ask the admin to trust your client ID, or create the project in the organization and choose **Internal**. |
| `Access blocked: <app>'s request is invalid`, "You can't sign in to this app because it doesn't comply with Google's OAuth 2.0 policy", `Error 400: invalid_request` | The tool uses the removed out-of-band (copy the code) flow. gdrive 2.x and some old tools do this. | Use gdrive 3 or later with a Desktop app client. On a headless machine, follow [Remote and headless setup](remote_and_headless.md). |
| "app name does not comply with requirements" (on the Branding page) | The app name contains or imitates a Google product name, such as `gdrive` or `Google Drive`. | Choose a neutral, unique name ([step 3](#3-set-up-google-auth-platform)). |
| `Token has been expired or revoked` or `invalid_grant`, about seven days after `gdrive account add` | The app is in Testing. Refresh tokens of Testing apps expire seven days after consent. | Publish the app, then sign in again ([step 7](#7-choose-testing-or-in-production)). |
| `Token has been expired or revoked` on a published app | You removed the app's access in your Google account, the token was unused for six months, or the account has more than 100 tokens for this client and Google dropped the oldest. | Sign in again: `gdrive account reauth` (4.0.0) or remove and add the account (3.9.x). |
| **Publish app** is greyed out | Branding lacks a home page, a privacy policy link or an authorized domain. | Fill them in ([step 7](#7-choose-testing-or-in-production)), or stay in Testing and sign in again every week. |
| `deleted_client` or `invalid_client` | Wrong client ID or secret, or Google deleted the client. Google deletes OAuth clients that were not used for six months; you can restore them for 30 days. | Check the values in Clients. Restore the client or create a new one, then add the account again. |
| `org_internal` | The app is Internal and the account is outside the organization. | Sign in with an account of the organization, or set the audience to External. |
| `admin_policy_enforced` | A Google Workspace admin blocks the Drive scopes for your account. | Ask the admin to allow the app. |
| "127.0.0.1 refused to connect", `ERR_CONNECTION_REFUSED`, "This site can't be reached" after you approve | The browser is not on the machine where gdrive runs, or gdrive is no longer waiting. | See [Remote and headless setup](remote_and_headless.md). |

## Sources

Google documentation used for this guide, checked on 2026-11-04:

- [Create a Google Cloud project](https://developers.google.com/workspace/guides/create-project)
- [Enable Google Workspace APIs](https://developers.google.com/workspace/guides/enable-apis)
- [Configure the OAuth consent screen and choose scopes](https://developers.google.com/workspace/guides/configure-oauth-consent)
- [Create access credentials](https://developers.google.com/workspace/guides/create-credentials) (Desktop app client)
- [Manage OAuth App Branding](https://support.google.com/cloud/answer/15549049)
- [App Identity & Branding](https://support.google.com/cloud/answer/13804963) (no Google product names in the app name)
- [Manage App Audience](https://support.google.com/cloud/answer/15549945) (Testing, In production, test users, the 100-user cap, seven-day expiry)
- [Unverified apps](https://support.google.com/cloud/answer/7454865)
- [When is verification not needed](https://support.google.com/cloud/answer/13464323) (personal use, fewer than 100 users)
- [Choose Google Drive API scopes](https://developers.google.com/workspace/drive/api/guides/api-specific-auth) (restricted scopes, security assessment)
- [Using OAuth 2.0 to Access Google APIs](https://developers.google.com/identity/protocols/oauth2) (refresh token expiration)
- [OAuth 2.0 for Mobile & Desktop Apps](https://developers.google.com/identity/protocols/oauth2/native-app) (Desktop app, loopback redirect, error codes, OOB removal)
- [Out-Of-Band (OOB) flow migration guide](https://developers.google.com/identity/protocols/oauth2/resources/oob-migration)
- [OAuth 2.0 for Web Server Applications](https://developers.google.com/identity/protocols/oauth2/web-server) (registered redirect URIs must match exactly)
- [Usability and safety updates to Google Auth Platform](https://developers.googleblog.com/en/usability-and-safety-updates-to-google-auth-platform/) (secret masking, deletion of unused clients, the move to Google Auth Platform)

Other sources:

- [rclone forum: Can't configure Google Drive client ID](https://forum.rclone.org/t/cant-configure-google-drive-client-id/54191) (August to October 2026: **Publish app** needs a home page and a privacy policy URL)
- [rclone: Making your own client_id](https://rclone.org/drive/#making-your-own-client-id)
- The error texts in the troubleshooting table come from upstream issues
  [#109](https://github.com/glotlabs/gdrive/issues/109), [#115](https://github.com/glotlabs/gdrive/issues/115),
  [#121](https://github.com/glotlabs/gdrive/issues/121), [#129](https://github.com/glotlabs/gdrive/issues/129),
  [#160](https://github.com/glotlabs/gdrive/issues/160) and [#22](https://github.com/glotlabs/gdrive/issues/22).
  The exact wording of the warning screen ("Google hasn't verified this app", **Advanced**, **Go to ...
  (unsafe)**) comes from user reports; Google's help pages describe the screen but do not quote it.
