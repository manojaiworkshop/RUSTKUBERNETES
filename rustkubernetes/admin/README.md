# rustkubeadmin — web console for rustkubernetes

One binary that serves a web console for **machines, clusters, users and RBAC**. Its design system is the same as
pgrustadmin's: GitHub-style tokens, light and dark themes, and live updates.

- **Sign in** as the super admin defined by `RUSTKUBEADMIN_USER` / `RUSTKUBEADMIN_PASSWORD`. Other users are created
  in the console.
- **Users & access**:
  - *Console permissions* control what a user may do in the console: `users.manage`, `machines.manage`,
    `clusters.manage`, `audit.view`.
  - *Cluster access* gives `view` / `edit` / `admin` on a whole cluster or on one namespace. It becomes real Kubernetes
    RoleBindings and ClusterRoleBindings. The console calls the cluster API *as the user* (`Impersonate-User:
    rustkubeadmin:<name>`), so Kubernetes RBAC enforces every action.
- **Machines**: add this host or any Linux server over SSH (using rustkubeadmin's own key, a pasted key, or a
  password). The **offline installer** copies a single bundle to the machine and installs it without root. The
  bundle holds every binary, the node service manager and the container images (pause, CoreDNS, kube-proxy), so the
  machine never downloads anything.
- **Clusters**: pick a master and workers and the console creates a secure cluster:
  - `kubeadm` PKI, with the CA key kept in the console;
  - services under systemd, `systemctl --user` or a nohup fallback;
  - kube-proxy and CoreDNS;
  - start, stop, retry and delete actions.

  You can also import an existing cluster from a kubeconfig.
- **Live views**: nodes, pods (logs, delete), deployments (deploy, scale, rolling restart), services, namespaces,
  events, Apply YAML (server-side apply), RBAC bindings, *your permissions*, control-plane service states and logs per
  machine, and the activity log with job logs. Everything updates through Server-Sent Events.

## Build and run

```bash
make admin                         # npm ci + next build (admin/web/out), then cargo build --release (embeds the UI)
export RUSTKUBEADMIN_USER=admin RUSTKUBEADMIN_PASSWORD='choose-a-long-one'
target/release/rustkubeadmin --listen 127.0.0.1:8090
# open http://127.0.0.1:8090
```

| Flag / env | Default | Meaning |
|------------|---------|---------|
| `RUSTKUBEADMIN_USER`, `RUSTKUBEADMIN_PASSWORD` | `admin` / *(required, ≥ 8 chars)* | super admin sign-in |
| `--listen` | `127.0.0.1:8090` | where to serve the console (put it behind HTTPS before exposing it) |
| `--data-dir`, `RUSTKUBEADMIN_DATA` | `~/.local/share/rustkubeadmin` | users, machines, cluster PKI, SSH key, bundle, job logs, audit log |
| `--binaries`, `RUSTKUBEADMIN_BINARIES` | directory of the executable | where the binaries for the offline bundle come from |
| `--dev-origin`, `RUSTKUBEADMIN_DEV_ORIGIN` | — | allow `npm run dev` (e.g. `http://localhost:3000`) |

Development with hot reload: run rustkubeadmin with `RUSTKUBEADMIN_DEV_ORIGIN=http://localhost:3000`, then
`cd admin/web && npm run dev`. `/api` is forwarded to `RUSTKUBEADMIN_API` (default `http://127.0.0.1:8090`).

## Layout

```
admin/
  src/
    main.rs        flags, env, startup (resumes watches, marks interrupted jobs)
    api.rs         REST + SSE API, permissions, CSRF checks, Kubernetes passthrough, embedded UI
    auth.rs        PBKDF2 passwords, sessions, principals, impersonation identity
    state.rs       state.json (users, machines, clusters) + audit.log
    machines.rs    add / install / test / uninstall
    bundle.rs      offline bundle (binaries + images via docker save)
    transport.rs   local shell or OpenSSH (ssh/scp/sshpass), scripts over stdin
    clusters.rs    provisioning, lifecycle, live snapshots, RBAC grant sync, apply, kubeconfigs
    jobs.rs        long-running jobs with live logs
    events.rs      SSE topic hub
  assets/
    install.sh     the installer inside the bundle
    rustkube-node  per-machine service manager (systemd / systemd --user / nohup)
  web/             Next.js + Tailwind + TypeScript, exported statically and embedded by build.rs
```

See [docs/admin-console.md](../docs/admin-console.md) for the design and the security model.
