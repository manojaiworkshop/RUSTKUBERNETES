# 10 · kubectl

Almost all kubectl code lives in **`staging/src/k8s.io/kubectl`** (published as `k8s.io/kubectl`) and **`staging/src/k8s.io/cli-runtime`** (shared CLI plumbing). The `cmd/kubectl` and `pkg/kubectl` packages in the main tree are thin wrappers.

## 1. Code map

| Location | Responsibility |
|----------|----------------|
| `cmd/kubectl/kubectl.go` | `main()` → `cmd.NewDefaultKubectlCommand()` → `cli.RunNoErrOutput` |
| `staging/.../kubectl/pkg/cmd/cmd.go` | Builds the root cobra command, global flags, command groups and **plugin dispatch** (`NewDefaultKubectlCommandWithArgs`, `PluginHandler`) |
| `kubectl/pkg/cmd/<verb>/` | One package per sub-command |
| `kubectl/pkg/cmd/util/factory.go` | `Factory`: builds REST clients, dynamic clients, RESTMapper, resource `Builder`, validator from kubeconfig flags |
| `kubectl/pkg/cmd/util/helpers.go` | `CheckErr`, flag helpers, `GetDryRunStrategy`, `AddFieldManagerFlagVar` |
| `kubectl/pkg/describe` | `kubectl describe` describers per kind |
| `kubectl/pkg/polymorphichelpers` | Per-kind behaviour: `LogsForObject`, `AttachablePodForObject`, `HistoryViewer`, `Rollbacker`, `StatusViewer`, `ObjectPauser`… |
| `kubectl/pkg/drain` | Cordon and drain library (also used by other tools) |
| `kubectl/pkg/explain` | `kubectl explain` (OpenAPI v3 based templates) |
| `kubectl/pkg/kuberc` | **kuberc**: user preferences file (`~/.kube/kuberc`) for aliases and default flags |
| `kubectl/pkg/scale`, `apps`, `rawhttp`, `proxy`, `metricsutil`, `validation`, `scheme`, `util/*` | Supporting packages |
| `cli-runtime/pkg/genericclioptions` | `ConfigFlags` (`--kubeconfig`, `--context`, `-n`, `--server`, …), `PrintFlags`, `IOStreams` |
| `cli-runtime/pkg/resource` | **Resource Builder**: turns files, URLs, stdin, `type/name`, selectors and kustomize dirs into a stream of `resource.Info` |
| `cli-runtime/pkg/printers` | `-o json|yaml|name|jsonpath|go-template|custom-columns|wide` and table printers |
| `pkg/printers` (main tree) | **Server-side** table column definitions (`internalversion/printers.go`) that produce what `kubectl get` shows |

## 2. Anatomy of a command (the `Options` pattern)

Every command follows **Complete → Validate → Run**:

```go
func NewCmdFoo(f cmdutil.Factory, streams genericiooptions.IOStreams) *cobra.Command {
    o := NewFooOptions(streams)
    cmd := &cobra.Command{
        Use: "foo TYPE NAME",
        Run: func(cmd *cobra.Command, args []string) {
            cmdutil.CheckErr(o.Complete(f, cmd, args))  // resolve flags, build clients & builders
            cmdutil.CheckErr(o.Validate())              // validate user input
            cmdutil.CheckErr(o.Run())                   // do the work
        },
    }
    o.PrintFlags.AddFlags(cmd)
    return cmd
}
```

## 3. Resource Builder (`cli-runtime/pkg/resource/builder.go`)

```go
r := f.NewBuilder().
    Unstructured().                       // or WithScheme(...)
    NamespaceParam(ns).DefaultNamespace().
    FilenameParam(enforceNS, &filenameOptions).   // -f, -k
    LabelSelectorParam(selector).
    ResourceTypeOrNameArgs(true, args...).        // "pods/foo", "deploy,svc"
    ContinueOnError().Latest().Flatten().
    Do()
err := r.Visit(func(info *resource.Info, err error) error { ... })
```

`resource.Info` holds `Client`, `Mapping` (GVK↔GVR), `Namespace`, `Name`, `Object`. Visitors (`visitor.go`) compose file reading, decoding, kustomize, flattening of lists, and so on.

## 4. All commands

