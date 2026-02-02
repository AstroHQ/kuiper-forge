# GitHub Actions Service API Research

## Overview

GitHub has an undocumented internal API used by ARC (Actions Runner Controller) that provides efficient access to queued jobs across an entire organization. This is an alternative to the slow per-repo polling approach.

## The API Flow

```bash
# 1. Get registration token (kuiper-forge already does this)
REGISTRATION_TOKEN=$(curl -X POST \
  "https://api.github.com/orgs/$ORG/actions/runners/registration-token" \
  -H "Authorization: Bearer $GITHUB_TOKEN")

# 2. Exchange for Pipeline URL + JWT (key step)
PIPELINE_INFOS=$(curl -X POST \
  "https://api.github.com/actions/runner-registration" \
  -H "Authorization: RemoteAuth $REGISTRATION_TOKEN" \
  -d '{"url": "https://github.com/ORG", "runnerEvent": "register"}')

# Returns: { "token": "JWT_HERE", "url": "https://pipelines...actions.githubusercontent.com/..." }

# 3. Create a runner scale set (returns SCALESET_ID)
curl -X POST "$PIPELINE_URL/_apis/runtime/runnerscalesets?api-version=6.0-preview" \
  -H "Authorization: Bearer $JWT_TOKEN" \
  -d '{"name": "kuiper-forge-listener", "runnerGroupId": 1, "labels": [...]}'

# 4. Query acquirable jobs (instant - no per-repo polling!)
curl "$PIPELINE_URL/_apis/runtime/runnerscalesets/$SCALESET_ID/acquirablejobs?api-version=6.0-preview" \
  -H "Authorization: Bearer $JWT_TOKEN"

# 5. Or use long-poll for real-time job notifications
curl "$PIPELINE_URL/_apis/runtime/runnerscalesets/$SCALESET_ID/messages?sessionId=$SESSION_ID&api-version=6.0-preview" \
  -H "Authorization: Bearer $JWT_TOKEN"
```

## Key Endpoints

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/_apis/runtime/runnerscalesets` | POST | Create scale set |
| `/_apis/runtime/runnerscalesets/{id}` | GET/PATCH | Get/update scale set |
| `/_apis/runtime/runnerscalesets/{id}/acquirablejobs` | GET | List queued jobs |
| `/_apis/runtime/runnerscalesets/{id}/messages` | GET | Long-poll for jobs |
| `/_apis/runtime/runnerscalesets/{id}/sessions` | POST | Create message session |

## Benefits

- **Instant job visibility** - one API call for all repos in org
- **Long-poll capability** - real-time notifications without webhooks
- **Same mechanism ARC uses** - battle-tested by GitHub

## Challenges

- **Undocumented API** - could change without notice
- **Requires runner scale set** - need to create/manage one per org
- **JWT token management** - tokens expire, need refresh logic

## Implementation Notes

### Authentication Flow
1. Get installation access token (GitHub App)
2. Get registration token for org
3. Exchange registration token for JWT + pipeline URL via `/actions/runner-registration`
4. Use JWT as Bearer token for all pipeline API calls

### AcquirableJob Response Structure
```json
{
  "count": 2,
  "value": [
    {
      "acquireJobUrl": "...",
      "messageType": "JobRequest",
      "runnerRequestId": 12345,
      "repositoryName": "my-repo",
      "ownerName": "my-org",
      "jobWorkflowRef": "my-org/my-repo/.github/workflows/ci.yml@refs/heads/main",
      "eventName": "push",
      "requestLabels": ["self-hosted", "linux"]
    }
  ]
}
```

## Sources

- [GitHub Actions Runner Architecture (Depot.dev)](https://depot.dev/blog/github-actions-runner-architecture-part-1-the-listener)
- [ARC Discussion #3289 - Non-ARC API Access](https://github.com/actions/actions-runner-controller/discussions/3289)
- [ARC Source Code](https://github.com/actions/actions-runner-controller)
- [Runner Scale Set API Integration (DeepWiki)](https://deepwiki.com/actions/actions-runner-controller/6.3-runner-scale-set-api-integration)
