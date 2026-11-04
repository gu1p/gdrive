# Service accounts and Application Default Credentials

Available from gdrive 4.0.0.

A service account is a Google identity for a program, not for a person. gdrive can use one instead of
your Google account: there is no browser login, no refresh token that expires after seven days in
Testing mode, and nothing to authorize again. That suits CI jobs, servers and containers.

## When to use one

A service account sees only what is shared with it: files and folders shared with its email address,
and shared drives it is a member of. It does not see your My Drive.

A service account has no Drive storage of its own and cannot own files. An upload into My Drive, or into
a folder that someone shared with it, fails with `storageQuotaExceeded` ("Service Accounts do not have
storage quota"). It can write files in two ways:

- **Shared drive** (Google Workspace): add the service account to a shared drive. Files in a shared
  drive belong to the drive, not to the service account. See [Shared drives](#shared-drives).
- **Domain-wide delegation** (Google Workspace): a Workspace administrator allows the service account
  to act for users of the domain. gdrive then works in that user's Drive, and the user owns the files.
  See [Domain-wide delegation](#domain-wide-delegation).

With a personal Gmail account neither is available. Use the normal login, `gdrive account add`, and
[Remote and headless setup](remote_and_headless.md) for servers.

## Create a key

1. Open the [Google Cloud console](https://console.cloud.google.com/) and select or create a project.
2. Enable the Google Drive API: **APIs & Services > Library**, search for "Google Drive API",
   **Enable**.
3. Create the service account: **IAM & Admin > Service accounts > Create service account**. Give it a
   name. It needs no IAM role for Drive; skip **Grant this service account access to project**.
4. Open the new service account, go to **Keys > Add key > Create new key**, choose **JSON** and
   **Create**. The browser downloads the key file.

Organizations can forbid keys (the policy `iam.disableServiceAccountKeyCreation`, on by default in
organizations created since May 2024). Then ask an administrator, or run gdrive on Google Cloud with
[Application Default Credentials](#application-default-credentials).

The key file grants everything the service account can access. Keep it secret and do not commit it.

## Add the account

```sh
gdrive account add --service-account ~/Downloads/my-project-1a2b3c4d5e6f.json
```

gdrive checks the file, asks Google for a token with it, and stores the key in its config directory
(see [Files](#files)). The account is named after the service account's email address, for example
`backup@my-project.iam.gserviceaccount.com`; choose another name with `--name`. gdrive then switches to
the new account. Nothing is stored when the key does not work.

To replace the key (key rotation), run the command again with the new key and the same name, then delete
the old key in the console.

## Shared drives

1. In Google Drive, open the shared drive and choose **Manage members**.
2. Add the service account's email address (the `client_email` of the key file) as **Content manager**.
3. Find the drive's ID and upload into it:

   ```sh
   gdrive drives list
   gdrive files upload --parent <DRIVE_OR_FOLDER_ID> backup.tar.gz
   ```

## Domain-wide delegation

A Google Workspace super administrator sets this up once:

1. In the [Admin console](https://admin.google.com/), go to **Security > Access and data control > API
   controls > Manage Domain Wide Delegation**, and choose **Add new**.
2. **Client ID**: the `client_id` of the key file, a long number.
3. **OAuth scopes**: `https://www.googleapis.com/auth/drive`. gdrive asks for this one scope only.

Then add the account with the user to act for:

```sh
gdrive account add --service-account key.json --subject alice@example.com --name alice-robot
```

The subject is stored with the account. If delegation is not set up, the command fails with
`unauthorized_client` and says what to allow in the Admin console. A change in the Admin console can take
a while to apply (Google says up to 24 hours).

## Application Default Credentials

Application Default Credentials (ADC) are the credentials that Google's own tools find in the
environment. gdrive looks in the same places, in this order:

1. the file that `GOOGLE_APPLICATION_CREDENTIALS` names: a service account key, or user credentials;
2. the user credentials of `gcloud auth application-default login`, in
   `~/.config/gcloud/application_default_credentials.json` (on Windows `%APPDATA%\gcloud\`, or
   `$CLOUDSDK_CONFIG` if set);
3. the metadata server on Google Cloud (Compute Engine, Cloud Run, GKE), which gives a token for the
   service account the machine or service runs as.

```sh
gdrive account add --adc
```

The account is named `adc` unless you pass `--name`. gdrive stores no credentials for it: every command
looks them up again, so a changed `GOOGLE_APPLICATION_CREDENTIALS` takes effect at once. When nothing is
found, the error lists the places it checked.

Notes:

- On Compute Engine the VM's access scopes must include Drive. Set them when you create the VM, or stop
  it and run `gcloud compute instances set-service-account VM --scopes=https://www.googleapis.com/auth/drive,cloud-platform`.
  Otherwise Drive answers "Request had insufficient authentication scopes".
- gcloud user credentials need the Drive scope:
  `gcloud auth application-default login --scopes=https://www.googleapis.com/auth/drive,https://www.googleapis.com/auth/cloud-platform`.
  Google may refuse gcloud's own client for Drive. Then pass your own Desktop client with
  `--client-id-file=client_secret.json` (see [Create Google API credentials](create_google_api_credentials.md)).
- Workload identity federation files (`"type": "external_account"`) and impersonation files
  (`"type": "impersonated_service_account"`) are not supported.
- The rules of [When to use one](#when-to-use-one) apply when the credentials are a service account.

## Other account commands

`gdrive account list`, `current`, `switch`, `remove`, `export` and `import` work for these accounts as
for others. `gdrive account reauth` is not needed and says so.

An exported archive of a service account contains its key: keep it secret and delete it after the
import. gdrive 3.x can unpack such an archive but cannot use the account.

## Files

Each account is a directory under `~/.config/gdrive3/` (see [Credentials and security](../README.md#credentials-and-security)):

| File | Content |
|---|---|
| `<account>/account.json` | The account type: `{"type": "service_account", "subject": "..."}` or `{"type": "application_default_credentials"}` |
| `<account>/service_account.json` | The key file as you gave it (service accounts only), mode 0600 |
| `<account>/tokens.json` | Cached access tokens (service accounts only), each marked with a hash of the key and subject it was issued for |

Accounts without `<account>/account.json` are accounts that log in with an OAuth client.

## Troubleshooting

| Error | Cause and fix |
|---|---|
| `storageQuotaExceeded`, "Service Accounts do not have storage quota" | The service account would own the file. Upload into a [shared drive](#shared-drives), or use [domain-wide delegation](#domain-wide-delegation). |
| `invalid_grant: Invalid JWT Signature` | The key was deleted or disabled. Create a new key and add the account again. |
| `invalid_grant: Invalid JWT: Token must be a short-lived token` | The computer's clock is wrong. Sync it (NTP). |
| `unauthorized_client` | Domain-wide delegation is not allowed for this client ID and scope. |
| `accessNotConfigured`, "Google Drive API has not been used in project" | Enable the Drive API in the service account's project. |
| "File not found" for a file you can see | The file is not shared with the service account. Share it with the `client_email`. |
