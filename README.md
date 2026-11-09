# Cluster Observatory

A Rust/egui desktop workspace for inspecting and operating Kubernetes clusters.
It includes resource trends, node utilization, and service-to-pod-to-node
topology.

## Install on macOS

Download the macOS disk image from [GitHub Releases](https://github.com/anuchonmpt/local-lens/releases/latest),
open it, and drag **Cluster Observatory** into **Applications**. The same download
supports Apple Silicon and Intel. The app is ad hoc signed and has not been notarized.

The app checks for updates on startup. A green version number with an up arrow
in the footer means a newer release is available. Click the version number to
open the changelog, check again, or install an update and reopen the app. The app
must be in a location writable by your account to install updates.

To build the disk image locally, install both Rust targets with
`rustup target add aarch64-apple-darwin x86_64-apple-darwin` and run
`./package-macos.sh`. The image is created in `target/dist/`.

## Run

From this folder:

```sh
./build.sh
./target/debug/cluster-observatory
```

The build script uses your Rust installation when available and can otherwise
download a toolchain into `.toolchain/`. Run `./build.sh --release` to build
`target/release/cluster-observatory`.

On startup the app discovers contexts from kubectl's standard configuration
(`KUBECONFIG` or `~/.kube/config`) and other kubeconfig files directly in
`~/.kube/`. It automatically connects to the current context from kubectl's
standard configuration and reads its resources. Click any entry under
**Local Kubeconfigs** in the sidebar to switch clusters and expand its resource
menu. Select a namespace in the toolbar to view data for that cluster. You can
also explicitly choose **All namespaces**. Open **Connections** to rescan local files. To add a file from another directory,
enter its path and select **Read file**. Kubectl must be installed and
authenticated for the cluster. The sample workspace is available on demand
from **Connections**.

You can also connect on launch:

```sh
./target/debug/cluster-observatory --kubeconfig /path/to/config --context my-context
```

## Cluster workflows

- Use the navigator and namespace picker to browse workloads, configuration,
  networking, storage, access control, namespaces, and events. When RBAC
  denies an all-namespace list, the app tries the context's configured
  namespace (or `default`) and reports the restricted view.
- Select a resource to see details, related events, and configuration.
  **Edit YAML** loads the current object. Saving keeps its identity and
  resource version so Kubernetes can reject concurrent edits.
  **Apply YAML** in the header accepts new or updated manifests.
- For a Pod, select a container to read recent or previous logs, follow
  live output, filter lines, and copy the retained output. On macOS,
  **Open Shell in Terminal** opens a container shell in Terminal.app.
  macOS may prompt for permission to control Terminal.
- Open **Port Forward** on a Pod or Service to bind a port on `127.0.0.1`.
  Manage sessions under **Network → Port Forwarding**. Switching clusters
  or closing the app stops its managed sessions.
- Scale Deployments, StatefulSets, or ReplicaSets and delete resources
  after confirming the command. The command panel runs explicit,
  space-separated kubectl arguments after confirmation.
- Live utilization uses metrics-server and history accumulates during the
  session.
- **Visualize** provides a rotatable 3D view of Ingress routes, Service
  selectors, workload ownership, and Pod placement. Drag to rotate,
  Shift-drag to pan, and scroll to zoom. Click a resource to inspect it.
  **Live Traffic Mesh** queries a discovered Prometheus Service for
  Istio HTTP request rates every five seconds. A specific namespace limits
  the view to traffic whose source and destination are both in that
  namespace. The graph uses measured rates when the API is available.

## Verification

```sh
cargo test --locked
./target/debug/cluster-observatory --screenshot shots/now.png
```

Screenshot capture requires a graphical desktop session. An explicit
`--context` and optional `--kubeconfig` can capture a live workspace as well.

Tested against an isolated Kind cluster with an nginx Deployment and Service.
The smoke test covered resource loading, logs, container exec, HTTP through
port-forward, creating and editing a ConfigMap, conflict handling, scaling,
and deleting a resource. Metrics-server was absent on the test cluster.
