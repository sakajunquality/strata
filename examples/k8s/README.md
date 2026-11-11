# Strata on Kubernetes

Compute spread over several pods, with nothing to coordinate between them.
A Strata node keeps no state that matters: refs and pack lists are in the
metadata store, objects are in a bucket, and every node serves every
repository from those two. So the nodes are an ordinary `Deployment` behind
an ordinary `Service`: any request can go to any pod, a fetch's two requests
can land on two of them, and pods can be added, replaced and removed while
pushes are in flight.

```
git clients, the application
            |
        Service/strata
     /       |        \
  pod       pod       pod        Deployment/strata: stateless, scale freely
     \       |        /
  metadata store    bucket      PostgreSQL or Cloud Spanner; S3, GCS or Azure Blob
```

## What is here

| | |
|---|---|
| `base/` | The compute nodes: `Deployment`, `Service`, a headless `Service` for reaching pods one by one, and a `PodDisruptionBudget`. Expects a `Secret` named `strata-env`. |
| `local/` | `base/` plus a PostgreSQL and an S3-compatible store (LocalStack) to try it with, sized for a laptop. |
| `kind.sh`, `kind.yaml` | Runs `local/` in a [kind](https://kind.sigs.k8s.io) cluster and tests it. |
| `test.sh` | The test, run from a pod inside the cluster. |
| `../../Dockerfile` | The image. |

## Try it locally

```sh
./kind.sh up      # a cluster with two workers, the image built and loaded, everything deployed
./kind.sh test
./kind.sh down
```

The test drives the nodes with git from inside the cluster:

- a push through the `Service` is seen at once by every pod, and a push
  through one pod by the next;
- of two pushes to one branch at the same moment, through two pods, one lands;
- a 12 MiB file pushed through one pod is cloned whole from each of the others;
- pushes go on for a minute through the `Service` while a pod is deleted,
  all of them are replaced one by one and two more are added, and everything
  pushed is then on every pod, the new ones included.

It needs about 2 GiB of memory free in Docker. The first pods restart a few
times while PostgreSQL comes up: a node that cannot reach its metadata
store exits, and is started again.

## For real

Use `base/` from an overlay of your own that provides the image and the
`strata-env` secret:

| | |
|---|---|
| `STRATA_META` | `postgres://user:password@host/database` or `spanner://projects/<p>/instances/<i>/databases/<d>`. Shared by all pods; SQLite will not do. |
| `STRATA_OBJECT_STORE` | `s3://bucket/prefix`, `gs://bucket/prefix` or `az://container/prefix`. |
| `STRATA_ADMIN_TOKEN` | The secret for the management API. |
| `AWS_*`, `GOOGLE_*`, `AZURE_*` | The object store's settings, if the pods' own identity (IRSA, Workload Identity) does not already give access. |
| `STRATA_AUTHZ_URL`, `STRATA_PRE_RECEIVE_URL`, `STRATA_HOOK_SECRET` | The application in front, if there is one (see [`examples/hub`](../hub)). |

Things the manifests decide, and why:

- **Readiness is `/readyz`**, which asks the metadata store: a pod that
  cannot reach it is taken out of the `Service` rather than answering 500.
- **A pod told to stop keeps serving for five seconds** (`preStop`), then
  stops taking connections and finishes the requests it has, within
  `terminationGracePeriodSeconds`. Without the pause, a pod can be sent new
  connections after it has stopped listening; with it, the rolling restart
  in the test loses no request.
- **Memory is the caches.** `--block-cache-mb`, `--object-cache-mb` and
  `--index-cache-mb` add up to what a pod holds, plus what requests in
  flight need; set the limit above their sum.
- **Scratch space is an `emptyDir`**: a push is spooled there while it is
  checked, so it has to hold the largest pushes in flight. It is the only
  thing a pod writes locally.
- **Maintenance needs no separate workload.** Every pod runs compaction and
  the sweeps in the background, and they take leases in the metadata store
  so as not to do the same work twice.

What is left to you: TLS and a way in from outside (an `Ingress` or a
`LoadBalancer`; allow large request bodies and long responses, since
pushes and clones are both), autoscaling (CPU is a fair signal; nothing
needs warming up beyond caches), and the database and bucket themselves.