| Group | Command | Package | Notes |
|-------|---------|---------|-------|
| Basic (Beginner) | `create` (+ `deployment`, `service`, `secret`, `configmap`, `job`, `cronjob`, `role`, `rolebinding`, `clusterrole[binding]`, `ingress`, `namespace`, `pdb`, `priorityclass`, `quota`, `serviceaccount`, `token`) | `cmd/create` | `create token` calls the TokenRequest API |
| | `expose` | `cmd/expose` | Builds a Service from an object's selector and ports |
| | `run` | `cmd/run` | Creates a single Pod |
| | `set` (`image`, `env`, `resources`, `selector`, `serviceaccount`, `subject`) | `cmd/set` | Patches the pod template |
| Basic (Intermediate) | `explain` | `cmd/explain` | OpenAPI v3 schema docs |
| | `get` | `cmd/get` | Requests **server-side Table** (`application/json;as=Table`), `--watch`, `-o`, `--chunk-size`, `--show-managed-fields` |
| | `edit` | `cmd/edit` (`util/editor`) | Opens `$EDITOR`, then patch |
| | `delete` | `cmd/delete` | `--cascade=background|foreground|orphan`, `--grace-period`, `--wait`, `--interactive` |
| Deploy | `rollout` (`status`, `history`, `undo`, `pause`, `resume`, `restart`) | `cmd/rollout` | Uses polymorphic helpers |
| | `scale` | `cmd/scale` | `/scale` subresource, `--current-replicas` precondition |
| | `autoscale` | `cmd/autoscale` | Creates an HPA |
| Cluster management | `certificate approve|deny` | `cmd/certificates` | CSRs |
| | `cluster-info` (+ `dump`) | `cmd/clusterinfo` | |
| | `top node|pod` | `cmd/top` | `metrics.k8s.io` |
| | `cordon`, `uncordon`, `drain` | `cmd/drain` + `pkg/drain` | Drain evicts pods through the Eviction API (respecting PDBs) |
| | `taint` | `cmd/taint` | |
| Troubleshooting | `describe` | `cmd/describe` | Describers plus events |
| | `logs` | `cmd/logs` | `pods/log` subresource, `-f`, `--previous`, `--all-containers` |
| | `attach`, `exec` | `cmd/attach`, `cmd/exec` | SPDY or **WebSocket** streaming (`client-go/tools/remotecommand`) |
| | `port-forward` | `cmd/portforward` | `client-go/tools/portforward` |
| | `proxy` | `cmd/proxy` | Local proxy to the API server |
| | `cp` | `cmd/cp` | `tar` over exec |
| | `auth can-i|reconcile|whoami` | `cmd/auth` | `SelfSubjectAccessReview`, `SelfSubjectReview` |
| | `debug` | `cmd/debug` | Ephemeral containers, pod copies, node debugging pods. Profiles `general`, `baseline`, `restricted`, `netadmin`, `sysadmin` |
| | `events` | `cmd/events` | `events.k8s.io` listing with `--for`, `--types`, `--watch` |
| Advanced | `diff` | `cmd/diff` | Server-side **dry-run** then a local diff (`KUBECTL_EXTERNAL_DIFF`) |
| | `apply` (+ `set-last-applied`, `view-last-applied`, `edit-last-applied`) | `cmd/apply` | Client-side apply: three-way strategic merge using the `kubectl.kubernetes.io/last-applied-configuration` annotation. **`--server-side`** uses SSA with `--field-manager` and `--force-conflicts`. Supports `--prune` with ApplySets |
| | `patch` | `cmd/patch` | `--type=strategic|merge|json` |
| | `replace` | `cmd/replace` | PUT, `--force` deletes and recreates |
| | `wait` | `cmd/wait` | `--for=condition=…`, `--for=delete`, `--for=jsonpath=…`, `--for=create` |
| | `kustomize` | `cmd/kustomize` | Embedded `sigs.k8s.io/kustomize` |
| Settings | `label`, `annotate` | `cmd/label`, `cmd/annotate` | |
| | `completion` | `cmd/completion` | bash, zsh, fish, powershell |
| | `alpha` | `cmd/alpha.go` | Experimental commands |
| Other | `api-resources`, `api-versions` | `cmd/apiresources` | Discovery |
| | `config` (`view`, `use-context`, `set-context`, `set-cluster`, `set-credentials`, `get-contexts`, …) | `cmd/config` | kubeconfig editing (`client-go/tools/clientcmd`) |
| | `kuberc` (`set`, …) | `cmd/kuberc` | Edit the kuberc preferences file |
| | `plugin list` | `cmd/plugin` | |
| | `version` | `cmd/version` | Client/server versions and version-skew warnings |

## 5. Plugins & kuberc

- **Plugins**: any executable on `$PATH` named `kubectl-<name>` (dashes map to sub-commands, `kubectl-foo-bar` → `kubectl foo bar`). `PluginHandler.Lookup` / `Execute` in `cmd.go`. Example: `staging/src/k8s.io/sample-cli-plugin`.
- **kuberc** (`pkg/kuberc`): YAML preferences with `aliases` (for example `getn` → `get -o wide`) and `defaults` (for example always `--server-side` for apply). Controlled by `KUBERC` / `--kuberc`.

## 6. Example flow: `kubectl apply -f x.yaml --server-side`

1. `ApplyOptions.Complete` builds the Builder, DryRunStrategy and field manager.
2. `Run` visits each `Info`. For SSA it sends `PATCH` with `application/apply-patch+yaml`, `fieldManager=kubectl`, `force` = `--force-conflicts`.
3. Results are printed with `PrintFlags.ToPrinter("serverside-applied")`.
4. With `--prune`/ApplySet, objects that belong to the set but are not in the input are deleted.
