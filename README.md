# run.lnurlcash.com

Hosted, self-managing [LNURLcash](https://lnurlcash.com) mints. Each one is an
unmodified [lnurl-mint-rust](../lnurl-mint-rust) with its own Lightning node,
provisioned by a git commit to a Kubernetes configuration repository, kept
liquid by LSPS2, and synced from a shared electrs over Bitcoin Core.

It serves the landing page and its beta list, which people join by signing
in with nostr. Approved members provision a mint from the page; Run commits
its manifests to the GitOps repository, and Argo CD rolls it out.

## What's here

One binary, `lnurlcash-run`. It serves:

| | |
|---|---|
| `GET /` | The landing page (`web/`, compiled in). |
| `GET /api/beta` | `{"count": n}`: how many are on the list. |
| `POST /api/beta` | Join the list (below). |
| `POST /api/mints` `{"name"}` | Signed (NIP-98) by an approved member: provision a mint. |
| `GET /api/mints` | Signed: the signer's mints. |
| `GET /healthz` | `ok`. |

**Joining.** The page asks the browser's NIP-07 signer (Alby, nos2x, …) for a
NIP-98 HTTP-auth event (kind 27235). The event is bound to
`<PUBLIC_URL>/api/beta`, `POST` and the SHA-256 of the body, and is sent as
`Authorization: Nostr <base64 event>`. The server checks:
* the event id;
* the Schnorr signature;
* the URL, the method and the payload hash;
* a clock skew of at most 60 s.

A verified sign-up proves the key is the signer's. Without an extension,
someone can type an npub. It is listed as unverified, can't overwrite a
verified entry, and is upgraded the first time its key signs in. The body
is `{"note": "...", "npub"?: "..."}`, and the note is at most 500 characters.

Only the public key, the note and a timestamp are stored, in
`<DATA_DIR>/beta.sqlite3`. There are no cookies. Export the list with:

```sh
lnurlcash-run export        # JSON lines: npub, verified, note, created_at
```

## Running

```sh
make dev                    # http://127.0.0.1:8200, PUBLIC_URL set to match
make check test
make e2e                    # the page in headless Chromium: sign-up, approval, provisioning
make build                  # docker image lnurlcash/lnurlcash-run
make run                    # (re)starts it on 127.0.0.1:8200, state in ./data, settings from .env
make release VERSION=0.1.0  # tags v0.1.0; CI builds and pushes :0.1.0, :0.1, :0, :latest
```

| Variable | Default | |
|---|---|---|
| `LISTEN` | `127.0.0.1:8200` | |
| `PUBLIC_URL` | `https://run.lnurlcash.com` | The origin people load the page from. Sign-ups are signed for `<PUBLIC_URL>/api/beta`, so this must match exactly behind the proxy. |
| `DATA_DIR` | `data` | Holds `beta.sqlite3`. |

The page is served with a strict CSP: same-origin scripts, plus Google Fonts.
The browser's signer needs a secure context, which means https or localhost.

## Provisioning by commit

As with the lnbits.com SaaS ([lnbits-saas-api](../lnbits-saas-api)), Run
never talks to the cluster. It changes a git repository of Kubernetes
configuration, and Argo CD makes the cluster match (`src/gitops.rs`):

```text
mints/<name>/mint.yaml        what was asked for: owner, image, domain (not applied)
mints/<name>/resources.yaml   PersistentVolumeClaim, StatefulSet, Service, Ingress
argocd/applicationset.yaml    one Argo CD Application per mints/* directory
```

Each change is one commit by `lnurlcash-run`, then `fetch` + `reset` onto the
remote and a push. A push that loses a race is retried once on the new remote
state, so commits made by hand in between are kept. Operations are
serialized. Manifests are built as JSON values and written as YAML with every
value double-quoted (JSON escaping), so a name can't break out of a field.
They pass `kubeconform -strict` against Kubernetes 1.31, ApplicationSet
included.

A mint runs `lnurlcash/lnurl-mint-rust` with these settings:
* `ELECTRUM_URL` set to the shared electrs (no bitcoind credentials anywhere),
  and `LSP_NODE` set to the LSPS2 provider.
* `/data` on its PVC. The PVC is annotated so Argo CD never prunes or deletes
  it: it holds the seed and the channels.
* Non-root (uid 1000) with a read-only root filesystem and all capabilities
  dropped. `/tmp` is an emptyDir.
* A 90 s grace period, so SIGTERM can write the channel state.

This exact container shape has been run against regtest bitcoind and electrs:
it syncs, gets funded and stops cleanly.

**Joining and approval.** People sign up on the page. The operator approves
them, and only keys that signed in themselves can provision:

```sh
lnurlcash-run approve npub1…            # --revoke to take it back
lnurlcash-run mints                     # JSON lines
lnurlcash-run provision <name> --owner npub1…   # as the operator, no approval needed
lnurlcash-run disable <name>            # replicas: 0; the volume, seed and channels stay
lnurlcash-run enable <name>
MINT_IMAGE=lnurlcash/lnurl-mint-rust:0.2.0 lnurlcash-run rerender   # an upgrade: one commit per mint
```

An approved member signs in on the page, names their mint and signs once
more. The mint is live at `<name>.<MINT_DOMAIN>` once Argo CD syncs.
`MINTS_PER_MEMBER` (default 1) caps how many each member can have.

Deleting a mint is deliberately manual. Remove its directory by hand; the PVC
stays until someone deletes it, after its funds have been moved out.

**Setting up the cluster side, once:**
1. Create the GitOps repository.
2. Give Run push access, as a token in `GITOPS_REPO` or through git's
   credential config. Errors redact credentials in URLs.
3. After the first provision, `kubectl apply -f argocd/applicationset.yaml`
   from it.

| Variable | Default | |
|---|---|---|
| `GITOPS_REPO` | (provisioning off) | The repository to push to. |
| `GITOPS_ARGO_URL` | `GITOPS_REPO` without credentials | The URL Argo CD reads it at. |
| `GITOPS_BRANCH` | `main` | |
| `GITOPS_DIR` | `<DATA_DIR>/gitops` | Run's clone. |
| `MINT_IMAGE` | `lnurlcash/lnurl-mint-rust:latest` | Pin a version in production. |
| `MINT_DOMAIN` | `run.lnurlcash.com` | Mints live at `<name>.<MINT_DOMAIN>`. |
| `MINT_NAMESPACE` | `lnurlcash-mints` | |
| `MINT_NETWORK` | `bitcoin` | |
| `MINT_ELECTRUM_URL` | `tcp://electrs.bitcoin.svc.cluster.local:50001` | The shared electrs. |
| `MINT_LSP_NODE` | none | The LSPS2 provider, `pubkey@host:port`. |
| `MINT_CERT_ISSUER` | `letsencrypt` | A cert-manager ClusterIssuer, used for one certificate per mint… |
| `MINT_TLS_SECRET` | none | …or a wildcard certificate's secret for `*.<MINT_DOMAIN>`. |
| `MINT_INGRESS_CLASS` | `nginx` | |
| `MINT_STORAGE`, `MINT_STORAGE_CLASS` | `2Gi`, cluster default | |
| `MINTS_PER_MEMBER` | `1` | |

## Not built yet

* Volume snapshots on a schedule (as `snapshot-cronjob.yml` does for lnbits).
  The landing page already promises these.
* Each mint's admin UI behind its owner's nostr login. Today the owner gets the
  public mint, and the operator uses `kubectl exec … lnurl-mint-cli`.
* Running the electrs and Bitcoin Core deployment and the LSP from this
  repository.
* Exporting the data directory to the owner.
