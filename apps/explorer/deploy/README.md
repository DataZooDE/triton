# Deploy — `dz-triton-explorer`

Substrate deployment artefacts for the Triton Explorer Flutter SPA.
**Tailnet-only**, internal ingress on the DataZoo Hetzner substrate.

## Shape

- `Dockerfile` — multi-stage Flutter web build → `nginx-unprivileged`.
- `nginx.conf` — SPA fallback + `/healthz` + `/version.json`.
- `explorer.nomad.hcl` — `exposure = internal` (ADR-0010); Consul service
  with the `intprefix-explorer.<env>.int.data-zoo.de` tag so the shared
  `fabio-internal` ingress on `tailscale0:443` proxies to it over HTTPS.

## Access

After deploy, tailnet peers (operators on `tag:ops`) reach the SPA at:

```
https://explorer.nonprod.int.data-zoo.de
https://explorer.prod.int.data-zoo.de
```

Real HTTPS, trusted cert (the per-env wildcard `*.<env>.int.data-zoo.de`
issued by the substrate's ACME helper into Vault). No public Fabio route
ever exists for this job; deploy gate enforces.

## CORS

The SPA talks to Triton's HTTP trio cross-origin. Triton must allow-list
the explorer's tailnet origin so the browser preflight passes:

```
TRITON_CORS_ALLOWED_ORIGINS=https://explorer.nonprod.int.data-zoo.de
```

Add the prod origin to Triton's prod env when promoting. The CORS layer
itself lives in `crates/triton-adapters-http/src/cors.rs`; the wiring
is in `triton-bin` (see PR #16).

## Build + push (manual today)

The substrate's reusable `build-app-image.yml` workflow lives in
`hetzner-agent-substrate` and triggers off `apps/<app>/**` paths inside
that repo. The Explorer's source is in **this** repo (`triton/`), so
until we add a per-app caller workflow here, the image is built and
pushed by hand from a workstation with `gcloud` auth'd to
`hetzner-agent-backplane`:

```bash
cd /path/to/triton

VERSION=$(date +%Y-%m-%d)-$(git rev-parse --short HEAD)
IMG=europe-west3-docker.pkg.dev/hetzner-agent-backplane/substrate/dz-triton-explorer

docker build -f apps/explorer/deploy/Dockerfile -t $IMG:$VERSION .

gcloud auth configure-docker europe-west3-docker.pkg.dev --quiet
docker push $IMG:$VERSION

# Capture the immutable digest for the manifest pin
DIGEST=$(docker inspect --format='{{index .RepoDigests 0}}' $IMG:$VERSION)
echo "$DIGEST"
```

Local smoke before push:

```bash
docker run --rm -p 8080:8080 $IMG:$VERSION
curl -fsS http://localhost:8080/healthz   # → "ok"
curl -fsS http://localhost:8080/ | head -3 # → SPA HTML
```

## Release (operator-side)

Pin in `hetzner-agent-substrate/ops/base-jobs.manifest`:

```
dz-triton-explorer.nomad.hcl   -var image=europe-west3-docker.pkg.dev/hetzner-agent-backplane/substrate/dz-triton-explorer:<sha-tag>
```

Then, from the substrate repo:

```bash
/release-app dz-triton-explorer nonprod
```

`/release-app` resolves the latest known-good tag, opens (or no-ops on)
the manifest-bump PR, waits for merge, dispatches `/deploy-base nonprod`,
and probes `/healthz`. See substrate-platform reference 18.

## Why no per-app Tailscale sidecar

Pre-ADR-0010 internal apps each ran a Tailscale sidecar that joined the
tailnet under its own MagicDNS name. ADR-0010 retired that model in
favour of one shared internal ingress (`fabio-internal`) per cli node,
fronting every internal app with the per-env wildcard cert. New
internal apps just add the `intprefix-` Consul tag — no sidecar.
