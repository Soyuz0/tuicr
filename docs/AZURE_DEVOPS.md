# Azure DevOps

tuicr reviews Azure Repos pull requests through Azure CLI and the
`azure-devops` extension. It does not store PATs, acquire tokens itself, or
make authenticated HTTP requests outside Azure CLI.

## Prerequisites

Install [Azure CLI](https://learn.microsoft.com/en-us/cli/azure/install-azure-cli)
and its Azure DevOps extension:

```bash
az extension add --name azure-devops
```

Authenticate with Microsoft Entra ID:

```bash
az login
```

Azure DevOps PAT login through the extension is also supported for normal PR
operations:

```bash
az devops login --organization https://dev.azure.com/ORG
```

tuicr never prompts for, reads, or stores that PAT. Configure defaults if you
also use Azure CLI interactively without explicit organization/project flags:

```bash
az devops configure --defaults \
  organization=https://dev.azure.com/ORG \
  project=PROJECT
```

## Open A Pull Request

From a matching Azure Repos checkout:

```bash
tuicr ado 125
tuicr tui ado 125
tuicr ado "https://dev.azure.com/ORG/PROJECT/_git/REPOSITORY/pullrequest/125"
```

`ado` is explicit and does not change the existing `pr` or `mr` meanings.
The Pull Requests selector auto-detects Azure remotes and lists active PRs.
Press `r` to switch between all active PRs and PRs where the current identity
is a reviewer.

Active draft PRs are listed with a `[draft]` marker and are fully reviewable:
tuicr can open their diffs, save comments, and submit Comment, Approve, or
Request changes feedback. This is distinct from tuicr's `:submit draft`
action, which means creating a private pending review on the forge.

Supported remote forms:

```text
https://dev.azure.com/ORG/PROJECT/_git/REPOSITORY
https://ORG@dev.azure.com/ORG/PROJECT/_git/REPOSITORY
git@ssh.dev.azure.com:v3/ORG/PROJECT/REPOSITORY
ssh://git@ssh.dev.azure.com/v3/ORG/PROJECT/REPOSITORY
https://ORG.visualstudio.com/PROJECT/_git/REPOSITORY
```

Organization, project, and repository path components are percent-decoded for
Azure CLI and percent-encoded independently in persisted `ado:` session slugs.

## Diff Loading

Azure's documented iteration-change API lists changed items but does not return
a cumulative unified patch. tuicr therefore resolves Azure's exact target and
source commit SHAs and runs parser-safe Git object operations in the matching
checkout. Missing commit objects are imported with:

```bash
git fetch --no-tags --no-write-fetch-head AZURE_REPOSITORY_URL COMMIT_SHA
```

tuicr never checks out a PR, switches branches, writes `FETCH_HEAD`, resets,
cleans, or stashes. The fetch can add missing objects to `.git/objects`; it does
not update refs or tracked files. A full PR URL is parsed anywhere, but loading
its diff currently requires the matching checkout because Azure CLI
authentication does not automatically provide Git transport credentials and
the documented REST resources cannot reconstruct every rename, binary, mode,
or truncated-content case safely.

## Existing Threads

tuicr loads Azure pull-request threads through `az devops invoke`. It preserves
PR-, file-, line-, and range-level locations, left/right diff sides, replies,
deleted-comment placeholders, and active/resolved/outdated state. Clearly
identified `commentType=system` messages are omitted; user replies are retained.

## Submit Actions

| tuicr action | Azure DevOps behavior |
|---|---|
| Comment | Create threads without changing the vote |
| Approve | Create threads, then `az repos pr set-vote --vote approve` |
| Request changes | Create threads, then `az repos pr set-vote --vote wait-for-author` |
| Draft | Unavailable and hidden from the Azure picker; this is the private pending-review action, not reviewing an Azure draft PR |

Azure thread creation and voting are separate operations, so submission is not
atomic. tuicr posts the summary, then each independent inline/file thread, and
applies the vote last. If a later call fails, successfully posted comments are
locked locally and identified in the error; failed/unattempted comments remain
local drafts. Retrying submits only those remaining drafts, preventing known
successful comments from being duplicated. Azure does not document an
idempotency key, so an interrupted command whose server result never reaches
tuicr remains inherently ambiguous.

File-only thread contexts are supported by Azure's REST thread-context model,
but Microsoft does not publish a dedicated file-only create example. This path
still requires validation against a disposable organization before it should be
treated as exhaustively verified.

## Commands And REST Resources

First-class commands:

```bash
az repos pr show --id PR_ID --organization ORGANIZATION_URL
az repos pr list --organization ORGANIZATION_URL --project PROJECT \
  --repository REPOSITORY --status active --skip OFFSET --top COUNT
az repos pr set-vote --id PR_ID --vote approve --organization ORGANIZATION_URL
az repos pr set-vote --id PR_ID --vote wait-for-author --organization ORGANIZATION_URL
```

All commands include `--output json --only-show-errors`. Generic calls use
`az devops invoke --api-version 7.1` for the documented Git resources:

- pull-request commits
- pull-request iterations
- pull-request iteration changes
- pull-request threads
- pull-request reviewers
- repository items

tuicr discovers the installed extension's exact Git `resourceName` at runtime
from its route metadata rather than hard-coding guessed resource identifiers.
JSON request bodies use a secure temporary file via `--in-file` and are removed
automatically.

## Permissions And Troubleshooting

Reading PRs, commits, items, and threads requires repository read access.
Creating threads and voting requires pull-request contribution permissions and
the applicable branch policies may impose additional reviewer requirements.

| Error | Resolution |
|---|---|
| Azure CLI missing | Install `az` or set `TUICR_AZ_BIN` to its path. |
| Extension missing | Run `az extension add --name azure-devops`. |
| Authentication failed | Run `az login` or `az devops login --organization URL`. |
| Organization/project unresolved | Verify the remote and `az devops configure --list`. |
| Permission denied | Verify repository/PR permissions for the authenticated identity. |
| PR/repository not found | Verify organization, project, repository, and PR ID. |
| Review-request filter cannot resolve identity | Use all-open scope, or run `az login`; PAT-only extension login still works for normal operations. |
| Missing diff commits | Verify Git credentials for the Azure remote and rerun from its checkout. |

## Testing

Normal tests use neutral JSON fixtures, injected runners, and a fake executable;
they never require Azure CLI or network access. Override the executable in a
subprocess test with:

```bash
TUICR_AZ_BIN=/path/to/fake-az cargo test forge::azure_devops
```

Azure DevOps has no supported local emulator. Validate changes against a
disposable organization and non-production PR using an already authenticated
CLI. Never complete, abandon, or merge that PR from a test. This implementation
has not yet completed a real Azure end-to-end run; fixture success is not a
claim of live integration success.
